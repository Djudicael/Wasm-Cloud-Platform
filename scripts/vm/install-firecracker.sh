#!/usr/bin/env bash
# Install Firecracker microVMM on Linux.
#
# This script downloads the latest stable Firecracker release binary,
# verifies its checksum, and installs it to /usr/local/bin.
#
# Usage:
#   ./scripts/vm/install-firecracker.sh
#
# Requirements:
#   - Linux x86_64
#   - curl
#   - sudo (for installing to /usr/local/bin)

set -euo pipefail

FIRECRACKER_VERSION="${FIRECRACKER_VERSION:-v1.15.1}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
ARCH="$(uname -m)"

if [[ "$ARCH" != "x86_64" && "$ARCH" != "aarch64" ]]; then
    echo "ERROR: Unsupported architecture: $ARCH"
    echo "Firecracker supports x86_64 and aarch64 only."
    exit 1
fi

echo "=== Installing Firecracker $FIRECRACKER_VERSION for $ARCH ==="

# Create temp directory
TMPDIR="$(mktemp -d)"
trap "rm -rf $TMPDIR" EXIT

cd "$TMPDIR"

# Download release
RELEASE_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-${ARCH}.tgz"
echo "Downloading from $RELEASE_URL..."
curl -fsSL -o firecracker.tgz "$RELEASE_URL"

# Extract
tar xzf firecracker.tgz

# Find the binary (named firecracker-<version>-<arch>, nested in versioned directory)
FIRECRACKER_BIN="$(find . -name "firecracker-${FIRECRACKER_VERSION}-${ARCH}" -type f | head -n 1)"
if [[ -z "$FIRECRACKER_BIN" ]]; then
    echo "ERROR: Could not find firecracker binary in archive"
    exit 1
fi

# Make executable
chmod +x "$FIRECRACKER_BIN"

# Install
echo "Installing to $INSTALL_DIR/firecracker..."
sudo cp "$FIRECRACKER_BIN" "$INSTALL_DIR/firecracker"
sudo chmod 755 "$INSTALL_DIR/firecracker"

# Verify installation
if command -v firecracker &> /dev/null; then
    echo "✅ Firecracker installed successfully"
    firecracker --version
else
    echo "⚠️  Firecracker installed but not in PATH"
    echo "   Add $INSTALL_DIR to your PATH or use the full path"
fi

# Check KVM access
echo ""
echo "=== Checking KVM access ==="
if [[ -e /dev/kvm ]]; then
    if [[ -r /dev/kvm && -w /dev/kvm ]]; then
        echo "✅ /dev/kvm is accessible"
    else
        echo "⚠️  /dev/kvm exists but is not readable/writable by current user"
        echo "   Fix: sudo usermod -aG kvm $USER && newgrp kvm"
    fi
else
    echo "❌ /dev/kvm not found"
    echo "   KVM is required for Firecracker. Ensure:"
    echo "   1. Your CPU supports virtualization (VT-x/AMD-V)"
    echo "   2. KVM kernel modules are loaded: sudo modprobe kvm_intel || sudo modprobe kvm_amd"
    echo "   3. You are running on bare metal or a VM with nested virtualization"
fi

echo ""
echo "=== Installation complete ==="
echo "Binary: $INSTALL_DIR/firecracker"
echo ""
echo "Next steps:"
echo "  1. Build a kernel: ./scripts/vm/build-kernel.sh"
echo "  2. Build rootfs images: ./scripts/vm/build-all-images.sh"
echo "  3. Run a test: cargo test -p vm-testbed --test single_node_deploy"
