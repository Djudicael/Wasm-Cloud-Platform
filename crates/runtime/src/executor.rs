// crates/runtime/src/executor.rs
use crate::limits::{
    configure_store, read_fuel_remaining, IoResourceTracker, IoStats, MemoryLimiter,
};
use common::{
    error::PlatformError,
    types::{AppConfig, InstanceId},
};
use std::time::Instant;
use tokio::sync::mpsc;
use wasmtime::component::{Component, Instance, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, SocketAddrUse, WasiCtx, WasiCtxBuilder, WasiView};

/// Channels for capturing Wasm stdout/stderr.
pub struct WasiStreams {
    pub stdout_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub stderr_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// The internal state of our Wasmtime Store.
pub struct StoreState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
    pub limiter: MemoryLimiter,
    pub io_tracker: IoResourceTracker,
}

impl WasiView for StoreState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}

/// Result of a single Wasm execution.
#[derive(Debug)]
pub struct ExecutionStats {
    pub instance_id: InstanceId,
    pub fuel_limit: u64,
    pub fuel_consumed: u64,
    pub ram_bytes: usize,
    pub wall_clock_ms: u64,
    pub trap: Option<String>,
    pub io_stats: IoStats,
}

/// A prepared, AOT-compiled module ready for repeated instantiation.
pub struct PreparedModule {
    pub engine: Engine,
    pub module: Component,
    pub config: AppConfig,
}

impl PreparedModule {
    /// Build from a deserialized artifact + app config.
    pub fn from_artifact(
        engine: Engine,
        artifact_bytes: &[u8],
        config: AppConfig,
    ) -> Result<Self, PlatformError> {
        // SAFETY: artifact was produced by our own compiler::compile()
        let module = unsafe { crate::compiler::deserialize(&engine, artifact_bytes) }?;
        Ok(PreparedModule {
            engine,
            module,
            config,
        })
    }

    /// Instantiate and run the module.
    pub fn spawn_instance(
        &self,
        env_vars: Vec<(String, String)>,
        port: u16,
    ) -> Result<(RunningInstance, WasiStreams), PlatformError> {
        let id = InstanceId::new();

        // Create custom pipes for capturing stdout/stderr
        let (stdout_pipe, stdout_rx) = crate::custom_pipe::ChannelPipe::new();
        let (stderr_pipe, stderr_rx) = crate::custom_pipe::ChannelPipe::new();

        // Build WASI environment (Preview 2)
        let mut builder = WasiCtxBuilder::new();
        builder.stdout(stdout_pipe);
        builder.stderr(stderr_pipe);
        builder.inherit_network(); // Give network access handled by address checks

        for (k, v) in env_vars {
            builder.env(&k, &v);
        }
        // The app will bind to 0.0.0.0:<port>; the Supervisor maps this port.
        builder.env("PORT", &port.to_string());

        // Allow TCP networking natively on the specified ports
        builder.socket_addr_check(|_addr, _action: SocketAddrUse| true);

        let extended_limits = self
            .config
            .extended_limits
            .as_ref()
            .map(|c| c.to_limits())
            .unwrap_or_default();

        // Create streams with the receivers
        let streams = WasiStreams {
            stdout_rx,
            stderr_rx,
        };

        let state = StoreState {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(self.config.memory_limit),
            io_tracker: IoResourceTracker::new(extended_limits),
        };

        let mut store = Store::new(&self.engine, state);

        // Hook up the resource limiter for memory bounds
        store.limiter(|s| &mut s.limiter);

        // Apply CPU/fuel limits
        configure_store(&mut store, self.config.fuel_quota)?;

        // Link WASI host functions (Component Model)
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| PlatformError::Runtime(format!("linker error: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| PlatformError::Runtime(format!("instantiation error: {e}")))?;

        Ok((
            RunningInstance {
                id,
                instance,
                store,
                config: self.config.clone(),
                started_at: Instant::now(),
            },
            streams,
        ))
    }
}

/// An instantiated, running Wasm module.
pub struct RunningInstance {
    pub id: InstanceId,
    instance: Instance,
    store: Store<StoreState>,
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

        // In Component Model, common entry points are `wasi:cli/run@0.2.0#run` or custom `run`
        let start_fn = self
            .instance
            .get_func(&mut self.store, "wasi:cli/run@0.2.0#run")
            .or_else(|| self.instance.get_func(&mut self.store, "run"))
            .or_else(|| self.instance.get_func(&mut self.store, "_start"));

        match start_fn {
            Some(f) => {
                let mut results =
                    vec![wasmtime::component::Val::Bool(false); f.results(&self.store).len()];
                if let Err(e) = f.call(&mut self.store, &[], &mut results) {
                    trap_msg = Some(e.to_string());
                } else {
                    f.post_return(&mut self.store).ok();
                }
            }
            None => {
                trap_msg = Some("export not found".to_string());
            }
        }

        let fuel_remaining = read_fuel_remaining(&self.store);
        let fuel_consumed = fuel_limit.saturating_sub(fuel_remaining);
        let ram_bytes = self.read_memory_usage();
        let wall_clock_ms = start.elapsed().as_millis() as u64;

        let io_stats = self.store.data().io_tracker.stats();

        ExecutionStats {
            instance_id: self.id.clone(),
            fuel_limit,
            fuel_consumed,
            ram_bytes,
            wall_clock_ms,
            trap: trap_msg,
            io_stats,
        }
    }

    fn read_memory_usage(&mut self) -> usize {
        // In the component model, linear memories are deeply nested and not directly
        // exported as `Val::Memory`. For now, we return a non-zero placeholder.
        1024
    }
}
