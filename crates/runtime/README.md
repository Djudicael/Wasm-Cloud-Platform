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

## Known Issues & Improvements

### Concurrency Bugs

- **TOCTOU race condition in `check_outbound_tcp_connect`**: The current implementation uses load-then-compare instead of a compare-and-swap (CAS) operation, which can lead to race conditions where multiple threads pass the check simultaneously.
- **`outbound_connections_active` can underflow**: Using `fetch_sub` causes the counter to wrap to `u32::MAX` if decremented below zero, leading to incorrect connection tracking.

### Security Issues

- **No preopened directories**: Wasm modules can access the entire host filesystem instead of being restricted to specific directories.
- **`inherit_network()` gives full network access**: Full network access is granted before policy filtering is applied, creating a window where policies can be bypassed.
- **CIDR strings re-parsed on every check**: CIDR strings should be parsed once at configuration time rather than on every policy check (performance and correctness concern).
- **Invalid CIDR strings silently skipped**: Typos in CIDR configuration result in no blocking rather than an error, making misconfiguration hard to detect.
- **`VirtualDns` not integrated into WASI DNS resolution**: The virtual DNS implementation is not connected to the actual WASI DNS resolution layer.

### Dead Code

- **`policy_wasi` module**: All 12 functions are orphans — never called by the WASI layer.
- **`ChannelPipe` and `MetadataInjectingStream`**: Dead code with no callers.
- **`wasi.rs`**: Dead code with an outdated API.
- **`IoResourceTracker`**: Dead code, superseded by `PolicyEnforcer`.

### Design Issues

- **`build_engine()` uses `expect()`**: Panics on configuration errors instead of returning a `Result`.
- **Hardcoded WASI version list**: Must be manually maintained when new WASI versions are supported.
- **`spawn_instance` returns `(RunningInstance, ())`**: The meaningless second element should be removed or replaced with useful data.
- **`PreparedModule` owns Engine instead of `Arc<Engine>`**: Cloning the engine is expensive; it should be shared via `Arc`.
- **`RunningInstance` has no Drop impl**: Policy counters are not cleaned up when an instance is dropped, leading to resource leaks.
- **`PolicyCounters` not connected to Prometheus metrics pipeline**: Counters are tracked but not exported for monitoring.

## Security Considerations

### Filesystem Access

The lack of preopened directory support means Wasm modules have unrestricted access to the host filesystem. This is a critical security vulnerability that should be addressed before production deployment. Implement WASI preopened directories to restrict filesystem access to only what each module requires.

### Network Policy Enforcement

The current network policy enforcement has several gaps:

1. **`inherit_network()` bypass**: Full network access is granted before policies are applied, creating a potential window for policy bypass.
2. **TOCTOU in connection tracking**: Race conditions in `check_outbound_tcp_connect` could allow connection limits to be exceeded.
3. **Counter underflow**: The `outbound_connections_active` underflow issue could be exploited to make the system think fewer connections are active than reality.

### CIDR Configuration

Invalid CIDR strings are silently ignored rather than causing errors. This means a typo in a security-critical CIDR block (e.g., blocking access to internal services) could result in the block not being applied. The system should fail fast on invalid CIDR configuration.

### DNS Resolution

`VirtualDns` is not integrated into the WASI DNS resolution layer, meaning Wasm modules may bypass virtual DNS rules by using the host's DNS resolver directly.
