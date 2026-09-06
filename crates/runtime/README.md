# Runtime Crate

## Overview

This crate provides a Wasmtime-based WebAssembly runtime for the Wasm Cloud Platform. It is responsible for the complete lifecycle of WebAssembly execution, from compilation to instance management:

- **Compilation**: Compiling raw `.wasm` binaries into AOT (ahead-of-time) artifacts for fast startup
- **Preparation**: Preparing stored artifacts for execution
- **Instance Spawning**: Spawning Wasm instances with WASI Preview 2 support
- **Resource Management**: Fuel metering, memory limits, and I/O resource tracking
- **Policy Enforcement**: Controlling network access, filesystem access, and other capabilities
- **DNS**: Virtual DNS resolution for Wasm instances

## Architecture

The runtime is built on top of [Wasmtime](https://wasmtime.dev/) and provides several layers of abstraction:

1. **Engine Layer** (`build_engine`): Configures and builds the Wasmtime `Engine` with appropriate settings for AOT compilation and WASI support.

2. **Module Preparation** (`PreparedModule`): Handles the compilation and caching of Wasm modules, making them ready for instantiation.

3. **Instance Management** (`RunningInstance`): Manages the lifecycle of running Wasm instances, including store state and execution tracking.

4. **Policy Layer** (`PolicyEnforcer`, `PolicyCounters`): Enforces security policies on Wasm instances, including network access control via CIDR-based rules and connection counting.

5. **Resource Tracking** (`MemoryLimiter`, `IoResourceTracker`): Tracks and limits memory consumption and I/O resources used by Wasm instances.

6. **Virtual Networking** (`VirtualDns`): Provides DNS resolution capabilities within the Wasm environment.

## Public API

### Core Types

| Type | Description |
|------|-------------|
| `WasmRuntime` | Main entry point for the Wasm runtime, managing engine configuration and module preparation |
| `PreparedModule` | A compiled Wasm module ready for instantiation |
| `RunningInstance` | A running Wasm instance with associated state and resources |
| `ExecutionStats` | Statistics about Wasm execution (fuel consumed, time elapsed, etc.) |
| `StoreState` | Internal state maintained within the Wasmtime store |

### Policy & Security

| Type | Description |
|------|-------------|
| `PolicyEnforcer` | Enforces security policies on Wasm instances |
| `PolicyCounters` | Tracks policy-related counters (active connections, etc.) |
| `VirtualDns` | Virtual DNS resolution for Wasm instances |

### Resource Management

| Type | Description |
|------|-------------|
| `MemoryLimiter` | Limits memory allocation for Wasm instances |
| `IoResourceTracker` | Tracks I/O resources used by Wasm instances |

## Sustained Load Review

The runtime now includes a repeatable Wasmtime load-review path for WSL/Linux:

- probe: `cargo run -p runtime --example wasmtime_load_probe -- --scenario baseline`
- full review: `bash scripts/run_wasmtime_load_review.sh`

The review uses the real `hello-axum` component, compares baseline vs cache vs pooling allocator scenarios, and records:

- cold vs warm compile latency
- repeated instantiation latency
- peak-live instance spawn latency
- process RSS after compile and after holding multiple live instances

The production template keeps pooling disabled by default. Enable it only if this review shows a material instantiation gain without unacceptable RSS growth for the workload you actually expect to run.

This review is the current evidence path for startup and instantiation latency. The repository does not currently publish a single end-to-end platform benchmark that substantiates a universal `sub-10ms cold start` claim across workloads and deployment shapes.

## Known Issues & Improvements

The connection limit is reserved with compare-and-swap, disconnect uses underflow protection, and instance teardown resets or releases its active reservations. Remaining limitations are:

- **`VirtualDns` is not the WASI resolver**: The type provides namespace-aware name mapping, while guest DNS uses Wasmtime's resolver subject to `allow_dns` and socket-address policy.
- **Filesystem activity accounting is incomplete**: Allowed paths are enforced with WASI preopens and the resource table is capped by `max_open_fds`, but per-operation file write accounting is not connected to wrapped WASI host calls.

### Removed Legacy Paths

The old `policy_wasi`, `ChannelPipe`, `MetadataInjectingStream`, and `wasi.rs` paths have been removed. `IoResourceTracker` remains as the I/O snapshot returned in execution statistics.

### Design Issues

- **WASI interface versions are enumerated explicitly**: Detection lists supported `wasi:cli/run` and `wasi:http/incoming-handler` 0.2 versions, so another interface version requires a code update.
- **Policy counters are runtime-local data**: Running CLI and HTTP instances expose their counters, but this crate does not itself publish them to Prometheus.

## Security Considerations

### Filesystem Access

Only paths listed in `policy.filesystem.allowed_paths` are preopened. With an empty list the guest receives no host-directory preopen. Directory and file permissions are derived from the create/delete policy flags, and a missing configured path causes instance preparation to fail. The remaining limitation is that the current WASI integration does not feed every file write into `max_fs_write_bytes` accounting.

### Network Policy Enforcement

Wasmtime networking is enabled at the context level so permitted applications can use sockets, then constrained with protocol flags and the composed socket-address callback. The callback applies bind, CIDR, DNS, and connection-limit policy.

### CIDR Configuration

Normal `PolicyConfig::resolve()` validation rejects invalid CIDRs before instance creation. The runtime's lower-level parsers also warn and omit an invalid entry if a caller constructs an `InstancePolicy` directly and bypasses that resolution path.

### DNS Resolution

Guest name lookup uses Wasmtime's DNS path when `allow_dns` permits it and still passes resolved addresses through socket policy. `VirtualDns` is a separate mapping utility and is not installed as the guest's resolver.
