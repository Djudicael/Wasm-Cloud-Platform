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
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-2048}"
UBUNTU_RELEASE="${UBUNTU_RELEASE:-noble}"

echo "=== Building wasm-node rootfs ==="

# Ensure wasm-node is built
BUILD_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [[ "${SKIP_RUST_BUILD:-false}" == true ]]; then
    [[ -x "$BUILD_TARGET_DIR/release/wasm-node" && -x "$BUILD_TARGET_DIR/release/wasm-ctl" ]] || {
        echo "SKIP_RUST_BUILD=true but release binaries are missing from $BUILD_TARGET_DIR/release." >&2
        exit 1
    }
    echo "Reusing existing release binaries from $BUILD_TARGET_DIR/release."
else
    echo "Building wasm-node binary..."
    cargo build --release --bin wasm-node
    cargo build --release --bin wasm-ctl
fi

# Create working directory
WORK_DIR="$(mktemp -d)"
TEMP_IMAGE=""
cleanup() {
    rm -rf -- "$WORK_DIR"
    if [[ -n "$TEMP_IMAGE" ]]; then
        rm -f -- "$TEMP_IMAGE"
    fi
}
trap cleanup EXIT

ROOTFS_DIR="$WORK_DIR/rootfs"
mkdir -p "$ROOTFS_DIR"

# Match the glibc ABI used for the native Rust build. Alpine/gcompat cannot
# provide newer glibc symbols such as __isoc23_sscanf.
command -v debootstrap >/dev/null || {
    echo "debootstrap is required (Ubuntu/WSL: sudo apt-get install debootstrap)." >&2
    exit 1
}
echo "Creating minimal Ubuntu $UBUNTU_RELEASE rootfs..."
sudo debootstrap \
    --variant=minbase \
    --include=ca-certificates,curl,iproute2,iptables,libelf1t64 \
    "$UBUNTU_RELEASE" \
    "$ROOTFS_DIR" \
    http://archive.ubuntu.com/ubuntu
sudo chown -R "$(id -u):$(id -g)" "$ROOTFS_DIR"

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
cp "$BUILD_TARGET_DIR/release/wasm-node" "$ROOTFS_DIR/usr/local/bin/"
cp "$BUILD_TARGET_DIR/release/wasm-ctl" "$ROOTFS_DIR/usr/local/bin/"
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

[auth]
trusted_proxies = ["172.20.0.1/32"]

[health]
check_interval_secs = 2
EOF

# Keep a guest-readable schema marker so provisioning can reject legacy cached
# images before starting Firecracker. Bump this value whenever the early-boot
# contract (PID 1, kernel arguments, or network bootstrap) changes.
echo "2" > "$ROOTFS_DIR/etc/wasm-node/image-schema-version"

# Set hostname
echo "wasm-node-vm" > "$ROOTFS_DIR/etc/hostname"

# Create MMDS client script (for fetching config from Firecracker metadata service)
cat > "$ROOTFS_DIR/usr/local/bin/setup-mmds-network" << 'EOF'
#!/bin/sh
# Fetch network config from Firecracker MMDS and apply it
MMDS_TOKEN=$(curl -fsSL -X PUT "http://169.254.169.254/latest/api/token" -H "X-metadata-token-ttl-seconds: 21600")
CONFIG=$(curl -fsSL -H "X-metadata-token: $MMDS_TOKEN" "http://169.254.169.254/node_config")

if [ -n "$CONFIG" ]; then
    echo "$CONFIG" > /etc/wasm-node/mmds-config.json
    NODE_ID=$(echo "$CONFIG" | sed -n 's/.*"node_id":"\([^"]*\)".*/\1/p')
    NATS_URL=$(echo "$CONFIG" | sed -n 's/.*"nats_url":"\([^"]*\)".*/\1/p')
    IP_ADDRESS=$(echo "$CONFIG" | sed -n 's/.*"ip":"\([^"]*\)".*/\1/p')
    GATEWAY=$(echo "$CONFIG" | sed -n 's/.*"gateway":"\([^"]*\)".*/\1/p')
    if [ -n "$NODE_ID" ]; then
        sed -i "s|node_id = .*|node_id = \"$NODE_ID\"|" /etc/wasm-node/config.toml
        hostname "$NODE_ID"
    fi
    if [ -n "$NATS_URL" ]; then
        sed -i "s|url = .*|url = \"$NATS_URL\"|" /etc/wasm-node/config.toml
    fi
    if [ -n "$IP_ADDRESS" ] && [ -n "$GATEWAY" ]; then
        ip address flush dev eth0 scope global
        ip address add "$IP_ADDRESS/24" dev eth0
        ip route replace default via "$GATEWAY" dev eth0
    fi
fi
EOF
chmod +x "$ROOTFS_DIR/usr/local/bin/setup-mmds-network"

# Use a deliberately small PID 1 for the disposable test guest. It mounts the
# kernel filesystems, establishes the bootstrap address needed to reach MMDS,
# applies the per-node address/config, and then execs the platform node.
rm -f "$ROOTFS_DIR/sbin/init"
cat > "$ROOTFS_DIR/sbin/init" << 'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
ip link set lo up
ip link set eth0 up
NODE_ID=vm-node
IP_ADDRESS=
GATEWAY=172.20.0.1
for ARGUMENT in $(cat /proc/cmdline); do
    case "$ARGUMENT" in
        wcp.node_id=*) NODE_ID=${ARGUMENT#wcp.node_id=} ;;
        wcp.ip=*) IP_ADDRESS=${ARGUMENT#wcp.ip=} ;;
        wcp.gateway=*) GATEWAY=${ARGUMENT#wcp.gateway=} ;;
    esac
done
if [ -n "$IP_ADDRESS" ]; then
    ip address flush dev eth0 scope global
    ip address add "$IP_ADDRESS/24" dev eth0
    ip route replace default via "$GATEWAY" dev eth0
else
    ip address add 172.20.0.2/24 dev eth0
    ip route replace 169.254.169.254 dev eth0
    /usr/local/bin/setup-mmds-network
    NODE_ID=$(sed -n 's/^node_id = "\([^"]*\)"/\1/p' /etc/wasm-node/config.toml)
fi
exec /usr/local/bin/wasm-node \
    --config /etc/wasm-node/config.toml \
    --node-id "$NODE_ID" \
    --db-path /var/lib/wasm-node/state.redb \
    --nats-url nats://172.20.0.10:4222 \
    --proxy-https-port 0 \
    --admin-bind-address 0.0.0.0 \
    --artifact-bind-address 0.0.0.0 \
    --deploy-ingress-bind-address 0.0.0.0 \
    --admin-advertised-host "$IP_ADDRESS" \
    --auth-enabled true \
    --auth-write-token local-test-write-token-change-me \
    --auth-require-tls false
EOF
chmod +x "$ROOTFS_DIR/sbin/init"

# Create ext4 image
echo "Creating ext4 image..."
mkdir -p "$OUTPUT_DIR"
IMAGE="$OUTPUT_DIR/wasm-node-rootfs.ext4"
TEMP_IMAGE="$(mktemp --tmpdir="$OUTPUT_DIR" .wasm-node-rootfs.ext4.XXXXXX)"

dd if=/dev/zero of="$TEMP_IMAGE" bs=1M count="$ROOTFS_SIZE_MB"
mkfs.ext4 -F "$TEMP_IMAGE"

# Mount and copy rootfs
MOUNT="$WORK_DIR/mount"
mkdir -p "$MOUNT"
sudo mount -o loop "$TEMP_IMAGE" "$MOUNT"
sudo cp -a "$ROOTFS_DIR"/* "$MOUNT/"
sudo umount "$MOUNT"
mv -f -- "$TEMP_IMAGE" "$IMAGE"
TEMP_IMAGE=""

echo ""
echo "=== wasm-node rootfs build complete ==="
echo "Output: $IMAGE"
echo "Size: $(du -h "$IMAGE" | cut -f1)"
echo ""
echo "Contents:"
ls -la "$ROOTFS_DIR/usr/local/bin/"
