# MicroVM Testbed for the Wasm Cloud Platform

A Firecracker-based microVM testbed for pre-production validation of the Wasm Cloud Platform. This crate enables running the full platform stack (wasm-node, NATS, eBPF) inside lightweight virtual machines that mirror production conditions.

## What Problem This Solves

Your existing `crates/e2e` tests run `wasm-node` as a **native process** on the host. This is fast and great for developer feedback, but it **cannot validate** several production-critical concerns:

| Production Concern | Native Process Test | MicroVM Test |
|---|---|---|
| **Kernel version compatibility** (5.8+ for eBPF) | Host kernel only | ✅ Any kernel version |
| **eBPF program loading** (`CAP_BPF`, `CAP_SYS_ADMIN`) | Runs with host caps | ✅ Real capability checks |
| **cgroups v2 resource limits** | Host cgroups | ✅ Isolated cgroups |
| **Persistent block storage** (redb on real disk) | Temp files on host FS | ✅ Real block device, reboot persistence |
| **Network namespaces / iptables / tc** | Host network | ✅ Isolated NIC, real netem |
| **systemd service integration** | Not testable | ✅ Full systemd unit test |
| **Package installation** (`.deb`, `.rpm`) | Not testable | ✅ Install and validate |
| **Kernel OOM killer behavior** | Host OOM | ✅ Real memory pressure |
| **Multi-node with real IPs** | 127.0.0.1 + ports | ✅ Real IP addresses |

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Host Machine (Linux with KVM)                    │
│                                                                          │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐          │
│  │  MicroVM Node-1 │  │  MicroVM Node-2 │  │  MicroVM Node-N │          │
│  │  (vCPU, 512MB)  │  │  (vCPU, 512MB)  │  │  (vCPU, 512MB)  │          │
│  │                 │  │                 │  │                 │          │
│  │ ┌─────────────┐ │  │ ┌─────────────┐ │  │ ┌─────────────┐ │          │
│  │ │ wasm-node   │ │  │ │ wasm-node   │ │  │ │ wasm-node   │ │          │
│  │ │ + eBPF      │ │  │ │ + eBPF      │ │  │ │ + eBPF      │ │          │
│  │ │ + redb      │ │  │ │ + redb      │ │  │ │ + redb      │ │          │
│  │ │ + systemd   │ │  │ │ + systemd   │ │  │ │ + systemd   │ │          │
│  │ └─────────────┘ │  │ └─────────────┘ │  │ └─────────────┘ │          │
│  │                 │  │                 │  │                 │          │
│  │  Kernel 6.x     │  │  Kernel 6.x     │  │  Kernel 6.x     │          │
│  │  BTF enabled    │  │  BTF enabled    │  │  BTF enabled    │          │
│  └────────┬────────┘  └────────┬────────┘  └────────┬────────┘          │
│           │                    │                    │                    │
│           └────────────────────┼────────────────────┘                    │
│                                │                                         │
│                         ┌──────▼──────┐                                  │
│                         │   NATS VM   │  (JetStream, cluster mode)       │
│                         │  (2 vCPUs)  │                                  │
│                         └─────────────┘                                  │
│                                                                          │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │  Test Orchestrator (this crate)                                  │    │
│  │  - Spawns VMs via Firecracker API                                │    │
│  │  - Runs existing E2E test suite against VM IPs                   │    │
│  │  - Injects failures (kill VM, network partition, disk corrupt)   │    │
│  │  - Measures TTR across real network                              │    │
│  └─────────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────┘
```

## Quick Start

### Prerequisites

- **Linux** with KVM support (`/dev/kvm` accessible)
- **Rust** 1.80+ with `wasm32-wasip2` target
- **sudo** or `CAP_NET_ADMIN` for network setup
- **Firecracker** binary installed

### 1. Install Firecracker

```bash
./scripts/vm/install-firecracker.sh
```

### 2. Build VM Images

```bash
./scripts/vm/build-all-images.sh
```

This creates:
- `./assets/vmlinux-6.1` — Linux kernel with eBPF/BTF support
- `./assets/wasm-node-rootfs.ext4` — Alpine Linux with wasm-node binary
- `./assets/nats-rootfs.ext4` — Alpine Linux with NATS Server

### 3. Run Tests

```bash
# Single node deploy test
sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture

# Chaos tests (L2 kill + restart, L5 network partition)
sudo cargo test -p vm-testbed --test vm_chaos -- --nocapture --test-threads=1
```

### 4. Use the CLI

```bash
# Spawn a cluster with 3 nodes
cargo run --bin vm-testbed-cli -- spawn-cluster --nodes 3

# Check health
cargo run --bin vm-testbed-cli -- health --ip 172.20.0.2

# Teardown
cargo run --bin vm-testbed-cli -- teardown
```

## Module Overview

| Module | Purpose |
|--------|---------|
| [`firecracker`](src/firecracker.rs) | REST API client for Firecracker microVMM |
| [`network`](src/network.rs) | Host network setup (bridge, TAP, NAT) |
| [`vm`](src/vm.rs) | Single microVM lifecycle management |
| [`cluster`](src/cluster.rs) | Multi-VM cluster orchestration |

## Testing Strategy

This testbed is designed to **complement**, not replace, your existing E2E tests:

```
┌─────────────────────────────────────────────────────────────┐
│                    E2E Test Strategy                         │
├─────────────────────────────────────────────────────────────┤
│  Tier 1: Native Process Tests (crates/e2e)                  │
│  - cargo test -p e2e                                        │
│  - Fast (< 30s per test)                                    │
│  - Developer feedback loop                                  │
│  - CI on every PR                                           │
├─────────────────────────────────────────────────────────────┤
│  Tier 2: MicroVM Installation Tests (this crate)            │
│  - Test actual OS packages, systemd, kernel modules         │
│  - Run nightly or on release                                │
│  - Slower (minutes) but production-realistic                │
└─────────────────────────────────────────────────────────────┘
```

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `FIRECRACKER_PATH` | Path to firecracker binary | Auto-detect |
| `VM_KERNEL_PATH` | Path to vmlinux kernel | `./assets/vmlinux-6.1` |
| `VM_NATS_ROOTFS` | Path to NATS rootfs image | `./assets/nats-rootfs.ext4` |
| `VM_NODE_ROOTFS` | Path to wasm-node rootfs | `./assets/wasm-node-rootfs.ext4` |
| `VM_NODE_DATA_DRIVE` | Optional data drive template | None |

## License

Same as the Wasm Cloud Platform workspace.
