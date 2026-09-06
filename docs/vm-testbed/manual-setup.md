# Manual Setup Guide: MicroVM Testbed

This guide walks you through setting up the microVM testbed **manually**, step by step, without using the automated scripts. This is useful if you want to:

- Understand every component in detail
- Customize the kernel, rootfs, or network configuration
- Debug issues with the automated scripts
- Run on a non-standard environment (e.g., custom Linux distro, ARM64)

The canonical, state-tracked workflow is `scripts/vm/provision-testbed.sh`,
`scripts/vm/deploy-test-application.sh`, and
`scripts/vm/destroy-testbed.sh`. The manual commands below explain the
components and support debugging, but they do not replace the scripts'
checksum verification, image-schema checks, exact process identities, or safe
teardown. Read current version and checksum pins from
`scripts/vm/kernel-testbed.env` rather than substituting newer downloads.

---

## Table of Contents

1. [Overview](#overview)
2. [Prerequisites](#prerequisites)
3. [Step 1: Verify KVM Support](#step-1-verify-kvm-support)
4. [Step 2: Install Firecracker](#step-2-install-firecracker)
5. [Step 3: Build the Linux Kernel](#step-3-build-the-linux-kernel)
6. [Step 4: Create the NATS Rootfs](#step-4-create-the-nats-rootfs)
7. [Step 5: Create the wasm-node Rootfs](#step-5-create-the-wasm-node-rootfs)
8. [Step 6: Set Up Host Networking](#step-6-set-up-host-networking)
9. [Step 7: Start a Single MicroVM](#step-7-start-a-single-microvm)
10. [Step 8: Start a Multi-Node Cluster](#step-8-start-a-multi-node-cluster)
11. [Step 9: Run Tests](#step-9-run-tests)
12. [Step 10: Manual Chaos Testing](#step-10-manual-chaos-testing)
13. [Troubleshooting](#troubleshooting)
14. [Reference: Firecracker API](#reference-firecracker-api)

---

## Overview

The testbed consists of three artifacts:

| Artifact | Purpose | Size |
|----------|---------|------|
| `vmlinux-6.18` | Checksum-pinned Linux 6.18.48 test kernel with eBPF/BTF support | ~15 MB |
| `nats-rootfs.ext4` | Alpine Linux + NATS Server | ~50 MB |
| `wasm-node-rootfs.ext4` | Alpine Linux + wasm-node binary | ~100 MB |

The core topology uses those artifacts. The aggregate image builder also
creates optional PostgreSQL and Vault service fixtures for application and
external seal-root integration tests. They are not platform components and the
initialized Vault image contains sensitive local fixture state. Follow
[Local service microVMs](./service-microvms.md) rather than manually extracting
credentials or starting those services with ad-hoc Firecracker commands.

And a network setup:

| Component | Purpose |
|-----------|---------|
| `br-wasm` | Linux bridge connecting all VMs |
| `tap-node-*` | TAP devices, one per VM |
| `iptables MASQUERADE` | NAT for outbound internet access |

---

## Prerequisites

### Hardware

- **x86_64** or **aarch64** CPU with virtualization support
  - Intel: VT-x (check with `grep vmx /proc/cpuinfo`)
  - AMD: AMD-V (check with `grep svm /proc/cpuinfo`)
- At least **4 GB RAM** (for host + 3 microVMs)
- **10 GB disk space** for kernel source and images

### Software

```bash
# Debian/Ubuntu
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    bc \
    bison \
    flex \
    libssl-dev \
    libelf-dev \
    curl \
    e2fsprogs \
    iproute2 \
    iptables \
    jq \
    openssl \
    python3 \
    unzip \
    util-linux \
    qemu-utils

# Fedora/RHEL
sudo dnf install -y \
    gcc make bc bison flex \
    openssl-devel elfutils-libelf-devel \
    curl iproute iptables qemu-img

# Arch
sudo pacman -S \
    base-devel bc bison flex \
    openssl libelf \
    curl iproute2 iptables qemu-utils
```

### Rust Toolchain

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Add WASI target
rustup target add wasm32-wasip2

# Verify
rustc --version  # Must match the repository's rust-toolchain.toml
```

---

## Step 1: Verify KVM Support

KVM (Kernel-based Virtual Machine) is required for Firecracker. It uses hardware virtualization extensions.

### Check CPU Support

```bash
# Intel
grep -c vmx /proc/cpuinfo

# AMD
grep -c svm /proc/cpuinfo
```

If the output is `0`, your CPU does not support virtualization, or it is disabled in BIOS.

### Check Kernel Modules

```bash
# Check if KVM modules are loaded
lsmod | grep kvm

# Expected output:
# kvm_intel             393216  0
# kvm                  1093632  1 kvm_intel

# If not loaded, load them:
sudo modprobe kvm
sudo modprobe kvm_intel   # Intel
# OR
sudo modprobe kvm_amd     # AMD

# Make persistent
echo "kvm" | sudo tee /etc/modules-load.d/kvm.conf
echo "kvm_intel" | sudo tee -a /etc/modules-load.d/kvm.conf
```

### Check /dev/kvm

```bash
ls -la /dev/kvm

# Expected: crw-rw----+ 1 root kvm 10, 232 Jan 1 00:00 /dev/kvm

# If it doesn't exist, KVM is not available.
# If permissions are wrong:
sudo chmod 666 /dev/kvm

# Add yourself to kvm group (logout/login required)
sudo usermod -aG kvm $USER
newgrp kvm
```

---

## Step 2: Install Firecracker

Firecracker is a microVMM (micro Virtual Machine Monitor) written in Rust by AWS. It boots VMs in under 125ms.

### Download Pre-built Binary

```bash
# Set version
FIRECRACKER_VERSION="v1.16.1"
ARCH="$(uname -m)"

# Create directory
sudo mkdir -p /usr/local/bin

# Download
curl -fsSL \
    "https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-${ARCH}.tgz" \
    -o /tmp/firecracker.tgz

# Extract
cd /tmp
tar xzf firecracker.tgz

# Find and install binary
FIRECRACKER_BIN="$(find . -name 'firecracker' -type f | head -n 1)"
sudo cp "$FIRECRACKER_BIN" /usr/local/bin/firecracker
sudo chmod 755 /usr/local/bin/firecracker

# Verify
firecracker --version
# Expected: Firecracker v1.16.1
```

### Build from Source (Optional)

If you need a custom build or the pre-built binary doesn't work:

```bash
# Clone
git clone https://github.com/firecracker-microvm/firecracker.git
cd firecracker

# Build (requires Rust)
tools/devtool build

# The binary will be at:
# build/cargo_target/$(uname -m)-unknown-linux-musl/release/firecracker
```

---

## Step 3: Build the Linux Kernel

Firecracker uses a **vmlinux** ELF binary (not a bzImage). We need to build a minimal kernel with eBPF and BTF support.

### Download Source

```bash
KERNEL_VERSION="6.18.48"
cd /tmp

# Download
if [[ ! -f "linux-${KERNEL_VERSION}.tar.xz" ]]; then
    curl -fsSL \
        "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" \
        -o "linux-${KERNEL_VERSION}.tar.xz"
fi

# Extract
tar xf "linux-${KERNEL_VERSION}.tar.xz"
cd "linux-${KERNEL_VERSION}"
```

### Configure the Kernel

Firecracker has specific requirements. We'll start with `tinyconfig` and add what we need:

```bash
# Start minimal
make tinyconfig

# Now add essential options using scripts/config
# Or manually edit .config
```

Create a config fragment file:

```bash
cat > /tmp/firecracker-config.fragment << 'EOF'
# 64-bit
CONFIG_64BIT=y

# Block devices (for virtio-blk)
CONFIG_BLOCK=y
CONFIG_BLK_DEV=y
CONFIG_VIRTIO_BLK=y

# Network (for virtio-net)
CONFIG_NET=y
CONFIG_INET=y
CONFIG_NETDEVICES=y
CONFIG_VIRTIO_NET=y

# Console (for serial output)
CONFIG_TTY=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_VIRTIO_CONSOLE=y

# Filesystems
CONFIG_EXT4_FS=y
CONFIG_TMPFS=y
CONFIG_PROC_FS=y
CONFIG_SYSFS=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y

# eBPF support (CRITICAL for wasm-node)
CONFIG_BPF=y
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_JIT=y
CONFIG_BPF_JIT_ALWAYS_ON=y
CONFIG_HAVE_EBPF_JIT=y
CONFIG_CGROUP_BPF=y
CONFIG_BPF_EVENTS=y

# BTF support (CRITICAL for modern eBPF)
CONFIG_DEBUG_INFO=y
CONFIG_DEBUG_INFO_BTF=y
CONFIG_DEBUG_INFO_BTF_MODULES=y

# cgroups v2 (for resource isolation)
CONFIG_CGROUPS=y
CONFIG_CGROUP_SCHED=y
CONFIG_CGROUP_PIDS=y
CONFIG_MEMCG=y

# Namespaces
CONFIG_NAMESPACES=y
CONFIG_UTS_NS=y
CONFIG_IPC_NS=y
CONFIG_USER_NS=y
CONFIG_PID_NS=y
CONFIG_NET_NS=y

# seccomp (for security)
CONFIG_SECCOMP=y
CONFIG_SECCOMP_FILTER=y

# PCI (for virtio devices)
CONFIG_PCI=y
CONFIG_VIRTIO_PCI=y

# Enable virtio MMIO (alternative to PCI)
CONFIG_VIRTIO_MMIO=y

# Power management
CONFIG_ACPI=y

# Disable debug to reduce size
CONFIG_DEBUG_KERNEL=n
CONFIG_DEBUG_FS=n
EOF

# Merge fragment into .config
# Method 1: Manual merge
while IFS= read -r line; do
    [[ -z "$line" || "$line" =~ ^# ]] && continue
    key="${line%%=*}"
    value="${line#*=}"
    if grep -q "^$key=" .config; then
        sed -i "s/^$key=.*/$line/" .config
    else
        echo "$line" >> .config
    fi
done < /tmp/firecracker-config.fragment

# Method 2: Use merge_config.sh (if available)
# ./scripts/kconfig/merge_config.sh .config /tmp/firecracker-config.fragment

# Update config with defaults for new options
make olddefconfig
```

### Build

```bash
# Build vmlinux (not bzImage!)
make -j$(nproc) vmlinux

# This produces: vmlinux (ELF binary)
# Size should be ~10-20 MB

# Strip debug symbols to reduce size
strip vmlinux

# Copy to assets directory
mkdir -p ~/wasm-cloud-platform/assets
cp vmlinux ~/wasm-cloud-platform/assets/vmlinux-6.18

echo "Kernel built: $(du -h vmlinux | cut -f1)"
```

### Verify BTF Support

```bash
# Check if BTF section exists
readelf -S vmlinux | grep BTF

# Expected: [xx] .BTF PROGBITS ...

# Alternative: use pahole (from dwarves package)
# pahole --btf_encode_detached vmlinux.btf vmlinux
```

---

## Step 4: Create the NATS Rootfs

The NATS VM runs a minimal Alpine Linux with NATS Server and JetStream.

### Create Directory Structure

```bash
WORK_DIR="$(mktemp -d)"
ROOTFS_DIR="$WORK_DIR/rootfs"
mkdir -p "$ROOTFS_DIR"

cd "$WORK_DIR"
```

### Download Alpine Base

```bash
ALPINE_VERSION="3.19"
curl -fsSL \
    "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/x86_64/alpine-minirootfs-${ALPINE_VERSION}.0-x86_64.tar.gz" \
    -o alpine-rootfs.tar.gz

tar xzf alpine-rootfs.tar.gz -C "$ROOTFS_DIR"
```

### Configure Package Repositories

```bash
mkdir -p "$ROOTFS_DIR/etc/apk"
cat > "$ROOTFS_DIR/etc/apk/repositories" << EOF
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community
EOF
```

### Install Packages

```bash
# Install packages into the rootfs
apk add --root "$ROOTFS_DIR" --initdb --no-cache \
    alpine-base \
    openrc \
    iproute2 \
    curl \
    ca-certificates
```

### Install NATS Server

```bash
NATS_VERSION="2.10.14"
curl -fsSL \
    "https://github.com/nats-io/nats-server/releases/download/v${NATS_VERSION}/nats-server-v${NATS_VERSION}-linux-amd64.tar.gz" \
    -o nats-server.tar.gz

tar xzf nats-server.tar.gz
mkdir -p "$ROOTFS_DIR/usr/local/bin"
cp "nats-server-v${NATS_VERSION}-linux-amd64/nats-server" "$ROOTFS_DIR/usr/local/bin/"
chmod +x "$ROOTFS_DIR/usr/local/bin/nats-server"
```

### Create NATS Configuration

```bash
mkdir -p "$ROOTFS_DIR/etc/nats"
cat > "$ROOTFS_DIR/etc/nats/nats-server.conf" << 'EOF'
port: 4222
http_port: 8222

jetstream {
    store_dir: "/var/lib/nats"
    max_memory_store: 1GB
    max_file_store: 10GB
}

allow_non_tls: true

debug: false
trace: false
logtime: true
logfile: "/var/log/nats-server.log"
EOF
```

### Create Init Script

```bash
cat > "$ROOTFS_DIR/etc/init.d/nats-server" << 'EOF'
#!/sbin/openrc-run

description="NATS Server"

command="/usr/local/bin/nats-server"
command_args="-c /etc/nats/nats-server.conf"
command_background=true
pidfile="/run/nats-server.pid"

depend() {
    need net
}

start_pre() {
    checkpath -d -m 0755 /var/lib/nats
    checkpath -d -m 0755 /var/log
}
EOF
chmod +x "$ROOTFS_DIR/etc/init.d/nats-server"

# Enable at boot
mkdir -p "$ROOTFS_DIR/etc/runlevels/default"
ln -sf /etc/init.d/nats-server "$ROOTFS_DIR/etc/runlevels/default/nats-server"
```

### Configure Boot

```bash
# /etc/inittab for serial console
cat > "$ROOTFS_DIR/etc/inittab" << 'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100

::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

# Hostname
echo "nats-vm" > "$ROOTFS_DIR/etc/hostname"

# Network (static IP)
cat > "$ROOTFS_DIR/etc/network/interfaces" << 'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.20.0.10
    netmask 255.255.255.0
    gateway 172.20.0.1
EOF
```

### Create ext4 Image

```bash
# Create empty image
dd if=/dev/zero of=nats-rootfs.ext4 bs=1M count=256
mkfs.ext4 -F nats-rootfs.ext4

# Mount and copy
mkdir -p mnt
sudo mount -o loop nats-rootfs.ext4 mnt
sudo cp -a "$ROOTFS_DIR"/* mnt/
sudo umount mnt

# Move to assets
mv nats-rootfs.ext4 ~/wasm-cloud-platform/assets/

echo "NATS rootfs created: $(du -h ~/wasm-cloud-platform/assets/nats-rootfs.ext4 | cut -f1)"
```

---

## Step 5: Create the wasm-node Rootfs

This rootfs contains the wasm-node binary, wasm-ctl, and all dependencies.

### Build wasm-node Binary

```bash
cd ~/wasm-cloud-platform
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

# Build release binaries
cargo build --release --bin wasm-node
cargo build --release --bin wasm-ctl

# Verify
ls -la "$CARGO_TARGET_DIR/release/wasm-node" "$CARGO_TARGET_DIR/release/wasm-ctl"
```

### Create Rootfs Directory

```bash
WORK_DIR="$(mktemp -d)"
ROOTFS_DIR="$WORK_DIR/rootfs"
mkdir -p "$ROOTFS_DIR"

cd "$WORK_DIR"
```

### Download Alpine Base

```bash
ALPINE_VERSION="3.19"
curl -fsSL \
    "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/x86_64/alpine-minirootfs-${ALPINE_VERSION}.0-x86_64.tar.gz" \
    -o alpine-rootfs.tar.gz

tar xzf alpine-rootfs.tar.gz -C "$ROOTFS_DIR"
```

### Install Packages

```bash
mkdir -p "$ROOTFS_DIR/etc/apk"
cat > "$ROOTFS_DIR/etc/apk/repositories" << EOF
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community
EOF

apk add --root "$ROOTFS_DIR" --initdb --no-cache \
    alpine-base \
    openrc \
    iproute2 \
    iptables \
    curl \
    ca-certificates \
    libelf \
    zlib \
    libgcc
```

### Install Binaries

```bash
mkdir -p "$ROOTFS_DIR/usr/local/bin"
cp "$CARGO_TARGET_DIR/release/wasm-node" "$ROOTFS_DIR/usr/local/bin/"
cp "$CARGO_TARGET_DIR/release/wasm-ctl" "$ROOTFS_DIR/usr/local/bin/"
chmod +x "$ROOTFS_DIR/usr/local/bin/"{wasm-node,wasm-ctl}
```

### Create Config

```bash
mkdir -p "$ROOTFS_DIR/etc/wasm-node"
cat > "$ROOTFS_DIR/etc/wasm-node/config.toml" << 'EOF'
[node]
node_id = "vm-node"

[storage]
db_path = "/var/lib/wasm-node/state.redb"

[nats]
url = "nats://172.20.0.10:4222"

[proxy]
http_port = 8080
https_port = 0

[admin]
port = 9090

[artifact]
port = 9091

[runtime]
port_start = 10000
port_end = 19999

[logging]
level = "info"
format = "json"

[dns]
stub_enabled = true
stub_port = 15353

[health]
check_interval_secs = 2
EOF
```

### Create Init Script

```bash
cat > "$ROOTFS_DIR/etc/init.d/wasm-node" << 'EOF'
#!/sbin/openrc-run

description="Wasm Cloud Platform Node"

command="/usr/local/bin/wasm-node"
command_args="--config /etc/wasm-node/config.toml"
command_background=true
pidfile="/run/wasm-node.pid"

depend() {
    need net
}

start_pre() {
    checkpath -d -m 0755 /var/lib/wasm-node
    checkpath -d -m 0755 /var/log/wasm-node
    checkpath -d -m 0755 /run/wasm-node
}
EOF
chmod +x "$ROOTFS_DIR/etc/init.d/wasm-node"

mkdir -p "$ROOTFS_DIR/etc/runlevels/default"
ln -sf /etc/init.d/wasm-node "$ROOTFS_DIR/etc/runlevels/default/wasm-node"
```

### Configure Boot

```bash
cat > "$ROOTFS_DIR/etc/inittab" << 'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100

::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

echo "wasm-node-vm" > "$ROOTFS_DIR/etc/hostname"

cat > "$ROOTFS_DIR/etc/network/interfaces" << 'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.20.0.2
    netmask 255.255.255.0
    gateway 172.20.0.1
EOF
```

### Create ext4 Image

```bash
dd if=/dev/zero of=wasm-node-rootfs.ext4 bs=1M count=512
mkfs.ext4 -F wasm-node-rootfs.ext4

mkdir -p mnt
sudo mount -o loop wasm-node-rootfs.ext4 mnt
sudo cp -a "$ROOTFS_DIR"/* mnt/
sudo umount mnt

mv wasm-node-rootfs.ext4 ~/wasm-cloud-platform/assets/

echo "wasm-node rootfs created: $(du -h ~/wasm-cloud-platform/assets/wasm-node-rootfs.ext4 | cut -f1)"
```

---

## Step 6: Set Up Host Networking

Each microVM needs a TAP interface connected to a bridge.

### Create the Bridge

```bash
# Create bridge
sudo ip link add br-wasm type bridge

# Assign IP (gateway for VMs)
sudo ip addr add 172.20.0.1/24 dev br-wasm

# Bring up
sudo ip link set br-wasm up

# Enable IP forwarding
sudo sysctl -w net.ipv4.ip_forward=1

# Make persistent
echo "net.ipv4.ip_forward=1" | sudo tee /etc/sysctl.d/99-ip-forward.conf
```

### Create TAP Devices

```bash
# For each VM you plan to run:

# Node 1
sudo ip tuntap add tap-node1 mode tap
sudo ip link set tap-node1 up
sudo ip link set tap-node1 master br-wasm

# Node 2
sudo ip tuntap add tap-node2 mode tap
sudo ip link set tap-node2 up
sudo ip link set tap-node2 master br-wasm

# NATS
sudo ip tuntap add tap-nats mode tap
sudo ip link set tap-nats up
sudo ip link set tap-nats master br-wasm
```

### Enable NAT (for outbound internet)

```bash
# Allow VMs to reach the internet through the host
sudo iptables -t nat -A POSTROUTING -s 172.20.0.0/24 ! -o br-wasm -j MASQUERADE

# Allow forwarding
sudo iptables -A FORWARD -i br-wasm -j ACCEPT
sudo iptables -A FORWARD -o br-wasm -j ACCEPT
```

### Verify Setup

```bash
# Show bridge
ip link show br-wasm

# Show TAP devices
ip link show tap-node1
ip link show tap-nats

# Show forwarding rules
sudo iptables -t nat -L -n -v
```

---

## Step 7: Start a Single MicroVM

We'll start the NATS VM manually using the Firecracker API.

### Start Firecracker Process

```bash
# Create a directory for this VM's runtime files
mkdir -p /tmp/fc-nats

# Start Firecracker
firecracker \
    --api-sock /tmp/fc-nats/firecracker.sock \
    --id nats-1 \
    > /tmp/fc-nats/firecracker.log 2>&1 &

FC_PID=$!
echo "Firecracker PID: $FC_PID"
```

### Configure via API

Firecracker exposes a REST API over a Unix domain socket. We'll use `curl`:

```bash
SOCKET="/tmp/fc-nats/firecracker.sock"

# 1. Machine configuration
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/machine-config" \
    -H "Content-Type: application/json" \
    -d '{
        "vcpu_count": 1,
        "mem_size_mib": 256,
        "smt": false,
        "track_dirty_pages": false
    }'

# 2. Boot source (kernel)
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/boot-source" \
    -H "Content-Type: application/json" \
    -d '{
        "kernel_image_path": "'"$HOME"'/wasm-cloud-platform/assets/vmlinux-6.18",
        "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
    }'

# 3. Root drive
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/drives/rootfs" \
    -H "Content-Type: application/json" \
    -d '{
        "drive_id": "rootfs",
        "path_on_host": "'"$HOME"'/wasm-cloud-platform/assets/nats-rootfs.ext4",
        "is_root_device": true,
        "is_read_only": false
    }'

# 4. Network interface
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/network-interfaces/eth0" \
    -H "Content-Type: application/json" \
    -d '{
        "iface_id": "eth0",
        "guest_mac": "AA:FC:00:00:00:01",
        "host_dev_name": "tap-nats"
    }'

# 5. Start the VM!
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/actions" \
    -H "Content-Type: application/json" \
    -d '{
        "action_type": "InstanceStart"
    }'
```

### Verify NATS is Running

```bash
# Wait a few seconds for boot
sleep 5

# Check NATS is listening
curl http://172.20.0.10:8222/varz

# Or connect with nats CLI
nats --server nats://172.20.0.10:4222 server info
```

### Stop the VM

```bash
# Graceful shutdown (sends Ctrl-Alt-Del)
curl --unix-socket "$SOCKET" -X PUT \
    "http://localhost/actions" \
    -d '{"action_type": "SendCtrlAltDel"}'

# Or kill the process
kill $FC_PID
```

---

## Step 8: Start a Multi-Node Cluster

Now start multiple wasm-node VMs that connect to the NATS VM.

### Start NATS VM (if not running)

Follow Step 7 above.

### Start Node 1

```bash
mkdir -p /tmp/fc-node1

firecracker \
    --api-sock /tmp/fc-node1/firecracker.sock \
    --id node-1 \
    > /tmp/fc-node1/firecracker.log 2>&1 &

FC_NODE1=$!
SOCKET="/tmp/fc-node1/firecracker.sock"

# Machine config
curl --unix-socket "$SOCKET" -X PUT "http://localhost/machine-config" \
    -d '{"vcpu_count": 2, "mem_size_mib": 512}'

# Boot source
curl --unix-socket "$SOCKET" -X PUT "http://localhost/boot-source" \
    -d '{
        "kernel_image_path": "'"$HOME"'/wasm-cloud-platform/assets/vmlinux-6.18",
        "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
    }'

# Root drive
curl --unix-socket "$SOCKET" -X PUT "http://localhost/drives/rootfs" \
    -d '{
        "drive_id": "rootfs",
        "path_on_host": "'"$HOME"'/wasm-cloud-platform/assets/wasm-node-rootfs.ext4",
        "is_root_device": true,
        "is_read_only": false
    }'

# Network
curl --unix-socket "$SOCKET" -X PUT "http://localhost/network-interfaces/eth0" \
    -d '{
        "iface_id": "eth0",
        "guest_mac": "AA:FC:00:00:00:02",
        "host_dev_name": "tap-node1"
    }'

# Start
curl --unix-socket "$SOCKET" -X PUT "http://localhost/actions" \
    -d '{"action_type": "InstanceStart"}'
```

### Start Node 2

Repeat with different MAC, TAP, and API socket:

```bash
mkdir -p /tmp/fc-node2

firecracker \
    --api-sock /tmp/fc-node2/firecracker.sock \
    --id node-2 \
    > /tmp/fc-node2/firecracker.log 2>&1 &

FC_NODE2=$!
SOCKET="/tmp/fc-node2/firecracker.sock"

# ... same API calls as Node 1 but with:
# guest_mac: "AA:FC:00:00:00:03"
# host_dev_name: "tap-node2"
```

### Verify Cluster

```bash
# Wait for nodes to boot
sleep 10

# Check node 1 health
curl http://172.20.0.2:9090/healthz

# Check node 2 health
curl http://172.20.0.3:9090/healthz

# Check NATS has both nodes connected
nats --server nats://172.20.0.10:4222 server info
```

---

## Step 9: Run Tests

### Using the Rust Test Suite

```bash
cd ~/wasm-cloud-platform

# Set environment variables
export FIRECRACKER_PATH="/usr/local/bin/firecracker"
export VM_KERNEL_PATH="./assets/vmlinux-6.18"
export VM_NATS_ROOTFS="./assets/nats-rootfs.ext4"
export VM_NODE_ROOTFS="./assets/wasm-node-rootfs.ext4"

# Run single node deploy test
sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture

# Run chaos tests
sudo cargo test -p vm-testbed --test vm_chaos -- --nocapture --test-threads=1
```

### Manual Testing with curl

```bash
# Deploy an app (via NATS)
nats --server nats://172.20.0.10:4222 pub deploy.apps '{
    "app_id": "hello:v1",
    "artifact_url": "http://172.20.0.2:9091/artifacts/hello.wasm",
    "config": {"fuel_limit": 100000000}
}'

# Add route
nats --server nats://172.20.0.10:4222 pub routes.add '{
    "host": "hello.local",
    "app_id": "hello:v1"
}'

# Test via proxy
curl -H "Host: hello.local" http://172.20.0.2:8080/
```

---

## Step 10: Manual Chaos Testing

### L2: Kill and Restart a Node

```bash
# Kill the VM (simulates power loss)
kill -9 $FC_NODE1

# Verify it's dead
! curl http://172.20.0.2:9090/healthz && echo "Node is dead"

# Restart by re-running Step 8 for Node 1
# The node should restore state from /var/lib/wasm-node/state.redb

# Verify recovery
curl http://172.20.0.2:9090/healthz
```

### L5: Network Partition

```bash
# Drop packets between Node 1 and NATS
sudo iptables -A FORWARD -s 172.20.0.2 -d 172.20.0.10 -j DROP
sudo iptables -A FORWARD -s 172.20.0.10 -d 172.20.0.2 -j DROP

# Verify node enters degraded mode but still serves
curl -H "Host: hello.local" http://172.20.0.2:8080/

# Heal partition
sudo iptables -D FORWARD -s 172.20.0.2 -d 172.20.0.10 -j DROP
sudo iptables -D FORWARD -s 172.20.0.10 -d 172.20.0.2 -j DROP

# Verify reconnection
curl http://172.20.0.2:9090/healthz
```

### L3: Disk Corruption

```bash
# Corrupt the redb file on the node rootfs
# (This requires the node to be stopped)
kill $FC_NODE1

# Mount the rootfs
mkdir -p /tmp/corrupt-mnt
sudo mount -o loop ~/wasm-cloud-platform/assets/wasm-node-rootfs.ext4 /tmp/corrupt-mnt

# Corrupt a page in the redb file
sudo dd if=/dev/urandom of=/tmp/corrupt-mnt/var/lib/wasm-node/state.redb bs=4096 count=1 conv=notrunc

# Unmount
sudo umount /tmp/corrupt-mnt

# Restart node - it should detect corruption and rebuild from NATS
curl http://172.20.0.2:9090/healthz
```

---

## Troubleshooting

### Firecracker fails to start

```bash
# Check KVM
ls -la /dev/kvm
# Should be: crw-rw----+ 1 root kvm 10, 232 ...

# Check permissions
sudo chmod 666 /dev/kvm
sudo usermod -aG kvm $USER
newgrp kvm

# Check logs
cat /tmp/fc-*/firecracker.log
```

### VM boots but network doesn't work

```bash
# Check bridge
ip link show br-wasm
ip addr show br-wasm

# Check TAP
ip link show tap-node1

# Check forwarding
sudo sysctl net.ipv4.ip_forward

# Check iptables
sudo iptables -t nat -L -n -v
sudo iptables -L FORWARD -n -v
```

### NATS not reachable from VM

```bash
# Check NATS is running inside VM
# (You need serial console access for this)

# Alternative: check from host
curl http://172.20.0.10:8222/varz

# Check NATS logs inside VM
# Mount rootfs and check /var/log/nats-server.log
```

### eBPF programs fail to load

```bash
# Check kernel has BTF
readelf -S vmlinux | grep BTF

# Check kernel version inside VM
# (Requires serial console or ssh)

# Check capabilities
# Inside VM: capsh --print | grep bpf

# Check kernel config
zcat /proc/config.gz | grep CONFIG_BPF
```

### Out of memory

```bash
# Check host memory
free -h

# Reduce VM memory
# In machine-config: "mem_size_mib": 256 (instead of 512)

# Or reduce number of VMs
```

---

## Reference: Firecracker API

### Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/` | Check API is running |
| PUT | `/machine-config` | Set vCPUs and memory |
| PUT | `/boot-source` | Set kernel image |
| PUT | `/drives/{id}` | Attach block device |
| PUT | `/network-interfaces/{id}` | Attach network interface |
| PUT | `/actions` | Start, stop, pause VM |
| PUT | `/balloon` | Configure memory balloon |
| PUT | `/logger` | Configure logging |
| PUT | `/metrics` | Configure metrics |
| PUT | `/mmds` | Set metadata |

### Example: Full VM Lifecycle

```bash
SOCKET="/tmp/fc-test.sock"

# 1. Start firecracker
firecracker --api-sock "$SOCKET" &

# 2. Configure
curl --unix-socket "$SOCKET" -X PUT http://localhost/machine-config \
    -d '{"vcpu_count": 2, "mem_size_mib": 512}'

curl --unix-socket "$SOCKET" -X PUT http://localhost/boot-source \
    -d '{"kernel_image_path": "/path/to/vmlinux", "boot_args": "console=ttyS0"}'

curl --unix-socket "$SOCKET" -X PUT http://localhost/drives/rootfs \
    -d '{"drive_id": "rootfs", "path_on_host": "/path/to/rootfs.ext4", "is_root_device": true}'

curl --unix-socket "$SOCKET" -X PUT http://localhost/network-interfaces/eth0 \
    -d '{"iface_id": "eth0", "guest_mac": "AA:FC:00:00:00:01", "host_dev_name": "tap0"}'

# 3. Start
curl --unix-socket "$SOCKET" -X PUT http://localhost/actions \
    -d '{"action_type": "InstanceStart"}'

# 4. Stop
curl --unix-socket "$SOCKET" -X PUT http://localhost/actions \
    -d '{"action_type": "SendCtrlAltDel"}'
```

---

## Next Steps

- Read the [crate README](../../crates/vm-testbed/README.md) for the Rust API
- Explore the [chaos tests](../../crates/vm-testbed/tests/vm_chaos.rs) for automated failure injection
- Integrate with your CI/CD pipeline using the [CLI tool](../../crates/vm-testbed/src/cli.rs)
