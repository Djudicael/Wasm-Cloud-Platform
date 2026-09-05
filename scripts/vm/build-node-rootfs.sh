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

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
    run_privileged() { wsl.exe -u root -- "$@"; }
else
    sudo -v
    run_privileged() { sudo -E "$@"; }
fi

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
    cargo build --release --bin wasm-node --features ebpf
    cargo build --release --bin wasm-ctl
fi

if [[ "${SKIP_EBPF_BUILD:-false}" == true ]]; then
    EBPF_BUILD_DIR="./crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release"
    for object in process_tracker tcp_monitor fd_watcher mem_pressure disk_monitor syscall_counter namespace_enforcer; do
        [[ -x "$EBPF_BUILD_DIR/$object" ]] || {
            echo "SKIP_EBPF_BUILD=true but eBPF object is missing: $EBPF_BUILD_DIR/$object" >&2
            exit 1
        }
    done
    echo "Reusing existing eBPF objects from $EBPF_BUILD_DIR."
else
    echo "Building eBPF programs..."
    "$(dirname "$0")/../ebpf/build-ebpf.sh"
fi

# Create working directory
WORK_DIR="$(mktemp -d)"
TEMP_IMAGE=""
MOUNT=""
cleanup() {
    if [[ -n "$MOUNT" ]] && mountpoint -q "$MOUNT"; then
        run_privileged umount "$MOUNT" || true
    fi
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
run_privileged debootstrap \
    --variant=minbase \
    --include=ca-certificates,chrony,curl,iproute2,iptables,libelf1t64 \
    "$UBUNTU_RELEASE" \
    "$ROOTFS_DIR" \
    http://archive.ubuntu.com/ubuntu
run_privileged chown -R "$(id -u):$(id -g)" "$ROOTFS_DIR"

# Create necessary directories
mkdir -p "$ROOTFS_DIR/etc/wasm-node"
mkdir -p "$ROOTFS_DIR/var/lib/wasm-node"
mkdir -p "$ROOTFS_DIR/var/log/wasm-node"
mkdir -p "$ROOTFS_DIR/run/wasm-node"
mkdir -p "$ROOTFS_DIR/usr/local/bin"
mkdir -p "$ROOTFS_DIR/opt/wasm-node/ebpf"
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
EBPF_BUILD_DIR="${EBPF_BUILD_DIR:-./crates/ebpf-monitor/bpf/target/bpfel-unknown-none/release}"
for object in process_tracker tcp_monitor fd_watcher mem_pressure disk_monitor syscall_counter namespace_enforcer; do
    cp "$EBPF_BUILD_DIR/$object" "$ROOTFS_DIR/opt/wasm-node/ebpf/$object.o"
done

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
stub_port = 53

[auth]
trusted_proxies = ["172.20.0.1/32"]

[health]
check_interval_secs = 2
# This disposable 2-GiB guest keeps 512 MiB of node filesystem space in
# reserve and admits application pools against a 1.5-GiB memory budget,
# leaving the remainder for the kernel and platform process overhead.
min_disk_free_bytes = 536870912
min_disk_free_inodes = 10000
max_memory_bytes = 1610612736
default_memory_pages = 2048
default_max_instances = 10

[ebpf]
required = false
EOF

# Keep a guest-readable schema marker so provisioning can reject legacy cached
# images before starting Firecracker. Bump this value whenever the early-boot
# contract (PID 1, kernel arguments, or network bootstrap) changes.
echo "14" > "$ROOTFS_DIR/etc/wasm-node/image-schema-version"

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
    OTLP_ENDPOINT=$(echo "$CONFIG" | sed -n 's/.*"otlp_endpoint":"\([^"]*\)".*/\1/p')
    if [ -n "$NODE_ID" ]; then
        sed -i "s|node_id = .*|node_id = \"$NODE_ID\"|" /etc/wasm-node/config.toml
        hostname "$NODE_ID"
    fi
    if [ -n "$NATS_URL" ]; then
        sed -i "s|url = .*|url = \"$NATS_URL\"|" /etc/wasm-node/config.toml
    fi
    if [ -n "$OTLP_ENDPOINT" ]; then
        sed -i "/^\[logging\]$/a otlp_endpoint = \"$OTLP_ENDPOINT\"" /etc/wasm-node/config.toml
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
# applies the per-node address/config, and supervises the platform node. Keeping
# wasm-node out of PID 1 lets the kernel terminate it under OOM and lets the
# guest recover instead of entering "Out of memory and no killable processes".
rm -f "$ROOTFS_DIR/sbin/init"
cat > "$ROOTFS_DIR/sbin/init" << 'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t tracefs tracefs /sys/kernel/tracing 2>/dev/null || true
mkdir -p /sys/fs/bpf
mount -t bpf bpf /sys/fs/bpf 2>/dev/null || true
ip link set lo up
ip link set eth0 up
NODE_ID=vm-node
IP_ADDRESS=
GATEWAY=172.20.0.1
EBPF_TEST_FAULT=
EBPF_REQUIRED=0
EBPF_DROP_CAPABILITIES=0
OTLP_ENDPOINT=
OIDC_ISSUER_URL=
OIDC_AUDIENCE=
OIDC_JWKS_URL=
for ARGUMENT in $(cat /proc/cmdline); do
    case "$ARGUMENT" in
        wcp.node_id=*) NODE_ID=${ARGUMENT#wcp.node_id=} ;;
        wcp.ip=*) IP_ADDRESS=${ARGUMENT#wcp.ip=} ;;
        wcp.gateway=*) GATEWAY=${ARGUMENT#wcp.gateway=} ;;
        wcp.ebpf_test_fault=*) EBPF_TEST_FAULT=${ARGUMENT#wcp.ebpf_test_fault=} ;;
        wcp.ebpf_required=1) EBPF_REQUIRED=1 ;;
        wcp.ebpf_drop_capabilities=1) EBPF_DROP_CAPABILITIES=1 ;;
        wcp.otlp_endpoint=*) OTLP_ENDPOINT=${ARGUMENT#wcp.otlp_endpoint=} ;;
        wcp.oidc_issuer_url=*) OIDC_ISSUER_URL=${ARGUMENT#wcp.oidc_issuer_url=} ;;
        wcp.oidc_audience=*) OIDC_AUDIENCE=${ARGUMENT#wcp.oidc_audience=} ;;
        wcp.oidc_jwks_url=*) OIDC_JWKS_URL=${ARGUMENT#wcp.oidc_jwks_url=} ;;
    esac
done
case "$EBPF_TEST_FAULT" in
    ""|missing_capability|permission_denied|program_rejected|probe_unavailable|missing_btf|consumer_exit) ;;
    *)
        echo "ignoring invalid local eBPF test fault: $EBPF_TEST_FAULT" >&2
        EBPF_TEST_FAULT=
        ;;
esac
# Emit host-class-specific mitigation evidence before the node starts. Static
# Kconfig auditing alone cannot account for the physical CPU, host kernel,
# firmware, or microcode presented to this guest.
echo "WCP_KERNEL_AUDIT_BEGIN"
echo "WCP_KERNEL_RELEASE=$(uname -r)"
for VULNERABILITY_FILE in /sys/devices/system/cpu/vulnerabilities/*; do
    [ -f "$VULNERABILITY_FILE" ] || continue
    VULNERABILITY_NAME=${VULNERABILITY_FILE##*/}
    VULNERABILITY_STATUS=$(tr '\n' ' ' < "$VULNERABILITY_FILE")
    echo "WCP_KERNEL_VULNERABILITY=${VULNERABILITY_NAME}|${VULNERABILITY_STATUS}"
done
echo "WCP_KERNEL_AUDIT_END"
if [ "$EBPF_REQUIRED" -eq 1 ]; then
    sed -i 's/^required = .*/required = true/' /etc/wasm-node/config.toml
fi
if [ -n "$EBPF_TEST_FAULT" ]; then
    export WASM_EBPF_TEST_FAULT="$EBPF_TEST_FAULT"
fi
if [ -n "$OTLP_ENDPOINT" ]; then
    export WASM_NODE_LOGGING_OTLP_ENDPOINT="$OTLP_ENDPOINT"
fi
if [ -n "$OIDC_ISSUER_URL" ] || [ -n "$OIDC_AUDIENCE" ] || [ -n "$OIDC_JWKS_URL" ]; then
    if [ -z "$OIDC_ISSUER_URL" ] || [ -z "$OIDC_AUDIENCE" ] || [ -z "$OIDC_JWKS_URL" ]; then
        echo "incomplete OIDC boot configuration; refusing to start wasm-node" >&2
        poweroff -f
        exit 1
    fi
    cat >> /etc/wasm-node/config.toml <<OIDC_CONFIG

[gateway.oidc]
issuer_url = "$OIDC_ISSUER_URL"
audience = "$OIDC_AUDIENCE"
jwks_url = "$OIDC_JWKS_URL"
jwks_refresh_secs = 30
clock_skew_secs = 30
OIDC_CONFIG
fi
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
# Use the platform's loopback split-DNS stub for `.internal` and retain a
# secondary resolver for public/operator-managed names. Production images must
# replace the secondary address with operator-controlled recursive resolvers.
cat > /etc/resolv.conf <<'RESOLV_CONF'
nameserver 127.0.0.1
nameserver 1.1.1.1
options timeout:1 attempts:2
RESOLV_CONF
# Firecracker guest clocks can lag after a laptop/WSL host is suspended. That
# makes freshly issued JWTs appear expired to clients even while readiness is
# green. Synchronize before accepting traffic and keep chronyd running. The
# public source is appropriate only for this disposable local test image;
# production images must use operator-controlled redundant time sources.
if command -v chronyd >/dev/null 2>&1; then
    chronyd -q -t 15 'server 162.159.200.1 iburst' || {
        echo "initial clock synchronization failed; refusing to start wasm-node" >&2
        poweroff -f
        exit 1
    }
    chronyd 'server 162.159.200.1 iburst' || {
        echo "failed to start continuous clock synchronization" >&2
        poweroff -f
        exit 1
    }
fi
NODE_PID=
SHUTDOWN_REQUESTED=0
RESTART_DELAY=2
forward_shutdown() {
    SHUTDOWN_REQUESTED=1
    if [ -n "$NODE_PID" ]; then
        kill -TERM "$NODE_PID" 2>/dev/null || true
    fi
}
trap forward_shutdown TERM INT

while :; do
    STARTED_AT=$(cut -d. -f1 /proc/uptime)
    set -- /usr/local/bin/wasm-node
    if [ "$EBPF_DROP_CAPABILITIES" -eq 1 ]; then
        set -- setpriv --bounding-set=-bpf,-sys_admin,-perfmon,-net_admin -- /usr/local/bin/wasm-node
    fi
    "$@" \
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
        --auth-require-tls false &
    NODE_PID=$!
    wait "$NODE_PID"
    NODE_STATUS=$?
    NODE_PID=
    if [ "$SHUTDOWN_REQUESTED" -eq 1 ]; then
        exit "$NODE_STATUS"
    fi
    ENDED_AT=$(cut -d. -f1 /proc/uptime)
    RUNTIME_SECONDS=$((ENDED_AT - STARTED_AT))
    if [ "$RUNTIME_SECONDS" -ge 60 ]; then
        RESTART_DELAY=2
    fi
    echo "wasm-node exited with status $NODE_STATUS after ${RUNTIME_SECONDS}s; restarting in ${RESTART_DELAY}s" >&2
    sync
    sleep "$RESTART_DELAY"
    if [ "$RUNTIME_SECONDS" -lt 60 ] && [ "$RESTART_DELAY" -lt 30 ]; then
        RESTART_DELAY=$((RESTART_DELAY * 2))
        if [ "$RESTART_DELAY" -gt 30 ]; then
            RESTART_DELAY=30
        fi
    fi
done
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
run_privileged mount -o loop "$TEMP_IMAGE" "$MOUNT"
run_privileged cp -a "$ROOTFS_DIR"/. "$MOUNT/"
run_privileged umount "$MOUNT"
mv -f -- "$TEMP_IMAGE" "$IMAGE"
TEMP_IMAGE=""

echo ""
echo "=== wasm-node rootfs build complete ==="
echo "Output: $IMAGE"
echo "Size: $(du -h "$IMAGE" | cut -f1)"
echo ""
echo "Contents:"
ls -la "$ROOTFS_DIR/usr/local/bin/"
