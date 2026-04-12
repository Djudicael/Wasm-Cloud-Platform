// crates/runtime/src/executor.rs
use crate::limits::{
    configure_store, read_fuel_remaining, IoResourceTracker, IoStats, MemoryLimiter,
};
use common::{
    error::PlatformError,
    types::{AppConfig, FuelQuota, InstanceId},
};
use std::time::Instant;
use wasmtime::{Engine, Instance, Linker, Module, Store};
use wasmtime_wasi::preview1::{add_to_linker_sync, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

/// The internal state of our Wasmtime Store.
pub struct StoreState {
    pub wasi: WasiP1Ctx,
    pub limiter: MemoryLimiter,
    pub io_tracker: IoResourceTracker,
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
    pub module: Module,
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
    ) -> Result<RunningInstance, PlatformError> {
        let id = InstanceId::new();

        // Build WASI environment (Preview 1 adapter for standard Core Wasm)
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio(); // For debugging
        for (k, v) in env_vars {
            builder.env(&k, &v);
        }
        // The app will bind to 0.0.0.0:<port>; the Supervisor maps this port.
        builder.env("PORT", &port.to_string());

        let extended_limits = self
            .config
            .extended_limits
            .as_ref()
            .map(|c| c.to_limits())
            .unwrap_or_default();

        let state = StoreState {
            wasi: builder.build_p1(),
            limiter: MemoryLimiter::new(self.config.memory_limit),
            io_tracker: IoResourceTracker::new(extended_limits),
        };

        let mut store = Store::new(&self.engine, state);

        // Hook up the resource limiter for memory bounds
        store.limiter(|s| &mut s.limiter);

        // Apply CPU/fuel limits
        configure_store(&mut store, self.config.fuel_quota)?;

        // Link WASI host functions
        let mut linker = Linker::new(&self.engine);
        add_to_linker_sync(&mut linker, |s: &mut StoreState| &mut s.wasi)
            .map_err(|e| PlatformError::Runtime(format!("linker error: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| PlatformError::Runtime(format!("instantiation error: {e}")))?;

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

        // Extract and run the _start function exported by WASI
        let start_fn = self
            .instance
            .get_typed_func::<(), ()>(&mut self.store, "_start");
        match start_fn {
            Ok(f) => {
                if let Err(e) = f.call(&mut self.store, ()) {
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
        // Get the Wasm linear memory size
        if let Some(mem) = self.instance.get_memory(&mut self.store, "memory") {
            mem.data_size(&self.store)
        } else {
            0
        }
    }
}
