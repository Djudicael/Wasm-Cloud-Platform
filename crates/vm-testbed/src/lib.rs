//! # MicroVM Testbed for the Wasm Cloud Platform
//!
//! This crate provides a Firecracker-based microVM testbed for pre-production
//! validation of the Wasm Cloud Platform. It enables running the full platform
//! stack (wasm-node, NATS, eBPF) inside lightweight virtual machines that mirror
//! production conditions.
//!
//! ## What This Enables
//!
//! | Capability | Native E2E Tests | MicroVM Testbed |
//! |------------|-----------------|-----------------|
//! | Fast developer feedback | ✅ < 30s | ❌ Minutes |
//! | Real kernel isolation | ❌ Host kernel | ✅ Any kernel |
//! | eBPF loading validation | ❌ Host caps | ✅ Real `CAP_BPF` |
//! | Persistent storage testing | ❌ Temp files | ✅ Real block device |
//! | Network partition (L5) | ❌ Host network | ✅ Isolated NICs |
//! | Power loss simulation | ❌ `kill -9` | ✅ VM kill |
//! | Multi-node clustering | ❌ Port collisions | ✅ Real IPs |
//! | systemd integration | ❌ Not testable | ✅ Full service test |
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Host Machine (Linux with KVM)                    │
//! │                                                                          │
//! │  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐          │
//! │  │  MicroVM Node-1 │  │  MicroVM Node-2 │  │  MicroVM Node-N │          │
//! │  │  (vCPU, 512MB)  │  │  (vCPU, 512MB)  │  │  (vCPU, 512MB)  │          │
//! │  │                 │  │                 │  │                 │          │
//! │  │ ┌─────────────┐ │  │ ┌─────────────┐ │  │ ┌─────────────┐ │          │
//! │  │ │ wasm-node   │ │  │ │ wasm-node   │ │  │ │ wasm-node   │ │          │
//! │  │ │ + eBPF      │ │  │ │ + eBPF      │ │  │ │ + eBPF      │ │          │
//! │  │ │ + redb      │ │  │ │ + redb      │ │  │ │ + redb      │ │          │
//! │  │ │ + systemd   │ │  │ │ + systemd   │ │  │ │ + systemd   │ │          │
//! │  │ └─────────────┘ │  │ └─────────────┘ │  │ └─────────────┘ │          │
//! │  │                 │  │                 │  │                 │          │
//! │  │  Kernel 6.x     │  │  Kernel 6.x     │  │  Kernel 6.x     │          │
//! │  │  BTF enabled    │  │  BTF enabled    │  │  BTF enabled    │          │
//! │  └────────┬────────┘  └────────┬────────┘  └────────┬────────┘          │
//! │           │                    │                    │                    │
//! │           └────────────────────┼────────────────────┘                    │
//! │                                │                                         │
//! │                         ┌──────▼──────┐                                  │
//! │                         │   NATS VM   │  (JetStream, cluster mode)       │
//! │                         │  (2 vCPUs)  │                                  │
//! │                         └─────────────┘                                  │
//! │                                                                          │
//! │  ┌─────────────────────────────────────────────────────────────────┐    │
//! │  │  Test Orchestrator (this crate)                                  │    │
//! │  │  - Spawns VMs via Firecracker API                                │    │
//! │  │  - Runs existing E2E test suite against VM IPs                   │    │
//! │  │  - Injects failures (kill VM, network partition, disk corrupt)   │    │
//! │  │  - Measures TTR across real network                              │    │
//! │  └─────────────────────────────────────────────────────────────────┘    │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Quick Start
//!
//! ```bash
//! # 1. Install Firecracker
//! ./scripts/vm/install-firecracker.sh
//!
//! # 2. Build VM images
//! ./scripts/vm/build-all-images.sh
//!
//! # 3. Run a single-node test
//! cargo test -p vm-testbed test_single_node_deploy -- --nocapture
//!
//! # 4. Run a 3-node cluster chaos test
//! sudo cargo test -p vm-testbed test_three_node_chaos -- --nocapture
//! ```
//!
//! ## Modules
//!
//! - [`firecracker`] — REST API client for the Firecracker microVMM
//! - [`network`] — Host network setup (bridge, TAP, NAT)
//! - [`vm`] — Single microVM lifecycle management
//! - [`cluster`] — Multi-VM cluster orchestration
//!
//! ## Privilege Requirements
//!
//! - **TAP creation**: `CAP_NET_ADMIN` (or `sudo`)
//! - **KVM access**: `/dev/kvm` read/write (usually `kvm` group)
//! - **IP forwarding**: `sysctl net.ipv4.ip_forward=1`
//!
//! ## Environment Variables
//!
//! | Variable | Purpose | Default |
//! |----------|---------|---------|
//! | `FIRECRACKER_PATH` | Path to firecracker binary | Auto-detect |
//! | `VM_KERNEL_PATH` | Path to vmlinux kernel | `./assets/vmlinux-6.1` |
//! | `VM_NATS_ROOTFS` | Path to NATS rootfs image | `./assets/nats-rootfs.ext4` |
//! | `VM_NODE_ROOTFS` | Path to wasm-node rootfs | `./assets/wasm-node-rootfs.ext4` |
//! | `VM_NODE_DATA_DRIVE` | Optional data drive template | None |

pub mod cluster;
pub mod firecracker;
pub mod network;
pub mod vm;

// Re-export primary types for convenience
pub use cluster::{ClusterError, ClusterFixture};
pub use firecracker::FirecrackerClient;
pub use network::{setup_network, teardown_network};
pub use vm::{MicroVm, VmConfig, VmError};
