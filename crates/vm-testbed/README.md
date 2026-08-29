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
- **Rust** pinned by the repository's `rust-toolchain.toml`, with the
  `wasm32-wasip2` target
- **sudo** or `CAP_NET_ADMIN` for network setup
- **Firecracker** binary installed
- **Image/service tooling**: `curl`, `debugfs`, `jq`, `mkfs.ext4`, `openssl`,
  `python3`, `unzip`, and standard mount utilities

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
- `./assets/postgres-rootfs.ext4` — optional application database fixture
- `./assets/vault-rootfs.ext4` — optional initialized Vault Transit fixture
- `./assets/vault-test-ca.crt` — private CA for the local Vault fixture

PostgreSQL and Vault are service microVMs, not platform nodes. PostgreSQL is
only an application dependency, while Vault exercises one supported external
seal-root integration. The aggregate builder creates a protected Vault
bootstrap outside the repository; the initialized Vault rootfs and bootstrap
must never be published or promoted to production. See the
[service microVM guide](../../docs/vm-testbed/service-microvms.md) for lifecycle,
state, credential, and teardown details.

### 3. Run Tests

```bash
# Single node deploy test
sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture

# Chaos tests (L2 kill + restart, L5 network partition)
sudo cargo test -p vm-testbed --test vm_chaos -- --nocapture --test-threads=1
```

### 4. Run a complete local lifecycle

The repository scripts provide the same stable entrypoints used by the agent skills:

```bash
bash scripts/vm/provision-testbed.sh --profile single-node
bash scripts/vm/deploy-test-application.sh
bash scripts/vm/destroy-testbed.sh
```

Pass the same `--state-file` value to all three scripts when using a non-default state path.

For a production-like local edge topology, choose the platform-node count explicitly:

```bash
bash scripts/vm/provision-testbed.sh --preset production-like --nodes 3
```

This starts one NATS microVM plus three platform microVMs, each with its built-in reverse proxy, and a host HAProxy front door at `127.0.0.1:8088`. Override the listener with `--front-door-bind HOST:PORT`, or disable it with `--front-door none`. HAProxy must already be installed in Linux/WSL. Deployment verification automatically uses the recorded front door, and the destroy script stops that exact process.

The preset is a production-like rehearsal rather than a production deployment: it does not provide production TLS, an external secrets backend, monitoring/alerting, or highly available NATS.

Optional PostgreSQL, Vault Transit, and observability fixtures can be attached
to the recorded topology for integration testing. They do not make those
external services production-ready and are not installed by the platform in
production. Follow the [service microVM guide](../../docs/vm-testbed/service-microvms.md).

### 5. Use the CLI directly

```bash
# Bring up a detached single-node manual topology
cargo run --bin vm-testbed-cli -- up --profile single-node --name dev-lab

# Show running state from the saved file
cargo run --bin vm-testbed-cli -- status

# Add one more node using the saved defaults
cargo run --bin vm-testbed-cli -- add-node

# Scale the cluster to 4 wasm-node VMs
cargo run --bin vm-testbed-cli -- scale --nodes 4

# Remove one node by id
cargo run --bin vm-testbed-cli -- remove-node --id dev-lab-node-3

# Deploy a Wasm app and publish a route
cargo run --bin vm-testbed-cli -- deploy-app \
  --app hello-axum \
  --version v1 \
  --wasm target/wasm32-wasip2/release/hello_axum.wasm \
  --route-host hello.local

# Remove the app again
cargo run --bin vm-testbed-cli -- undeploy-app --app-id default:hello-axum:v1

# Tear it down later
cargo run --bin vm-testbed-cli -- down
```

Available profiles in this first CLI pass:

- `single-node`
- `multi-node`
- `chaos-ready`

The CLI now persists cluster state to `.vm-testbed-state.json` by default so you can bring a topology up, validate it manually, and destroy it later.

## Manual Operator Workflow

The CLI is intended to cover the manipulations you expect from a production-style lab:

- topology lifecycle:
  - `up`
  - `status`
  - `down`
- node lifecycle:
  - `add-node`
  - `remove-node`
  - `scale`
  - `kill`
- application lifecycle:
  - `deploy-app`
  - `undeploy-app`
- environment preparation:
  - `assets install-firecracker`
  - `assets build-kernel`
  - `assets build-node-rootfs`
  - `assets build-nats-rootfs`
  - `assets build-all-images`
  - `assets setup-network`

The cluster state file stores:

- topology name and profile
- bridge and subnet details
- NATS endpoint
- kernel and rootfs paths
- default node memory and vCPU settings
- every detached VM id, pid, IP, admin endpoint, artifact endpoint, and proxy endpoint

That state is what allows later commands to add nodes, remove nodes, deploy apps, or tear the lab down without keeping the original `up` process attached.

## Common CLI Flows

### Bring up a lab

```bash
cargo run --bin vm-testbed-cli -- up --profile multi-node --name staging-lab --nodes 3
```

### Inspect the lab

```bash
cargo run --bin vm-testbed-cli -- status
```

### Add or remove capacity

```bash
# Add one node with saved defaults
cargo run --bin vm-testbed-cli -- add-node

# Add one larger node
cargo run --bin vm-testbed-cli -- add-node --memory 1024 --vcpus 4

# Scale the topology to 5 nodes
cargo run --bin vm-testbed-cli -- scale --nodes 5

# Remove a specific node
cargo run --bin vm-testbed-cli -- remove-node --id staging-lab-node-4
```

### Deploy an application

```bash
cargo run --bin vm-testbed-cli -- deploy-app \
  --app hello-axum \
  --version v1 \
  --namespace default \
  --wasm target/wasm32-wasip2/release/hello_axum.wasm \
  --route-host hello.local \
  --env RUST_LOG=info \
  --secret DATABASE_URL
```

`deploy-app` will:

- upload the artifact to a selected node
- query `/admin/cluster/nodes`
- authorize peer manifests for active nodes
- publish `DeployApp` on NATS
- optionally add a route

Use `--target-node <node-id>` if you want to force the upload source node instead of using the first node in the detached state.

### Remove an application

```bash
cargo run --bin vm-testbed-cli -- undeploy-app --app-id default:hello-axum:v1
```

### Tear down the lab

```bash
cargo run --bin vm-testbed-cli -- down
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
