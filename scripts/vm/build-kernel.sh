#!/usr/bin/env bash
# Build a minimal Linux kernel for Firecracker microVMs.
#
# This script downloads the Linux source, configures it for Firecracker,
# and builds a vmlinux ELF binary.
#
# Usage:
#   ./scripts/vm/build-kernel.sh [version]
#
# Arguments:
#   version - Linux kernel version (default: 6.1.80)
#
# Output:
#   ./assets/vmlinux-${VERSION}
#
# Requirements:
#   - build-essential (gcc, make, etc.)
#   - bc
#   - bison
#   - flex
#   - libssl-dev
#   - libelf-dev
#   - dwarves (pahole, for BTF)

set -euo pipefail

# Resolve project root (relative to this script's location)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

VERSION="${1:-6.1.80}"
SHORT_VERSION="${VERSION%.*}"  # e.g., 6.1
OUTPUT_DIR="${OUTPUT_DIR:-$PROJECT_ROOT/assets}"
JOBS="$(nproc)"

echo "=== Building Linux Kernel $VERSION for Firecracker ==="

# Install dependencies
echo "Checking dependencies..."
MISSING=""
for pkg in build-essential bc bison flex libssl-dev libelf-dev dwarves; do
    if ! dpkg -s "$pkg" 2>/dev/null | grep -q '^Status:.*installed'; then
        MISSING="$MISSING $pkg"
    fi
done

if [[ -n "$MISSING" ]]; then
    echo "Installing missing packages:$MISSING"
    sudo apt-get update
    sudo apt-get install -y $MISSING
fi

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Download kernel source
KERNEL_DIR="/tmp/linux-$VERSION"
if [[ ! -d "$KERNEL_DIR" ]]; then
    echo "Downloading Linux $VERSION source..."
    cd /tmp
    if [[ ! -f "linux-$VERSION.tar.xz" ]]; then
        curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v${SHORT_VERSION%%.*}.x/linux-$VERSION.tar.xz" \
            -o "linux-$VERSION.tar.xz"
    fi
    tar xf "linux-$VERSION.tar.xz"
fi

cd "$KERNEL_DIR"

# Use Firecracker's recommended config as base
echo "Configuring kernel for Firecracker..."

# Start with tinyconfig and add what we need
make tinyconfig

# Enable essential options
cat >> .config << 'EOF'
# 64-bit kernel
CONFIG_64BIT=y
CONFIG_SMP=y
CONFIG_NR_CPUS=64
CONFIG_MODULES=y
CONFIG_MULTIUSER=y
CONFIG_BINFMT_ELF=y
CONFIG_BINFMT_SCRIPT=y
CONFIG_HIGH_RES_TIMERS=y
CONFIG_POSIX_TIMERS=y
CONFIG_FUTEX=y
CONFIG_FUTEX_PI=y
CONFIG_EPOLL=y
CONFIG_SIGNALFD=y
CONFIG_TIMERFD=y
CONFIG_EVENTFD=y
CONFIG_AIO=y
CONFIG_IO_URING=y
CONFIG_INOTIFY_USER=y
CONFIG_MEMBARRIER=y
CONFIG_RSEQ=y
CONFIG_SYSVIPC=y
CONFIG_SYSVIPC_SYSCTL=y
CONFIG_POSIX_MQUEUE=y
CONFIG_SHMEM=y
CONFIG_TMPFS=y
CONFIG_TMPFS_POSIX_ACL=y

# KVM/Firecracker paravirtual clock. Firecracker does not provide a legacy PIT,
# HPET, or ACPI PM timer, so a tinyconfig kernel otherwise stalls while trying
# to calibrate an unstable TSC.
CONFIG_HYPERVISOR_GUEST=y
CONFIG_PARAVIRT=y
CONFIG_KVM_GUEST=y

# Essential subsystem support
CONFIG_BLOCK=y
CONFIG_BLK_DEV=y
CONFIG_BLK_DEV_LOOP=y
CONFIG_BLK_DEV_SD=y
CONFIG_SCSI=y
CONFIG_SCSI_MOD=y
CONFIG_SCSI_SPI_ATTRS=y
CONFIG_ATA=y
CONFIG_ATA_SFF=y
CONFIG_ATA_BMDMA=y
CONFIG_ATA_PIIX=y
CONFIG_NET=y
CONFIG_INET=y
CONFIG_UNIX=y
CONFIG_NETDEVICES=y
CONFIG_VIRTIO_MENU=y
CONFIG_VIRTIO=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_PRINTK=y
CONFIG_EARLY_PRINTK=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_TTY=y
CONFIG_UNIX98_PTYS=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_TMPFS=y
CONFIG_PROC_FS=y
CONFIG_SYSFS=y
CONFIG_TMPFS_POSIX_ACL=y
CONFIG_EXT4_FS=y
CONFIG_EXT4_USE_FOR_EXT2=y
CONFIG_MSDOS_FS=y
CONFIG_VFAT_FS=y
CONFIG_NLS=y
CONFIG_NLS_DEFAULT="utf-8"
CONFIG_NLS_CODEPAGE_437=y
CONFIG_NLS_ISO8859_1=y

# eBPF support
CONFIG_BPF=y
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_JIT=y
CONFIG_BPF_JIT_ALWAYS_ON=y
CONFIG_CGROUP_BPF=y
CONFIG_BPF_EVENTS=y

# Tracing hooks used by the platform's tracepoint and kprobe programs.
# KALLSYMS is required for resolving kprobe targets by function name.
CONFIG_KALLSYMS=y
CONFIG_KPROBES=y
CONFIG_KPROBE_EVENTS=y
CONFIG_UPROBES=y
CONFIG_UPROBE_EVENTS=y
CONFIG_FTRACE=y
CONFIG_TRACING=y
CONFIG_FTRACE_SYSCALLS=y

# BTF support (required for modern eBPF)
CONFIG_DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT=y
CONFIG_DEBUG_INFO_BTF=y
CONFIG_DEBUG_INFO_BTF_MODULES=y

# Enable BPF LSM (optional but recommended)
CONFIG_BPF_LSM=y
CONFIG_LSM="lockdown,yama,integrity,apparmor,bpf"

# Networking for NATS and HTTP
CONFIG_NETFILTER=y
CONFIG_NETFILTER_ADVANCED=y
CONFIG_NF_CONNTRACK=y
CONFIG_NF_NAT=y
CONFIG_IP_NF_IPTABLES=y
CONFIG_IP_NF_FILTER=y
CONFIG_IP_NF_NAT=y
CONFIG_IP_NF_TARGET_MASQUERADE=y

# Enable IPv6 (NATS may use it)
CONFIG_IPV6=y

# Enable bridge support
CONFIG_BRIDGE=y
CONFIG_BRIDGE_NETFILTER=y

# Enable overlayfs (for container-like workloads)
CONFIG_OVERLAY_FS=y

# Enable cgroups v2
CONFIG_CGROUPS=y
CONFIG_CGROUP_CPUACCT=y
CONFIG_CGROUP_DEVICE=y
CONFIG_CGROUP_FREEZER=y
CONFIG_CGROUP_SCHED=y
CONFIG_CGROUP_PIDS=y
CONFIG_CGROUP_RDMA=y
CONFIG_CGROUP_PERF=y
CONFIG_CGROUP_BPF=y
CONFIG_CGROUP_MISC=y
CONFIG_MEMCG=y
CONFIG_BLK_CGROUP=y

# Enable namespaces
CONFIG_NAMESPACES=y
CONFIG_UTS_NS=y
CONFIG_IPC_NS=y
CONFIG_USER_NS=y
CONFIG_PID_NS=y
CONFIG_NET_NS=y

# Enable seccomp
CONFIG_SECCOMP=y
CONFIG_SECCOMP_FILTER=y

# Enable KSM (Kernel Samepage Merging) for memory dedup
CONFIG_KSM=y

# Enable THP (Transparent Huge Pages)
CONFIG_TRANSPARENT_HUGEPAGE=y

# Enable NUMA (if host has multiple nodes)
CONFIG_NUMA=y

# Enable PCI (for virtio devices)
CONFIG_PCI=y
CONFIG_PCI_MSI=y

# Enable ACPI (for power management)
CONFIG_ACPI=y

# Enable hotplug
CONFIG_MEMORY_HOTPLUG=y
CONFIG_MEMORY_HOTREMOVE=y

# Disable unnecessary debug options to reduce size
CONFIG_DEBUG_FS=n
CONFIG_KALLSYMS_ALL=n
CONFIG_MAGIC_SYSRQ=n

# Enable compressed initramfs support
CONFIG_RD_GZIP=y
CONFIG_RD_BZIP2=y
CONFIG_RD_LZMA=y
CONFIG_RD_XZ=y
CONFIG_RD_LZO=y
CONFIG_RD_LZ4=y
CONFIG_RD_ZSTD=y
EOF

# Update config
make olddefconfig

# Firecracker's x86 legacy transport advertises its virtio devices through
# `virtio_mmio.device=...` kernel arguments. Merely enabling VIRTIO_MMIO is not
# sufficient: without CMDLINE_DEVICES the kernel boots but never discovers the
# root disk or network adapter.
for required_config in \
    CONFIG_SMP=y \
    CONFIG_MODULES=y \
    CONFIG_MULTIUSER=y \
    CONFIG_BINFMT_ELF=y \
    CONFIG_BINFMT_SCRIPT=y \
    CONFIG_HIGH_RES_TIMERS=y \
    CONFIG_POSIX_TIMERS=y \
    CONFIG_FUTEX=y \
    CONFIG_EPOLL=y \
    CONFIG_SIGNALFD=y \
    CONFIG_TIMERFD=y \
    CONFIG_EVENTFD=y \
    CONFIG_SYSVIPC=y \
    CONFIG_SHMEM=y \
    CONFIG_TMPFS=y \
    CONFIG_HYPERVISOR_GUEST=y \
    CONFIG_PARAVIRT=y \
    CONFIG_KVM_GUEST=y \
    CONFIG_PARAVIRT_CLOCK=y \
    CONFIG_BLOCK=y \
    CONFIG_VIRTIO_MENU=y \
    CONFIG_VIRTIO=y \
    CONFIG_VIRTIO_BLK=y \
    CONFIG_VIRTIO_NET=y \
    CONFIG_VIRTIO_MMIO=y \
    CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y \
    CONFIG_UNIX=y \
    CONFIG_EXT4_FS=y \
    CONFIG_PRINTK=y \
    CONFIG_SERIAL_8250_CONSOLE=y \
    CONFIG_DEBUG_INFO=y \
    CONFIG_DEBUG_INFO_BTF=y \
    CONFIG_BPF=y \
    CONFIG_BPF_SYSCALL=y \
    CONFIG_BPF_JIT=y \
    CONFIG_BPF_JIT_ALWAYS_ON=y \
    CONFIG_BPF_EVENTS=y \
    CONFIG_KALLSYMS=y \
    CONFIG_KPROBES=y \
    CONFIG_KPROBE_EVENTS=y \
    CONFIG_FTRACE=y \
    CONFIG_TRACING=y \
    CONFIG_FTRACE_SYSCALLS=y; do
    grep -qx "$required_config" .config || {
        echo "Required Firecracker kernel setting is missing after olddefconfig: $required_config" >&2
        exit 1
    }
done
configured_nr_cpus=$(sed -n 's/^CONFIG_NR_CPUS=//p' .config)
if [[ -z "$configured_nr_cpus" || "$configured_nr_cpus" -lt 2 ]]; then
    echo "Firecracker validation requires CONFIG_NR_CPUS >= 2; got ${configured_nr_cpus:-unset}." >&2
    exit 1
fi

# Build kernel
echo "Building kernel with $JOBS parallel jobs..."
make -j"$JOBS" vmlinux

# Copy output
OUTPUT="$OUTPUT_DIR/vmlinux-$SHORT_VERSION"
cp "vmlinux" "$OUTPUT"
# Remove ordinary DWARF while retaining the compact BTF section needed by the
# guest eBPF tooling. A plain `strip` silently removes BTF as well.
strip --strip-debug --keep-section=.BTF "$OUTPUT"
readelf -S "$OUTPUT" | grep '[.]BTF' >/dev/null || {
    echo "Built kernel is missing its required BTF section." >&2
    exit 1
}
cp .config "$OUTPUT.config"
echo "7" > "$OUTPUT.schema"

echo ""
echo "=== Kernel build complete ==="
echo "Output: $OUTPUT"
echo "Size: $(du -h "$OUTPUT" | cut -f1)"
echo ""
echo "Verify eBPF/BTF support:"
file "$OUTPUT"
readelf -S "$OUTPUT" | grep -i btf
