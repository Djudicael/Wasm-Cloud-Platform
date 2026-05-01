# WSL Quickstart Guide: MicroVM Testbed

This guide is specifically for running the microVM testbed inside **WSL2** (Windows Subsystem for Linux), which is the required environment.

## Why WSL2?

The microVM testbed requires:
- **KVM** (`/dev/kvm`) for hardware virtualization
- **`CAP_NET_ADMIN`** for creating TAP devices
- **Linux kernel 5.8+** for eBPF/BTF support
- **Unix signals** (`SIGKILL`, `SIGTERM`) for chaos testing

These are not available on native Windows.

## Prerequisites

### 1. Install WSL2

```powershell
# Run in PowerShell as Administrator
wsl --install
# Restart your computer when prompted
```

### 2. Verify WSL2

```bash
# Inside WSL
wsl.exe -l -v
# Should show: NAME STATE VERSION
#                * Ubuntu Running 2

# Check kernel version
uname -r
# Should be 5.15+ (WSL2 default is usually 5.15 or newer)
```

### 3. Enable Nested Virtualization (if WSL kernel < 5.15)

If your WSL kernel is older, you may need to enable nested virtualization in Windows:

```powershell
# PowerShell as Administrator
# Check if virtualization is enabled in WSL
wsl cat /proc/cpuinfo | findstr vmx
# If empty, nested virtualization may not be enabled
```

For older WSL2 builds, create `.wslconfig`:

```ini
# C:\Users\YOUR_USERNAME\.wslconfig
[wsl2]
processors=4
memory=8GB
nestedVirtualization=true
```

Then restart WSL:

```powershell
wsl --shutdown
```

### 4. Install Dependencies in WSL

```bash
# Update packages
sudo apt-get update

# Install build dependencies
sudo apt-get install -y \
    build-essential \
    bc \
    bison \
    flex \
    libssl-dev \
    libelf-dev \
    curl \
    iproute2 \
    iptables \
    qemu-utils \
    pkg-config

# Verify KVM is available
ls -la /dev/kvm
# Should show: crw-rw----+ 1 root kvm 10, 232 ...

# Fix permissions if needed
sudo chmod 666 /dev/kvm
sudo usermod -aG kvm $USER
# Log out and back in for group change to take effect
```

### 5. Install Rust in WSL

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Add WASI target
rustup target add wasm32-wasip2

# Verify
rustc --version  # Should be 1.80+
```

## Project Setup in WSL

### 1. Access Your Windows Files

Your Windows files are mounted under `/mnt/`:

```bash
# Navigate to your project
cd /mnt/d/dev/Wasm-Cloud-Platform

# Or clone directly in WSL
cd ~
git clone <your-repo-url>
cd Wasm-Cloud-Platform
```

### 2. Build the Platform

```bash
# Build all workspace binaries
cargo build --release --bin wasm-node --bin wasm-ctl

# Build test Wasm app
RUSTFLAGS='--cfg tokio_unstable' \
    cargo build --manifest-path apps/hello-axum/Cargo.toml \
    --target wasm32-wasip2 --release
```

## Running the Testbed

### Option A: Automated Scripts (Recommended)

```bash
# 1. Install Firecracker
./scripts/vm/install-firecracker.sh

# 2. Build all VM images (kernel + rootfs)
./scripts/vm/build-all-images.sh

# 3. Run tests
sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture
```

### Option B: Manual Steps

```bash
# Step 1: Build kernel
./scripts/vm/build-kernel.sh

# Step 2: Build NATS rootfs
./scripts/vm/build-nats-rootfs.sh

# Step 3: Build wasm-node rootfs
./scripts/vm/build-node-rootfs.sh

# Step 4: Set up network
sudo ./scripts/vm/setup-network.sh

# Step 5: Run tests
sudo cargo test -p vm-testbed -- --nocapture
```

## Common WSL-Specific Issues

### Issue: `/dev/kvm` does not exist

```bash
# Check WSL version (must be WSL2)
wsl.exe -l -v

# If it's WSL1, convert:
# PowerShell: wsl --set-version Ubuntu 2

# Check Windows Hyper-V is enabled
# PowerShell: Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V
```

### Issue: Permission denied on `/dev/kvm`

```bash
# Temporary fix
sudo chmod 666 /dev/kvm

# Permanent fix - add udev rule
sudo tee /etc/udev/rules.d/50-kvm.rules << 'EOF'
KERNEL=="kvm", GROUP="kvm", MODE="0666"
EOF
sudo udevadm control --reload-rules
sudo udevadm trigger
```

### Issue: Slow file access on `/mnt/d/`

WSL2 has slower I/O on Windows-mounted drives. For better performance:

```bash
# Clone the repo directly in WSL filesystem (not /mnt/)
cd ~
git clone <your-repo-url> Wasm-Cloud-Platform
cd Wasm-Cloud-Platform

# Now all operations are in native Linux ext4
```

### Issue: Firecracker fails with "KVM_CREATE_VM"

```bash
# Check nested virtualization
cat /proc/cpuinfo | grep -c vmx
# Should be > 0

# If 0, your CPU may not support it, or it's disabled in BIOS
# Enable Intel VT-x / AMD-V in BIOS settings
```

### Issue: Network bridge conflicts with WSL networking

```bash
# WSL2 uses its own virtual switch. Use a non-conflicting subnet.
# The testbed uses 172.20.0.0/24 which should not conflict.

# Check for conflicts
ip addr show
# Look for 172.20.0.x addresses
```

## Running Tests

### Single Node Deploy Test

```bash
# WSL terminal
cd /mnt/d/dev/Wasm-Cloud-Platform  # or your WSL path

sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture
```

Expected output:
```
✅ MicroVM is healthy at 172.20.0.2:9090
✅ Response body: Hello from wasm-node!
✅ Test passed!
```

### Chaos Tests

```bash
# L2: Node kill and restart
sudo cargo test -p vm-testbed --test vm_chaos test_vm_kill_and_restart -- --nocapture

# L5: Network partition
sudo cargo test -p vm-testbed --test vm_chaos test_vm_network_partition -- --nocapture
```

### Using the CLI

```bash
# Spawn a cluster
cargo run --bin vm-testbed-cli -- spawn-cluster --nodes 3

# In another terminal, check health
cargo run --bin vm-testbed-cli -- health --ip 172.20.0.2

# Teardown when done
cargo run --bin vm-testbed-cli -- teardown
```

## VS Code Integration

For the best development experience, use VS Code with the WSL extension:

1. Install "WSL" extension in VS Code
2. Open project: `code /mnt/d/dev/Wasm-Cloud-Platform`
3. VS Code will automatically connect to WSL
4. Use the integrated terminal — it's already in WSL

## Performance Tips

### 1. Store Project in WSL Filesystem

```bash
# Instead of /mnt/d/... use ~/projects/
mkdir -p ~/projects
cd ~/projects
git clone <repo>
```

This gives native Linux ext4 performance instead of 9P protocol overhead.

### 2. Allocate More Memory to WSL

Create `C:\Users\YOUR_USERNAME\.wslconfig`:

```ini
[wsl2]
memory=8GB
processors=4
swap=2GB
localhostForwarding=true
nestedVirtualization=true
```

Then: `wsl --shutdown`

### 3. Use `cargo build` in WSL, not Windows

Never run `cargo build` from Windows PowerShell/CMD for this project. Always use WSL.

## Cleanup

```bash
# Kill all Firecracker processes
sudo pkill -f firecracker

# Remove bridge and TAP devices
sudo ip link del br-wasm 2>/dev/null || true
for i in $(ip link show | grep tap- | awk -F: '{print $2}' | xargs); do
    sudo ip link del $i 2>/dev/null || true
done

# Clean up temp files
sudo rm -rf /tmp/vm-testbed-*
sudo rm -rf /tmp/fc-*
```

## Next Steps

- Read the [full manual setup guide](manual-setup.md) for detailed explanations
- Check [CI integration guide](ci-integration.md) for GitHub Actions/GitLab setup
- Explore [architecture documentation](architecture.md) for design details
