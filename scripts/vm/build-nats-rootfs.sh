#!/usr/bin/env bash
# Build a root filesystem for the NATS microVM.
#
# This creates a minimal Alpine Linux rootfs with NATS Server + JetStream.
#
# Usage:
#   ./scripts/vm/build-nats-rootfs.sh
#
# Output:
#   ./assets/nats-rootfs.ext4

set -euo pipefail

OUTPUT_DIR="${OUTPUT_DIR:-./assets}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-256}"
ALPINE_VERSION="${ALPINE_VERSION:-3.19}"
NATS_VERSION="${NATS_VERSION:-2.10.14}"

echo "=== Building NATS rootfs ==="

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

# Install packages
mkdir -p "$ROOTFS_DIR/etc/apk"
cat > "$ROOTFS_DIR/etc/apk/repositories" << EOF
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main
https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community
EOF

sudo cp -L /etc/resolv.conf "$ROOTFS_DIR/etc/resolv.conf"
sudo chroot "$ROOTFS_DIR" /sbin/apk add --no-cache \
    alpine-base \
    openrc \
    iproute2 \
    curl \
    ca-certificates
sudo chown -R "$(id -u):$(id -g)" "$ROOTFS_DIR"

# Download NATS binary
echo "Downloading NATS Server $NATS_VERSION..."
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64) NATS_ARCH="amd64" ;;
    aarch64) NATS_ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

mkdir -p "$ROOTFS_DIR/usr/local/bin"
curl -fsSL \
    "https://github.com/nats-io/nats-server/releases/download/v${NATS_VERSION}/nats-server-v${NATS_VERSION}-linux-${NATS_ARCH}.tar.gz" \
    -o "$WORK_DIR/nats-server.tar.gz"

tar xzf "$WORK_DIR/nats-server.tar.gz" -C "$WORK_DIR"
cp "$WORK_DIR/nats-server-v${NATS_VERSION}-linux-${NATS_ARCH}/nats-server" "$ROOTFS_DIR/usr/local/bin/"
chmod +x "$ROOTFS_DIR/usr/local/bin/nats-server"

# Create directories
mkdir -p "$ROOTFS_DIR"/{etc/nats,var/lib/nats,run/nats,proc,sys,dev,tmp}
echo "2" > "$ROOTFS_DIR/etc/nats/image-schema-version"

# Create NATS config
cat > "$ROOTFS_DIR/etc/nats/nats-server.conf" << 'EOF'
# NATS Server configuration for microVM testbed

port: 4222
http_port: 8222

jetstream {
    store_dir: "/var/lib/nats"
    max_memory_store: 1GB
    max_file_store: 10GB
}

# Allow connections from the testbed subnet
allow_non_tls: true

# Logging
debug: false
trace: false
logtime: true
logfile: "/var/log/nats-server.log"
EOF

# Create init script
cat > "$ROOTFS_DIR/etc/init.d/nats-server" << 'EOF'
#!/sbin/openrc-run

description="NATS Server"

command="/usr/local/bin/nats-server"
command_args="-c /etc/nats/nats-server.conf"
command_background=true
pidfile="/run/nats-server.pid"

depend() {
    need net
    after firewall
}

start_pre() {
    checkpath -d -m 0755 -o root:root /var/lib/nats
    checkpath -d -m 0755 -o root:root /var/log
}
EOF
chmod +x "$ROOTFS_DIR/etc/init.d/nats-server"

# Enable at boot
mkdir -p "$ROOTFS_DIR/etc/runlevels/default"
ln -sf /etc/init.d/nats-server "$ROOTFS_DIR/etc/runlevels/default/nats-server"

# Use a small deterministic PID 1 for this disposable service VM. This avoids
# depending on distribution runlevels and makes the configured address match
# the testbed's NATS convention exactly.
rm -f "$ROOTFS_DIR/sbin/init"
cat > "$ROOTFS_DIR/sbin/init" << 'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
ip link set lo up
ip link set eth0 up
ip address add 172.20.0.10/24 dev eth0
ip route replace default via 172.20.0.1 dev eth0
mkdir -p /var/lib/nats /var/log
exec /usr/local/bin/nats-server -c /etc/nats/nats-server.conf
EOF
chmod +x "$ROOTFS_DIR/sbin/init"

# Set hostname
echo "nats-vm" > "$ROOTFS_DIR/etc/hostname"

# Configure networking
cat > "$ROOTFS_DIR/etc/network/interfaces" << 'EOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.20.0.10
    netmask 255.255.255.0
    gateway 172.20.0.1
EOF

# Create ext4 image
echo "Creating ext4 image..."
mkdir -p "$OUTPUT_DIR"
IMAGE="$OUTPUT_DIR/nats-rootfs.ext4"

dd if=/dev/zero of="$IMAGE" bs=1M count="$ROOTFS_SIZE_MB"
mkfs.ext4 -F "$IMAGE"

MOUNT="$WORK_DIR/mount"
mkdir -p "$MOUNT"
sudo mount -o loop "$IMAGE" "$MOUNT"
sudo cp -a "$ROOTFS_DIR"/* "$MOUNT/"
sudo umount "$MOUNT"

echo ""
echo "=== NATS rootfs build complete ==="
echo "Output: $IMAGE"
echo "Size: $(du -h "$IMAGE" | cut -f1)"
