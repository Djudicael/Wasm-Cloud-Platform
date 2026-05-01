#!/usr/bin/env bash
# Build a root filesystem for the wasm-node microVM.
#
# This creates a minimal Alpine Linux rootfs with:
# - wasm-node binary
# - wasm-ctl binary
# - init system (openrc)
# - essential tools (ip, iptables, curl)
# - eBPF dependencies (libelf for BTF)
#
# Usage:
#   ./scripts/vm/build-node-rootfs.sh
#
# Output:
#   ./assets/wasm-node-rootfs.ext4

set -euo pipefail

OUTPUT_DIR="${OUTPUT_DIR:-./assets}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-512}"
ALPINE_VERSION="${ALPINE_VERSION:-3.19}"

echo "=== Building wasm-node rootfs ==="

# Ensure wasm-node is built
echo "Building wasm-node binary..."
cargo build --release --bin wasm-node
cargo build --release --bin wasm-ctl

# Create working directory
WORK_DIR="$(mktemp -d)"
trap "rm -rf $WORK_DIR" EXIT

ROOTFS_DIR="$WORK_DIR/rootfs"
mkdir -p "$ROOTFS_DIR"

# Download Alpine mini rootfs
echo "Downloading Alpine $ALPINE_VERSION mini rootfs..."
curl -fsSL \
    "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/$(uname -m)/alpine-minirootfs-${ALPINE_VERSION}.0-$(uname -m).tar.gz" \
    -o "$WORK_DIR/alpine-rootfs.tar.gz"

tar xzf "$WORK_DIR/alpine-rootfs.tar.gz" -C "$ROOTFS_DIR"

# Install packages inside the rootfs
echo "Installing packages..."
mkdir -p "$ROOTFS_DIR/etc/apk"
cat > "$ROOTFS_DIR/etc/apk/repositories" << EOF
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community
EOF

# Use apk to install packages in the rootfs
apk add --root "$ROOTFS_DIR" --initdb --no-cache \
    alpine-base \
    openrc \
    iproute2 \
    iptables \
    curl \
    ca-certificates \
    libelf \
    elfutils-dev \
    zlib \
    libgcc \
    musl \
    2>/dev/null || {
    echo "Warning: Some packages may have failed to install, continuing..."
}

# Create necessary directories
mkdir -p "$ROOTFS_DIR/etc/wasm-node"
mkdir -p "$ROOTFS_DIR/var/lib/wasm-node"
mkdir -p "$ROOTFS_DIR/var/log/wasm-node"
mkdir -p "$ROOTFS_DIR/run/wasm-node"
mkdir -p "$ROOTFS_DIR/usr/local/bin"
mkdir -p "$ROOTFS_DIR/root"
mkdir -p "$ROOTFS_DIR/proc"
mkdir -p "$ROOTFS_DIR/sys"
mkdir -p "$ROOTFS_DIR/dev"
mkdir -p "$ROOTFS_DIR/tmp"
mkdir -p "$ROOTFS_DIR/run"

# Install wasm-node binaries
echo "Installing wasm-node binaries..."
cp "target/release/wasm-node" "$ROOTFS_DIR/usr/local/bin/"
cp "target/release/wasm-ctl" "$ROOTFS_DIR/usr/local/bin/"
chmod +x "$ROOTFS_DIR/usr/local/bin/"{wasm-node,wasm-ctl}

# Create default config
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

# Create init script for openrc
cat > "$ROOTFS_DIR/etc/init.d/wasm-node" << 'EOF'
#!/sbin/openrc-run

description="Wasm Cloud Platform Node"

command="/usr/local/bin/wasm-node"
command_args="--config /etc/wasm-node/config.toml"
command_background=true
pidfile="/run/wasm-node.pid"

depend() {
    need net
    after firewall
}

start_pre() {
    checkpath -d -m 0755 -o root:root /var/lib/wasm-node
    checkpath -d -m 0755 -o root:root /var/log/wasm-node
    checkpath -d -m 0755 -o root:root /run/wasm-node
}
EOF
chmod +x "$ROOTFS_DIR/etc/init.d/wasm-node"

# Enable service at boot
mkdir -p "$ROOTFS_DIR/etc/runlevels/default"
ln -sf /etc/init.d/wasm-node "$ROOTFS_DIR/etc/runlevels/default/wasm-node"

# Create /etc/inittab for serial console
cat > "$ROOTFS_DIR/etc/inittab" << 'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default

ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100

::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

# Set hostname
echo "wasm-node-vm" > "$ROOTFS_DIR/etc/hostname"

# Configure networking
cat > "$ROOTFS_DIR/etc/network/interfaces" << 'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.20.0.2
    netmask 255.255.255.0
    gateway 172.20.0.1
EOF

# Create MMDS client script (for fetching config from Firecracker metadata service)
cat > "$ROOTFS_DIR/usr/local/bin/setup-mmds-network" << 'EOF'
#!/bin/sh
# Fetch network config from Firecracker MMDS and apply it
MMDS_TOKEN=$(curl -fsSL -X PUT "http://169.254.169.254/latest/api/token" -H "X-metadata-token-ttl-seconds: 21600")
CONFIG=$(curl -fsSL -H "X-metadata-token: $MMDS_TOKEN" "http://169.254.169.254/latest/meta-data/node_config")

if [ -n "$CONFIG" ]; then
    echo "$CONFIG" > /etc/wasm-node/mmds-config.json
    # Apply NATS URL from MMDS if present
    NATS_URL=$(echo "$CONFIG" | sed -n 's/.*"nats_url":"\([^"]*\)".*/\1/p')
    if [ -n "$NATS_URL" ]; then
        sed -i "s|url = .*|url = \"$NATS_URL\"|" /etc/wasm-node/config.toml
    fi
fi
EOF
chmod +x "$ROOTFS_DIR/usr/local/bin/setup-mmds-network"

# Add MMDS setup to boot
mkdir -p "$ROOTFS_DIR/etc/local.d"
cat > "$ROOTFS_DIR/etc/local.d/mmds.start" << 'EOF'
#!/bin/sh
/usr/local/bin/setup-mmds-network
EOF
chmod +x "$ROOTFS_DIR/etc/local.d/mmds.start"

# Create ext4 image
echo "Creating ext4 image..."
mkdir -p "$OUTPUT_DIR"
IMAGE="$OUTPUT_DIR/wasm-node-rootfs.ext4"

dd if=/dev/zero of="$IMAGE" bs=1M count="$ROOTFS_SIZE_MB"
mkfs.ext4 -F "$IMAGE"

# Mount and copy rootfs
MOUNT="$WORK_DIR/mount"
mkdir -p "$MOUNT"
sudo mount -o loop "$IMAGE" "$MOUNT"
sudo cp -a "$ROOTFS_DIR"/* "$MOUNT/"
sudo umount "$MOUNT"

echo ""
echo "=== wasm-node rootfs build complete ==="
echo "Output: $IMAGE"
echo "Size: $(du -h "$IMAGE" | cut -f1)"
echo ""
echo "Contents:"
ls -la "$ROOTFS_DIR/usr/local/bin/"
