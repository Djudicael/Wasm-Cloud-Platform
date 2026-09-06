# MicroVM Testbed Architecture

This document provides a deep dive into the architecture of the microVM testbed, explaining design decisions and how components interact.

## Design Goals

1. **Production Realism**: The testbed should mirror production conditions as closely as possible
2. **Isolation**: Each test runs in a clean environment with no cross-test contamination
3. **Reproducibility**: Given the same inputs, the testbed produces the same results
4. **Observability**: Every component exposes metrics and logs for debugging
5. **Speed**: Despite using real VMs, the testbed should be fast enough for CI

## Component Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Host Linux Kernel                               │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │                           KVM Module                                   │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  │  │
│  │  │  MicroVM 1  │  │  MicroVM 2  │  │  MicroVM N  │  │  NATS VM    │  │  │
│  │  │  (Firecracker) │  │  (Firecracker) │  │  (Firecracker) │  │  (Firecracker) │  │  │
│  │  │             │  │             │  │             │  │             │  │  │
│  │  │ ┌─────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │  │  │
│  │  │ │ Guest   │ │  │ │ Guest   │ │  │ │ Guest   │ │  │ │ Guest   │ │  │  │
│  │  │ │ Kernel  │ │  │ │ Kernel  │ │  │ │ Kernel  │ │  │ │ Kernel  │ │  │  │
│  │  │ │6.18+BTF │ │  │ │6.18+BTF │ │  │ │6.18+BTF │ │  │ │6.18+BTF │ │  │  │
│  │  │ └────┬────┘ │  │ └────┬────┘ │  │ └────┬────┘ │  │ └────┬────┘ │  │  │
│  │  │      │      │  │      │      │  │      │      │  │      │      │  │  │
│  │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │  │
│  │  │ │ Rootfs  │ │  │ │ Rootfs  │ │  │ │ Rootfs  │ │  │ │ Rootfs  │ │  │  │
│  │  │ │ (ext4)  │ │  │ │ (ext4)  │ │  │ │ (ext4)  │ │  │ │ (ext4)  │ │  │  │
│  │  │ └────┬────┘ │  │ └────┬────┘ │  │ └────┬────┘ │  │ └────┬────┘ │  │  │
│  │  │      │      │  │      │      │  │      │      │  │      │      │  │  │
│  │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │ ┌────▼────┐ │  │  │
│  │  │ │wasm-node│ │  │ │wasm-node│ │  │ │wasm-node│ │  │ │  NATS   │ │  │  │
│  │  │ │+ eBPF   │ │  │ │+ eBPF   │ │  │ │+ eBPF   │ │  │ │ Server  │ │  │  │
│  │  │ │+ redb   │ │  │ │+ redb   │ │  │ │+ redb   │ │  │ │+ JetSt  │ │  │  │
│  │  │ └─────────┘ │  │ └─────────┘ │  │ └─────────┘ │  │ └─────────┘ │  │  │
│  │  └─────────────┘  └─────────────┘  └─────────────┘  └─────────────┘  │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Host Network Stack                           │   │
│  │  ┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐      │   │
│  │  │ br-wasm  │◄──►│tap-node1 │◄──►│tap-node2 │◄──►│tap-nats  │      │   │
│  │  │172.20.0.1│    │          │    │          │    │          │      │   │
│  │  └────┬─────┘    └──────────┘    └──────────┘    └──────────┘      │   │
│  │       │                                                             │   │
│  │  ┌────┴────┐                                                        │   │
│  │  │iptables │  MASQUERADE 172.20.0.0/24                               │   │
│  │  │  NAT    │                                                        │   │
│  │  └────┬────┘                                                        │   │
│  │       │                                                             │   │
│  │  ┌────┴────┐                                                        │   │
│  │  │  eth0   │  Host physical NIC                                      │   │
│  │  └─────────┘                                                        │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Why Firecracker?

We chose Firecracker over alternatives for these reasons:

| Criterion | Firecracker | QEMU | Cloud Hypervisor | Docker |
|-----------|-------------|------|------------------|--------|
| Boot time | < 125ms | 1-5s | 500ms | N/A |
| Memory overhead | ~5MB | ~100MB | ~20MB | N/A |
| Binary size | ~10MB | ~50MB | ~15MB | N/A |
| API | REST + UDS | QMP + CLI | REST | CLI |
| Snapshot support | Yes | Yes | Yes | N/A |
| Production use | AWS Lambda | Wide | Kata Containers | Wide |
| Rust-native | Yes (AWS) | No | Yes (Intel) | No (Go) |

Firecracker's combination of fast boot, small footprint, and REST API makes it ideal for test scenarios where we spawn and destroy VMs frequently.

## Network Design

### Why TAP + Bridge?

Firecracker supports two networking modes:

1. **TAP device + Linux bridge** (what we use)
   - Full Layer 2 connectivity between VMs
   - Host can route/firewall between VMs
   - Supports iptables/tc for chaos testing

2. **IP-based (TAP without bridge)**
   - Simpler setup
   - No inter-VM L2 communication
   - Harder to do network partitions

We chose the bridge approach because network partition testing (L5 chaos) requires manipulating traffic between specific VMs, which is easy with bridge + iptables.

### IP Allocation Strategy

```
172.20.0.0/24
├── 172.20.0.1    br-wasm (host gateway)
├── 172.20.0.2    node-0
├── 172.20.0.3    node-1
├── 172.20.0.4    node-2
├── ...
├── 172.20.0.10   nats-0
├── 172.20.0.11   nats-1 (if clustered)
├── 172.20.0.20   optional PostgreSQL application service
├── 172.20.0.21   optional Vault Transit integration service
└── 172.20.0.255  broadcast
```

This is hardcoded in the testbed for simplicity. For multi-tenant scenarios, you'd use a larger subnet or VLANs.

PostgreSQL and Vault are independent service microVMs and are not included in
the platform-node count. The local PostgreSQL image validates an application's
database path. The local Vault image validates the platform's external
seal-root client and failure behavior. Neither service is deployed or managed
by the Wasm Cloud Platform in production. See
[Local service microVMs](./service-microvms.md) for their state and lifecycle.

### MAC Address Allocation

We use the locally administered address range (`AA:FC:00:00:00:XX`) to avoid conflicts with real hardware:

```
Node 0: AA:FC:00:00:00:01
Node 1: AA:FC:00:00:00:02
Node 2: AA:FC:00:00:00:03
NATS:   AA:FC:00:00:00:10
```

## Rootfs Design

### Why Alpine Linux?

| Distro | Size | Init System | Package Manager | musl libc |
|--------|------|-------------|-----------------|-----------|
| Alpine | ~5MB | openrc | apk | Yes |
| Debian | ~100MB | systemd | apt | No |
| Fedora | ~150MB | systemd | dnf | No |
| Buildroot | ~3MB | custom | None | Optional |

Alpine provides the best balance of small size, package availability, and standard init system. The musl libc is statically linkable, which simplifies binary distribution.

### Why ext4?

Firecracker supports multiple filesystem formats for drives:

| Format | Pros | Cons |
|--------|------|------|
| ext4 | Standard, resizable, journaling | Slightly larger than raw |
| raw | Fastest, simplest | No features |
| qcow2 | Copy-on-write, snapshots | Slower, complex |

We use ext4 because:
- Journaling protects against corruption during VM kills
- `resize2fs` allows growing the filesystem
- Standard tools (`mkfs.ext4`, `e2fsck`) are widely available

### MMDS (Metadata Service)

Firecracker provides a metadata service accessible at `169.254.169.254` inside the VM. We use this to pass configuration without baking it into the rootfs:

```json
{
  "node_config": {
    "node_id": "node-1",
    "nats_url": "nats://172.20.0.10:4222",
    "proxy_port": 8080
  }
}
```

This allows the same rootfs image to be used for multiple nodes with different configurations.

## Kernel Configuration

### Versioned guest kernel

The authoritative pins are in `scripts/vm/kernel-testbed.env`. The current
testbed builds Linux 6.18.48 against Firecracker's checksum-pinned maintained
6.18 guest configuration, applies the platform overlay, and records kernel image
schema 8. `provision-testbed.sh` rejects an asset whose schema marker does not
match. This is a test fixture; production operators select and validate their
own host kernel.

### Critical Kernel Options

These options are **required** for the testbed to work:

```
CONFIG_BPF=y              # eBPF support
CONFIG_BPF_SYSCALL=y      # BPF syscalls
CONFIG_BPF_JIT=y          # JIT compilation for performance
CONFIG_DEBUG_INFO_BTF=y   # BTF type information
CONFIG_VIRTIO_NET=y       # Network device
CONFIG_VIRTIO_BLK=y       # Block device
CONFIG_VIRTIO_PCI=y       # PCI transport for virtio
CONFIG_EXT4_FS=y          # Root filesystem
CONFIG_SERIAL_8250=y      # Serial console
```

These options are **recommended**:

```
CONFIG_CGROUP_BPF=y       # BPF cgroup programs
CONFIG_BPF_LSM=y          # BPF LSM hooks
CONFIG_NAMESPACES=y       # All namespace types
CONFIG_SECCOMP=y          # seccomp filters
CONFIG_USER_NS=y          # User namespaces
```

## Failure Injection Architecture

### L1: Instance Crash

In native E2E tests, we kill a Wasm instance by calling the admin API. In microVMs, instances are Tokio tasks inside the wasm-node process, so the same approach works.

### L2: Node Kill

```
Test:          kill -9 $FIRECRACKER_PID
What happens:  VMM dies immediately, guest kernel and all processes vanish
VM state:      redb file on disk is in unknown state (may be corrupted)
Recovery:      Restart VM, wasm-node detects redb, runs integrity check
Expected TTR:  < 30 seconds
```

### L3: Disk Corruption

```
Test:          dd if=/dev/urandom of=rootfs.ext4 bs=4096 seek=100 count=1
What happens:  One 4KB page of redb is corrupted
VM state:      redb checksum fails on next access
Recovery:      wasm-node detects corruption, rebuilds from NATS JetStream
Expected TTR:  < 10 seconds
```

### L4: Full Node Rebuild

```
Test:          rm rootfs.ext4 && recreate from
What happens:  The node loses its local state and cached artifacts
VM state:      A fresh node image boots with the recorded topology identity
Recovery:      Bootstrap from the durable control plane and authorized artifact sources
Expected TTR:  Measure and record it for the tested topology
```

Failure injection must target the exact VM, network resource, and state file
created for the test. Use the canonical scripts and validators; never select
processes, TAP devices, bridges, or rootfs files by broad pattern matching.
