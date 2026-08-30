#!/usr/bin/env bash
# Build all VM images for the testbed.
#
# This is a convenience script that runs all the individual build scripts
# in the correct order.
#
# Usage:
#   ./scripts/vm/build-all-images.sh
#
# Output:
#   ./assets/vmlinux-6.1
#   ./assets/wasm-node-rootfs.ext4
#   ./assets/nats-rootfs.ext4
#   ./assets/postgres-rootfs.ext4
#   ./assets/vault-rootfs.ext4

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

cd "$PROJECT_ROOT"

echo "========================================"
echo "  Building all VM Testbed Images"
echo "========================================"
echo ""

# Create assets directory
mkdir -p ./assets

node_rootfs_is_current() {
    local image=$1
    local schema

    command -v debugfs >/dev/null || return 1
    schema=$(debugfs -R 'cat /etc/wasm-node/image-schema-version' "$image" 2>/dev/null || true)
    [[ "$schema" == "10" ]]
}

kernel_is_current() {
    local kernel=$1
    [[ -f "$kernel.schema" ]] && [[ $(<"$kernel.schema") == "7" ]]
}

nats_rootfs_is_current() {
    local image=$1
    local schema

    command -v debugfs >/dev/null || return 1
    schema=$(debugfs -R 'cat /etc/nats/image-schema-version' "$image" 2>/dev/null || true)
    [[ "$schema" == "2" ]]
}

postgres_rootfs_is_current() {
    local image=$1
    local schema

    command -v debugfs >/dev/null || return 1
    schema=$(debugfs -R 'cat /etc/postgresql-image-schema-version' "$image" 2>/dev/null || true)
    [[ "$schema" == "4" ]]
}

vault_rootfs_is_current() {
    local image=$1
    local schema

    command -v debugfs >/dev/null || return 1
    schema=$(debugfs -R 'cat /etc/vault-image-schema-version' "$image" 2>/dev/null || true)
    [[ "$schema" == "1" ]]
}

# Build kernel
echo "[1/5] Building kernel..."
if [[ ! -f "./assets/vmlinux-6.1" ]]; then
    "$SCRIPT_DIR/build-kernel.sh"
elif ! kernel_is_current "./assets/vmlinux-6.1"; then
    echo "       Existing kernel lacks the current Firecracker boot schema; rebuilding."
    "$SCRIPT_DIR/build-kernel.sh"
else
    echo "       Kernel already exists, skipping."
    echo "       To rebuild explicitly: $SCRIPT_DIR/build-kernel.sh"
fi

# Build NATS rootfs
echo ""
echo "[2/5] Building NATS rootfs..."
if [[ ! -f "./assets/nats-rootfs.ext4" ]]; then
    "$SCRIPT_DIR/build-nats-rootfs.sh"
elif ! nats_rootfs_is_current "./assets/nats-rootfs.ext4"; then
    echo "       Existing NATS rootfs uses a legacy boot schema; rebuilding."
    "$SCRIPT_DIR/build-nats-rootfs.sh"
else
    echo "       NATS rootfs already exists, skipping."
    echo "       To rebuild explicitly: $SCRIPT_DIR/build-nats-rootfs.sh"
fi

# Build wasm-node rootfs
echo ""
echo "[3/5] Building wasm-node rootfs..."
if [[ ! -f "./assets/wasm-node-rootfs.ext4" ]]; then
    "$SCRIPT_DIR/build-node-rootfs.sh"
elif ! node_rootfs_is_current "./assets/wasm-node-rootfs.ext4"; then
    echo "       Existing wasm-node rootfs uses a legacy boot schema; rebuilding."
    "$SCRIPT_DIR/build-node-rootfs.sh"
else
    echo "       wasm-node rootfs already exists, skipping."
    echo "       To rebuild explicitly: $SCRIPT_DIR/build-node-rootfs.sh"
fi

echo "[4/5] Building PostgreSQL rootfs..."
if [[ ! -f "./assets/postgres-rootfs.ext4" ]]; then
    "$SCRIPT_DIR/build-postgres-rootfs.sh"
elif ! postgres_rootfs_is_current "./assets/postgres-rootfs.ext4"; then
    echo "       Existing PostgreSQL rootfs uses a legacy boot schema; rebuilding."
    "$SCRIPT_DIR/build-postgres-rootfs.sh"
else
    echo "       PostgreSQL rootfs already exists, skipping."
fi

echo ""
echo "[5/5] Building Vault rootfs..."
vault_bootstrap=${VAULT_BOOTSTRAP_FILE:-${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json}
if [[ ! -f "./assets/vault-rootfs.ext4" || ! -f "$vault_bootstrap" || ! -f "./assets/vault-test-ca.crt" ]]; then
    "$SCRIPT_DIR/build-vault-rootfs.sh"
elif ! vault_rootfs_is_current "./assets/vault-rootfs.ext4"; then
    echo "       Existing Vault rootfs uses a legacy boot schema; rebuilding."
    "$SCRIPT_DIR/build-vault-rootfs.sh"
else
    echo "       Vault rootfs already exists, skipping."
fi

echo ""
echo "========================================"
echo "  All images built successfully!"
echo "========================================"
echo ""
echo "Assets in ./assets/:"
ls -lh ./assets/
echo ""
echo "Next steps:"
echo "  1. Install Firecracker (if not done): $SCRIPT_DIR/install-firecracker.sh"
echo "  2. Run tests: sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture"
echo "  3. Or use the CLI: cargo run --bin vm-testbed-cli -- spawn-cluster --nodes 3"
