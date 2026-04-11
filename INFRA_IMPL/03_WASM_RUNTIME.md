# Step 03 — Wasm Runtime (Wasmtime & WASI Preview 2 Integration)

## Goal
Build the `runtime` crate that wraps `wasmtime`. It handles:
1. **AOT compilation** of raw `.wasm` bytes → native machine code artifact

---

## Context & Rationale

### The Problem This Solves

A Wasm binary (`.wasm` file) is bytecode — a portable instruction format that must be
translated to native machine code before it can run. There are two approaches:

- **JIT (Just-In-Time)**: Translate at the moment the module is instantiated. Fast to start
  the first time, but slow to execute until the JIT warms up. Also: every cold start pays
  the compilation cost.
- **AOT (Ahead-of-Time)**: Translate once at deploy time. Store the native artifact. Every
  subsequent instantiation just loads the pre-compiled binary. Zero compilation cost at runtime.

This platform uses **AOT via Cranelift (Wasmtime Default)**. The cold start target is < 10ms. JIT compilation
of a typical Axum app takes 500ms–2s. That alone would make the target impossible.

### Why Cranelift (Wasmtime Default) as the AOT Backend?

Wasmtime supports multiple compilers: Cranelift (Wasmtime Default), LLVM, and Singlepass.

| Backend    | Compile time | Code quality        | Notes                            |
| ---------- | ------------ | ------------------- | -------------------------------- |
| Cranelift (Wasmtime Default)  | ~200ms       | Within 5% of LLVM   | Best balance for servers         |
| LLVM       | 2–30 seconds | Fastest runtime     | Too slow to compile at deploy    |
| Singlepass | ~50ms        | 2–3x slower runtime | Designed for blockchain/metering |

Cranelift (Wasmtime Default) was purpose-built for JIT/AOT in language runtimes (it powers Wasmtime and Firefox).
It produces excellent machine code quickly and its serialization format is stable.

### The Compile-Once, Run-Many Flow

```
Deploy time (once):                Runtime (every cold start):
                                   
.wasm bytes                        redb artifact bytes
     │                                     │
     ▼                                     ▼
Component::new(engine, bytes)         Component::deserialize(engine, bytes)
  (Cranelift (Wasmtime Default) compiles to           (< 1ms — just maps bytes to memory)
   native machine code)                    │
     │                                     ▼
     ▼                             Instance::new(store, module, imports)
Module::serialize()                (links WASI imports, initializes globals)
     │                                     │
     ▼                                     ▼
Store in redb [artifacts]          _start() called → Axum server starts
```

The key insight: `Component::deserialize()` is **not re-compilation**. It is loading a
pre-compiled native binary from bytes into memory. This is the same operation as loading
a shared library (`.so`) — nearly instantaneous.

### Why Fuel Metering (and Not cgroups or Wall-Clock Limits)?

**cgroups CPU limits** (`cpu.max` in cgroupsv2) limit the percentage of wall-clock time
a process can use. For a multi-tenant system this has two problems:

1. **Shared process**: Multiple Wasm instances run in the same OS process. cgroups limits
   apply to the whole process, not individual Wasm modules.
2. **Not deterministic**: A program that does 1 billion integer multiplications takes
   different wall-clock time depending on whether the CPU is idle or loaded.

**Fuel** is different: it counts Wasm instructions executed. 1 billion instructions always
consumes exactly 1 billion fuel, regardless of CPU speed, system load, or time of day.
This is the right primitive for **fair billing and tenant isolation** — a tenant pays for
computation, not for waiting.

### Memory Limit Challenge: Why cgroups Is Not Enough

cgroups `memory.max` limits the **total process memory**. But in a shared process with 50
Wasm instances, setting `memory.max = 100MB` caps all 50 instances combined. There is no
per-Wasm-instance memory limit in cgroups.

The `LimitedTunables` struct in this step implements **per-instance memory limits** at the
Wasm level. It intercepts `memory.grow` calls and refuses growth beyond the configured limit.
This is the correct approach: isolation at the Wasm boundary, not the OS boundary.

### Engine Sharing (Why One Engine per Process)

A Wasmtime `Engine` holds the Cranelift (Wasmtime Default) compiler state and a cache of compiled module
metadata. Creating an engine is expensive (tens of milliseconds). Sharing one `Arc<Engine>`
across all instantiations means:

- Compiled artifacts from the same engine are always compatible
- Thread-safe: multiple Supervisors can instantiate modules concurrently from the same engine
- `spawn_blocking` is needed for `compile()` (CPU-bound) but **not** for `deserialize()`

---
2. **Serialization** of the artifact for storage in `redb`
3. **Instantiation** of a compiled module with fuel + memory limits
4. **Execution** of the Wasm module (running the Axum server inside it)
5. **Resource extraction** after execution (fuel consumed, RAM used, wall-clock time)

---

## 1. Compiler Module

Compiles `.wasm` bytecode to native machine code using Cranelift (Wasmtime Default) (AOT).
The result is a serialized artifact that can be stored and later deserialized without re-compilation.

```rust
// crates/runtime/src/compiler.rs
use wasmtime::{Engine, Module, Store};
use wasmtime_compiler_cranelift::Cranelift (Wasmtime Default);
use common::error::PlatformError;

/// Build a Cranelift (Wasmtime Default)-based AOT engine.
/// Call once per process and share via Arc.
pub fn build_engine() -> Engine {
    let compiler = Cranelift (Wasmtime Default)::default();
    Engine::new(compiler.into(), Default::default())
}

/// Compile raw `.wasm` bytes into a native artifact.
/// This is CPU-intensive — run on a blocking thread (tokio::task::spawn_blocking).
///
/// Returns: serialized artifact bytes (store in redb).
pub fn compile(engine: &Engine, wasm_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
    let module = Component::new(engine, wasm_bytes)
        .map_err(|e| PlatformError::Runtime(format!("compile error: {e}")))?;

    // Serialize the compiled module to bytes (portable Artifact format).
    let artifact = module.serialize()
        .map_err(|e| PlatformError::Runtime(format!("serialize error: {e}")))?;

    Ok(artifact.to_vec())
}

/// Deserialize a stored artifact back to a Module.
/// This is near-instant (<1ms for most apps).
///
/// # Safety
/// `artifact_bytes` must be produced by `compile()` with a compatible engine.
pub unsafe fn deserialize(engine: &Engine, artifact_bytes: &[u8]) -> Result<Module, PlatformError> {
    Component::deserialize(engine, artifact_bytes)
        .map_err(|e| PlatformError::Runtime(format!("deserialize error: {e}")))
}
```

---

## 2. Resource Limits Module

Configures per-instance fuel and memory limits using Wasmtime Tunables.

```rust
// crates/runtime/src/limits.rs
use wasmtime::{Pages, Store};
use common::types::{FuelQuota, MemoryPages};
use common::error::PlatformError;

/// Apply resource limits to a Store before creating an Instance.
pub fn configure_store(
    store: &mut Store,
    fuel: FuelQuota,
    memory: MemoryPages,
) -> Result<(), PlatformError> {
    // Set fuel limit (CPU metering).
    // Every Wasm instruction decrements this counter.
    store.set_fuel(fuel.0)
        .map_err(|e| PlatformError::Runtime(format!("fuel error: {e}")))?;

    // Memory limit is enforced at the engine level via custom Tunables.
    // This prevents the Wasm module from growing its linear memory beyond the limit.
    // Implementation requires a custom Tunables struct wrapping BaseTunables.
    // See: https://docs.rs/wasmtime/latest/wasmtime/trait.Tunables.html
    //
    // For the initial implementation, use wasmtime's built-in limit API:
    store.set_trap_on_out_of_fuel(true); // raise Trap instead of returning error

    tracing::debug!(
        fuel = fuel.0,
        memory_pages = memory.0,
        memory_bytes = memory.to_bytes(),
        "store limits configured"
    );
    Ok(())
}

/// Read how much fuel remains after execution.
pub fn read_fuel_remaining(store: &Store) -> u64 {
    store.get_fuel().unwrap_or(0)
}
```

### Custom Tunables (Memory Limit Enforcement)

```rust
// crates/runtime/src/limits.rs (continued)
use wasmtime::{BaseTunables, Pages, Target, Tunables, MemoryError, MemoryStyle, TableStyle};
use wasmtime::vm::{VMMemoryDefinition, VMTableDefinition};
use std::ptr::NonNull;

/// Wraps BaseTunables to enforce a maximum linear memory size.
pub struct LimitedTunables {
    limit: Pages,
    base: BaseTunables,
}

impl LimitedTunables {
    pub fn new(limit: MemoryPages) -> Self {
        LimitedTunables {
            limit: Pages(limit.0),
            base: BaseTunables::for_target(&Target::default()),
        }
    }
}

impl Tunables for LimitedTunables {
    fn memory_style(&self, memory: &wasmtime::MemoryType) -> MemoryStyle {
        // Clamp max pages to our limit
        let adjusted = wasmtime::MemoryType::new(
            memory.minimum,
            Some(memory.maximum.unwrap_or(self.limit).min(self.limit)),
            memory.shared,
        );
        self.base.memory_style(&adjusted)
    }

    fn table_style(&self, table: &wasmtime::TableType) -> TableStyle {
        self.base.table_style(table)
    }

    fn create_host_memory(
        &self,
        ty: &wasmtime::MemoryType,
        style: &MemoryStyle,
    ) -> Result<wasmtime::VMMemory, MemoryError> {
        if ty.minimum > self.limit {
            return Err(MemoryError::Generic(format!(
                "memory minimum {} exceeds limit {}",
                ty.minimum.0, self.limit.0
            )));
        }
        self.base.create_host_memory(ty, style)
    }

    unsafe fn create_vm_memory(
        &self,
        ty: &wasmtime::MemoryType,
        style: &MemoryStyle,
        vm_definition_location: NonNull<VMMemoryDefinition>,
    ) -> Result<wasmtime::VMMemory, MemoryError> {
        self.base.create_vm_memory(ty, style, vm_definition_location)
    }

    unsafe fn create_vm_table(
        &self,
        ty: &wasmtime::TableType,
        style: &TableStyle,
        vm_definition_location: NonNull<VMTableDefinition>,
    ) -> Result<wasmtime::VMTable, wasmtime::TableError> {
        self.base.create_vm_table(ty, style, vm_definition_location)
    }

    fn create_host_table(
        &self,
        ty: &wasmtime::TableType,
        style: &TableStyle,
    ) -> Result<wasmtime::VMTable, wasmtime::TableError> {
        self.base.create_host_table(ty, style)
    }
}
```

---

## 3. Executor Module

Instantiates a module and runs it. Captures resource usage after the run.

```rust
// crates/runtime/src/executor.rs
use wasmtime::{Engine, Instance, Module, Store, imports};
use wasmtime_wasix::{WasiEnv, WasiEnvBuilder};
use common::{
    error::PlatformError,
    types::{AppConfig, FuelQuota, InstanceId},
};
use std::time::Instant;
use crate::limits::{configure_store, read_fuel_remaining, LimitedTunables};

/// Result of a single Wasm execution.
#[derive(Debug)]
pub struct ExecutionStats {
    pub instance_id: InstanceId,
    pub fuel_limit: u64,
    pub fuel_consumed: u64,
    pub ram_bytes: usize,
    pub wall_clock_ms: u64,
    pub trap: Option<String>,
}

/// A prepared, AOT-compiled module ready for repeated instantiation.
pub struct PreparedModule {
    pub engine: Engine,
    pub module: Module,
    pub config: AppConfig,
}

impl PreparedModule {
    /// Build from a deserialized artifact + app config.
    pub fn from_artifact(
        artifact_bytes: &[u8],
        config: AppConfig,
    ) -> Result<Self, PlatformError> {
        let engine = crate::compiler::build_engine();
        // SAFETY: artifact was produced by our own compiler::compile()
        let module = unsafe { crate::compiler::deserialize(&engine, artifact_bytes) }?;
        Ok(PreparedModule { engine, module, config })
    }

    /// Instantiate and run the module.
    /// The Wasm module starts its own async Tokio runtime internally and
    /// binds to the port injected by the Supervisor.
    pub fn spawn_instance(
        &self,
        env_vars: Vec<(String, String)>,
        port: u16,
    ) -> Result<RunningInstance, PlatformError> {
        let id = InstanceId::new();
        let mut store = Store::new(self.engine.clone());

        // Apply limits
        configure_store(&mut store, self.config.fuel_quota, self.config.memory_limit)?;

        // Build WASI environment with injected env vars + port
        let mut builder = WasiEnv::builder(self.config.id.0.as_str());
        for (k, v) in &env_vars {
            builder = builder.env(k, v);
        }
        // The app will bind to 0.0.0.0:<port>; the Supervisor maps this port.
        builder = builder.env("PORT", &port.to_string());

        let wasi_env = builder
            .finalize(&mut store)
            .map_err(|e| PlatformError::Runtime(format!("WASI init error: {e}")))?;

        let import_object = wasi_env.import_object(&mut store, &self.module)
            .map_err(|e| PlatformError::Runtime(format!("import object error: {e}")))?;

        let instance = Instance::new(&mut store, &self.module, &import_object)
            .map_err(|e| PlatformError::Runtime(format!("instantiation error: {e}")))?;

        // Initialize WASI (required before calling _start)
        wasi_env.initialize(&mut store, instance.clone())
            .map_err(|e| PlatformError::Runtime(format!("WASI initialize error: {e}")))?;

        Ok(RunningInstance {
            id,
            instance,
            store,
            config: self.config.clone(),
            started_at: Instant::now(),
        })
    }
}

/// An instantiated, running Wasm module.
pub struct RunningInstance {
    pub id: InstanceId,
    instance: Instance,
    store: Store,
    config: AppConfig,
    started_at: Instant,
}

impl RunningInstance {
    /// Call `_start` (the WASI entry point). This blocks until the Wasm app exits.
    /// For a server like Axum, this runs indefinitely until the Supervisor kills it.
    pub fn run(&mut self) -> ExecutionStats {
        let fuel_limit = self.config.fuel_quota.0;
        let start = Instant::now();
        let mut trap_msg = None;

        let start_fn = self.instance.exports.get_function("_start");
        match start_fn {
            Ok(f) => {
                if let Err(e) = f.call(&mut self.store, &[]) {
                    trap_msg = Some(e.to_string());
                }
            }
            Err(e) => {
                trap_msg = Some(format!("export not found: {e}"));
            }
        }

        let fuel_remaining = read_fuel_remaining(&self.store);
        let fuel_consumed = fuel_limit.saturating_sub(fuel_remaining);
        let ram_bytes = self.read_memory_usage();
        let wall_clock_ms = start.elapsed().as_millis() as u64;

        ExecutionStats {
            instance_id: self.id.clone(),
            fuel_limit,
            fuel_consumed,
            ram_bytes,
            wall_clock_ms,
            trap: trap_msg,
        }
    }

    fn read_memory_usage(&self) -> usize {
        // Get the Wasm linear memory size
        if let Ok(mem) = self.instance.exports.get_memory("memory") {
            mem.view(&self.store).data_size()
        } else {
            0
        }
    }
}
```

---

## 4. The Runtime Facade

Public API of the `runtime` crate, used by the Supervisor.

```rust
// crates/runtime/src/lib.rs
pub mod compiler;
pub mod executor;
pub mod limits;

use common::{error::PlatformError, types::AppConfig};
use executor::PreparedModule;
use wasmtime::Engine;
use std::sync::Arc;

/// High-level runtime handle shared across the node.
#[derive(Clone)]
pub struct WasmRuntime {
    engine: Arc<Engine>,
}

impl WasmRuntime {
    pub fn new() -> Self {
        WasmRuntime {
            engine: Arc::new(compiler::build_engine()),
        }
    }

    /// Compile raw `.wasm` bytes to a serializable artifact.
    /// Run this in `tokio::task::spawn_blocking` — it is CPU-intensive.
    pub fn compile(&self, wasm_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
        compiler::compile(&self.engine, wasm_bytes)
    }

    /// Prepare a stored artifact for execution (near-instant).
    pub fn prepare(&self, artifact_bytes: &[u8], config: AppConfig) -> Result<PreparedModule, PlatformError> {
        let module = unsafe { compiler::deserialize(&self.engine, artifact_bytes) }?;
        Ok(PreparedModule {
            engine: (*self.engine).clone(),
            module,
            config,
        })
    }
}
```

---

## 5. Integration Test

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use common::types::{AppId, FuelQuota, MemoryPages};

    /// Compile and run a minimal WAT (WebAssembly Text Format) module.
    /// In real usage, this would be an Axum app compiled to wasm32-wasip2.
    #[test]
    fn test_compile_and_run_minimal() {
        // Minimal WAT: exports _start that returns immediately
        let wat = r#"
            (module
                (func (export "_start"))
            )
        "#;
        let wasm = wat::parse_str(wat).expect("invalid WAT");

        let runtime = WasmRuntime::new();
        let artifact = runtime.compile(&wasm).expect("compile failed");
        assert!(!artifact.is_empty());

        let config = AppConfig {
            id: AppId::new("test", "v1"),
            fuel_quota: FuelQuota(1_000_000),
            memory_limit: MemoryPages(256),
            env_vars: vec![],
            port: 9001,
        };

        let mut prepared = runtime.prepare(&artifact, config).expect("prepare failed");
        let mut instance = prepared.spawn_instance(vec![], 9001).expect("spawn failed");
        let stats = instance.run();

        assert!(stats.fuel_consumed > 0);
        assert!(stats.trap.is_none());
    }
}
```

---

## 6. Key Design Decisions

| Decision                     | Rationale                                                                                                             |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| AOT via Cranelift (Wasmtime Default)            | JIT (Singlepass) is faster to compile but slower to execute. Cranelift (Wasmtime Default) is the best AOT backend for Wasmtime on servers. |
| Engine shared via `Arc`      | Engine is thread-safe and expensive to create. Share one per process.                                                 |
| `spawn_blocking` for compile | Compilation is synchronous and CPU-bound. Never block the Tokio reactor.                                              |
| Fuel set per invocation      | This allows per-request quotas, not just per-instance quotas. Premium users can get more fuel.                        |
| Memory limit via Tunables    | `cgroups` cannot limit Wasm linear memory on all platforms. Tunables work at the Wasm level regardless of OS.         |

---

## 7. Extended Resource Limits

Fuel (CPU) and linear memory are the two primitives Wasm provides natively. But a
properly isolated tenant boundary requires additional limits — especially when WASI grants
the module access to networking and filesystem operations.

### Why These Extra Limits Matter

Without them, a malicious or buggy module could:
- Open thousands of outbound TCP connections (port exhaustion / outbound DDoS)
- Write multi-GB temp files to the preallocated virtual filesystem
- Send a burst of HTTP requests to an East-West service, amplifying internal load
- Starve other tenants of network bandwidth even while staying within fuel limits

Fuel only counts **instructions executed**. A single `send()` syscall consumes a few
hundred fuel but pushes megabytes onto the wire. Instruction-level metering does not
capture I/O-level resource use.

### 7.1 Open File Descriptor / Socket Limit

WASI maps file and socket operations to host-side descriptors. The Supervisor enforces a
per-instance descriptor cap by tracking descriptor allocation in the WASI host function
implementations.

```rust
// crates/runtime/src/limits.rs (continued)

/// Per-instance limits beyond fuel and memory.
#[derive(Debug, Clone, Copy)]
pub struct ExtendedLimits {
    /// Maximum number of simultaneously open file descriptors (files + sockets).
    /// Default: 64. This prevents port exhaustion on the host.
    pub max_open_fds: u32,

    /// Maximum bytes the module can write to its virtual filesystem.
    /// Default: 50 MB. Prevents disk exhaustion from temp files or logs.
    pub max_fs_write_bytes: u64,

    /// Maximum outbound network bytes per execution (request).
    /// Default: 10 MB. Prevents bandwidth abuse via East-West amplification.
    pub max_net_egress_bytes: u64,

    /// Maximum number of outbound TCP connections opened per execution.
    /// Default: 16. Prevents connection flooding.
    pub max_outbound_connections: u32,
}

impl Default for ExtendedLimits {
    fn default() -> Self {
        ExtendedLimits {
            max_open_fds: 64,
            max_fs_write_bytes: 50 * 1024 * 1024,   // 50 MB
            max_net_egress_bytes: 10 * 1024 * 1024,  // 10 MB
            max_outbound_connections: 16,
        }
    }
}

/// Tracks I/O resource usage for a single instance execution.
/// Passed to WASI host function wrappers to enforce limits at the syscall boundary.
pub struct IoResourceTracker {
    limits: ExtendedLimits,
    open_fds: u32,
    fs_bytes_written: u64,
    net_egress_bytes: u64,
    outbound_connections: u32,
}

impl IoResourceTracker {
    pub fn new(limits: ExtendedLimits) -> Self {
        IoResourceTracker {
            limits,
            open_fds: 0,
            fs_bytes_written: 0,
            net_egress_bytes: 0,
            outbound_connections: 0,
        }
    }

    /// Called by the WASI host when a file or socket is opened.
    /// Returns Err if the limit is exceeded — the WASI call returns EMFILE to the guest.
    pub fn track_fd_open(&mut self) -> Result<(), PlatformError> {
        if self.open_fds >= self.limits.max_open_fds {
            return Err(PlatformError::Runtime(format!(
                "fd limit reached: {} (max {})",
                self.open_fds, self.limits.max_open_fds
            )));
        }
        self.open_fds += 1;
        Ok(())
    }

    pub fn track_fd_close(&mut self) {
        self.open_fds = self.open_fds.saturating_sub(1);
    }

    /// Called by the WASI host on every write() to the virtual filesystem.
    pub fn track_fs_write(&mut self, bytes: u64) -> Result<(), PlatformError> {
        self.fs_bytes_written += bytes;
        if self.fs_bytes_written > self.limits.max_fs_write_bytes {
            return Err(PlatformError::Runtime(format!(
                "fs write limit exceeded: {} bytes (max {})",
                self.fs_bytes_written, self.limits.max_fs_write_bytes
            )));
        }
        Ok(())
    }

    /// Called by the WASI host on every outbound send().
    pub fn track_net_egress(&mut self, bytes: u64) -> Result<(), PlatformError> {
        self.net_egress_bytes += bytes;
        if self.net_egress_bytes > self.limits.max_net_egress_bytes {
            return Err(PlatformError::Runtime(format!(
                "network egress limit exceeded: {} bytes (max {})",
                self.net_egress_bytes, self.limits.max_net_egress_bytes
            )));
        }
        Ok(())
    }

    /// Called by the WASI host on each outbound TCP connect().
    pub fn track_outbound_connect(&mut self) -> Result<(), PlatformError> {
        self.outbound_connections += 1;
        if self.outbound_connections > self.limits.max_outbound_connections {
            return Err(PlatformError::Runtime(format!(
                "outbound connection limit exceeded: {} (max {})",
                self.outbound_connections, self.limits.max_outbound_connections
            )));
        }
        Ok(())
    }

    /// Snapshot of I/O usage for inclusion in ExecutionStats.
    pub fn stats(&self) -> IoStats {
        IoStats {
            open_fds_peak: self.open_fds,
            fs_bytes_written: self.fs_bytes_written,
            net_egress_bytes: self.net_egress_bytes,
            outbound_connections: self.outbound_connections,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IoStats {
    pub open_fds_peak: u32,
    pub fs_bytes_written: u64,
    pub net_egress_bytes: u64,
    pub outbound_connections: u32,
}
```

### 7.2 AppConfig Integration

Extended limits are optional per-app overrides. If not set, the defaults apply.

```rust
// crates/common/src/types.rs (addition to AppConfig)

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    // ... existing fields ...

    /// Optional extended I/O resource limits. Platform defaults apply if None.
    #[serde(default)]
    pub extended_limits: Option<ExtendedLimitsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtendedLimitsConfig {
    pub max_open_fds: Option<u32>,
    pub max_fs_write_bytes: Option<u64>,
    pub max_net_egress_bytes: Option<u64>,
    pub max_outbound_connections: Option<u32>,
}

impl ExtendedLimitsConfig {
    /// Merge with platform defaults: user-specified values override, missing ones
    /// fall back to ExtendedLimits::default().
    pub fn to_limits(&self) -> ExtendedLimits {
        let defaults = ExtendedLimits::default();
        ExtendedLimits {
            max_open_fds: self.max_open_fds.unwrap_or(defaults.max_open_fds),
            max_fs_write_bytes: self.max_fs_write_bytes.unwrap_or(defaults.max_fs_write_bytes),
            max_net_egress_bytes: self.max_net_egress_bytes.unwrap_or(defaults.max_net_egress_bytes),
            max_outbound_connections: self.max_outbound_connections.unwrap_or(defaults.max_outbound_connections),
        }
    }
}
```

### 7.3 Execution Flow with I/O Tracking

The `IoResourceTracker` is created alongside the Wasm `Store` and threaded through the
WASI host function imports. The WASI host functions (provided by `wasmtime_wasix`) are
wrapped to call the tracker before forwarding to the real implementation.

```
spawn_instance()
   │
   ├── Store::new() + configure_store() (fuel + memory)
   │
   ├── IoResourceTracker::new(extended_limits)
   │
   ├── WasiEnv::builder()
   │      └── .set_fd_limit(max_open_fds)  ← wasmtime_wasix native support
   │
   ├── Instance::new() with import wrappers
   │      └── fd_write   → tracker.track_fs_write(len) → real fd_write
   │      └── sock_send  → tracker.track_net_egress(len) → real sock_send
   │      └── sock_open  → tracker.track_outbound_connect() → real sock_open
   │      └── fd_close   → tracker.track_fd_close() → real fd_close
   │
   ├── _start() runs
   │
   └── ExecutionStats { ..., io_stats: tracker.stats() }
```

### 7.4 Why Not OS-Level Limits?

| Approach               | Problem                                                                                             |
| ---------------------- | --------------------------------------------------------------------------------------------------- |
| `ulimit -n` (fd limit) | Per-process, not per-Wasm-instance. 50 instances share the same limit.                              |
| `tc` (traffic shaping) | Per-interface, not per-instance. No tenant attribution.                                             |
| cgroups `io.max`       | Per-cgroup. Would need a cgroup per instance — too expensive to create/destroy at cold-start speed. |

Tracking at the WASI host function level is the only approach that provides per-instance
granularity without OS overhead. The WASI boundary is the natural enforcement point because
every I/O operation must cross it.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Compiler
- [ ] `WasmRuntime::new()` constructs without error
- [ ] `compile()` takes raw `.wasm` bytes and returns a non-empty artifact `Vec<u8>`
- [ ] `compile()` rejects invalid bytes with a `PlatformError::Runtime` (not a panic)
- [ ] `deserialize()` on the artifact bytes returns a `Module` without recompiling
- [ ] Deserialization is measurably faster than compilation (< 5ms for a typical app)
- [ ] Running `compile()` inside `tokio::task::spawn_blocking` does not block the async executor

### Fuel Metering
- [ ] `configure_store()` sets the fuel limit on the store without error
- [ ] A Wasm module that loops infinitely raises a Trap when fuel is exhausted (not a hang)
- [ ] `read_fuel_remaining()` returns a value less than the initial limit after running
- [ ] `fuel_consumed = fuel_limit - fuel_remaining` is a positive, non-zero number
- [ ] Setting `fuel = 0` causes an immediate Trap on the first instruction

### Memory Limits
- [ ] `LimitedTunables` prevents a module from allocating more than `MemoryPages` pages
- [ ] A module that calls `memory.grow` beyond the limit receives `-1` (growth refused) — not a crash
- [ ] `read_memory_usage()` returns a value > 0 after a module has run

### Execution
- [ ] A minimal `_start` WAT module runs to completion with `trap = None`
- [ ] `ExecutionStats` contains non-zero `fuel_consumed`, valid `ram_bytes`, valid `wall_clock_ms`
- [ ] Two modules can be instantiated simultaneously from the same `Engine` without data races

### Tests
- [ ] `test_compile_and_run_minimal` passes
- [ ] `test_fuel_exhaustion_trap` — infinite-loop module traps correctly
- [ ] `test_memory_limit_enforced` — oversized memory.grow is rejected

### Extended Resource Limits
- [ ] `IoResourceTracker` enforces `max_open_fds` — opening beyond the limit returns an error to the guest
- [ ] `IoResourceTracker` enforces `max_fs_write_bytes` — writes beyond the limit are rejected
- [ ] `IoResourceTracker` enforces `max_net_egress_bytes` — sends beyond the limit are rejected
- [ ] `IoResourceTracker` enforces `max_outbound_connections` — connections beyond the limit are rejected
- [ ] `ExtendedLimitsConfig` merges user overrides with platform defaults correctly (partial overrides fill from defaults)
- [ ] `IoStats` is included in `ExecutionStats` after each request for billing/observability
- [ ] A test verifies that fd_close decrements the open fd counter (allows reuse up to the limit)
- [ ] A test verifies that default `ExtendedLimits` are applied when `AppConfig.extended_limits` is `None`
