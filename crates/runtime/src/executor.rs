// crates/runtime/src/executor.rs
use crate::limits::{configure_store, read_fuel_remaining, IoStats, MemoryLimiter};
use crate::policy_tracker::PolicyEnforcer;
use common::{
    error::PlatformError,
    types::{AppConfig, InstanceId},
};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use wasmtime::component::{Component, Instance, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Simplified mirror of `wasmtime_wasi::sockets::SocketAddrUse` for the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketAddrUse {
    TcpBind,
    TcpConnect,
    UdpBind,
    UdpConnect,
    UdpOutgoingDatagram,
}

/// Async callback for validating outbound socket addresses.
/// Returns `true` to allow the operation, `false` to deny.
pub type SocketAddrCheckFn = Box<
    dyn Fn(SocketAddr, SocketAddrUse) -> Pin<Box<dyn Future<Output = bool> + Send + Sync>>
        + Send
        + Sync,
>;

/// Store state for WASI Preview 2
pub struct StoreState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
    pub limiter: MemoryLimiter,
    pub policy_enforcer: PolicyEnforcer,
}

impl std::fmt::Debug for StoreState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreState")
            .field("limiter", &self.limiter)
            .field("policy_enforcer", &self.policy_enforcer)
            .finish_non_exhaustive()
    }
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
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
    pub engine: Arc<Engine>,
    pub module: Component,
    pub config: AppConfig,
}

impl std::fmt::Debug for PreparedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedModule")
            .field("config", &self.config)
            .finish()
    }
}

impl PreparedModule {
    /// Build from a deserialized artifact + app config.
    pub fn from_artifact(
        engine: Arc<Engine>,
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
    ///
    /// `socket_addr_check` is an optional async callback invoked by wasmtime-wasi
    /// for every outbound socket operation. It receives the destination address
    /// and the operation type (connect, bind, etc.). Return `true` to allow,
    /// `false` to deny (the Wasm module receives a permission-denied error).
    pub fn spawn_instance(
        &self,
        env_vars: Vec<(String, String)>,
        port: u16,
        socket_addr_check: Option<SocketAddrCheckFn>,
    ) -> Result<RunningInstance, PlatformError> {
        tracing::info!(app = %self.config.id.0, "spawn_instance called");
        let id = InstanceId::new();
        tracing::info!(instance_id = %id.0, "instance ID created");

        // Resolve the policy for this instance
        let policy = match self.config.policy.as_ref() {
            Some(p) => p.resolve(port),
            None => common::policy::PolicyConfig::default().resolve(port),
        }
        .map_err(|e| PlatformError::runtime(format!("invalid policy config: {e}")))?;

        // Build WASI environment (Preview 2)
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdout();
        builder.inherit_stderr();

        // Network configuration based on policy.
        //
        // socket_addr_check provides per-destination validation. It is called
        // by wasmtime-wasi for every socket bind/connect. It blocks cross-namespace
        // connections to direct app ports, but the gateway port (9080) is open to
        // all namespaces. Namespace isolation relies primarily on service discovery.
        builder.inherit_network();
        builder.allow_tcp(policy.network.allow_outbound_tcp || policy.network.allow_inbound);
        builder.allow_udp(policy.network.allow_outbound_udp);
        builder.allow_ip_name_lookup(policy.network.allow_dns);

        if let Some(check) = socket_addr_check {
            builder.socket_addr_check(move |addr, use_type| {
                let use_enum = match use_type {
                    wasmtime_wasi::sockets::SocketAddrUse::TcpBind => SocketAddrUse::TcpBind,
                    wasmtime_wasi::sockets::SocketAddrUse::TcpConnect => SocketAddrUse::TcpConnect,
                    wasmtime_wasi::sockets::SocketAddrUse::UdpBind => SocketAddrUse::UdpBind,
                    wasmtime_wasi::sockets::SocketAddrUse::UdpConnect => SocketAddrUse::UdpConnect,
                    wasmtime_wasi::sockets::SocketAddrUse::UdpOutgoingDatagram => {
                        SocketAddrUse::UdpOutgoingDatagram
                    }
                };
                check(addr, use_enum)
            });
            tracing::debug!("socket_addr_check installed for namespace-aware outbound filtering");
        }

        tracing::debug!(
            allow_tcp = %(policy.network.allow_outbound_tcp || policy.network.allow_inbound),
            allow_udp = %policy.network.allow_outbound_udp,
            allow_ip_name_lookup = %policy.network.allow_dns,
            "WASI config built from policy"
        );

        for (k, v) in env_vars {
            builder.env(&k, &v);
        }
        // The app will bind to 0.0.0.0:<port>; the Supervisor maps this port.
        let port_str = port.to_string();
        builder.env("PORT", &port_str);

        // TODO(step-33): Preopened directories are not yet configured from
        // policy.filesystem.allowed_paths. WasiCtxBuilder::preopened_dir()
        // exists and can be wired up here — this is a complexity gap, not a
        // library limitation. When implemented, each path in allowed_paths
        // should be preopened with read-only or read-write permissions based
        // on allow_file_create / allow_file_delete. Currently the Wasm module
        // inherits the host filesystem with no restrictions at the WASI layer.

        let state = StoreState {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(self.config.memory_limit),
            policy_enforcer: PolicyEnforcer::new(policy),
        };

        let mut store = Store::new(&*self.engine, state);

        // Hook up the resource limiter for memory bounds
        store.limiter(|s| &mut s.limiter);

        // Apply CPU/fuel limits
        configure_store(&mut store, self.config.fuel_quota)?;

        // Link WASI host functions (Component Model Preview 2)
        let mut linker = Linker::new(&*self.engine);
        add_to_linker_sync(&mut linker)
            .map_err(|e| PlatformError::runtime(format!("linker error: {e}")))?;

        tracing::debug!("instantiating component");
        let instance = linker.instantiate(&mut store, &self.module).map_err(|e| {
            tracing::warn!(error = %e, "instantiation failed");
            PlatformError::runtime(format!("instantiation error: {e}"))
        })?;

        tracing::debug!("component instantiated");

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

impl Drop for RunningInstance {
    fn drop(&mut self) {
        // NOTE: We cannot easily decrement policy counters (outbound_connections_active,
        // open_fds, etc.) here because the counters live inside StoreState which is owned
        // by self.store. During Drop, self.store is also being dropped, so accessing its
        // data is not safe. A proper fix would require the counters to be held in an
        // Arc separate from the Store, or a pre-drop hook called explicitly before the
        // instance is dropped. For now, counters are approximate and may over-count
        // active resources until the Store is fully collected.
        tracing::debug!(instance_id = %self.id.0, "Dropping RunningInstance");
    }
}

impl RunningInstance {
    /// Call `_start` (the WASI entry point). This blocks until the Wasm app exits.
    /// For a server like Axum, this runs indefinitely until the Supervisor kills it.
    pub fn run(&mut self) -> ExecutionStats {
        tracing::info!(instance_id = %self.id.0, "RunningInstance::run() called");
        let fuel_limit = self.config.fuel_quota.0;
        let start = Instant::now();
        let mut trap_msg = None;

        // WASI Preview 2 components export `wasi:cli/run@0.2.x` as an interface containing `run`.
        // We need to navigate: interface -> function inside it.
        let wasi_versions = [
            "0.2.6", "0.2.5", "0.2.4", "0.2.3", "0.2.2", "0.2.1", "0.2.0",
        ];

        let start_fn = wasi_versions.iter().find_map(|ver| {
            let interface_name = format!("wasi:cli/run@{ver}");

            // Get the interface index
            let interface_idx =
                self.instance
                    .get_export_index(&mut self.store, None, &interface_name);

            tracing::trace!(interface = %interface_name, "checking for entry point");

            let interface_idx = interface_idx?;

            // Get the function index inside the interface
            let func_idx =
                self.instance
                    .get_export_index(&mut self.store, Some(&interface_idx), "run");

            tracing::trace!(interface = %interface_name, has_run = func_idx.is_some(), "checking for run function");

            let func_idx = func_idx?;
            self.instance.get_func(&mut self.store, func_idx)
        });

        if start_fn.is_some() {
            tracing::info!("WASI entry point found and callable");
        }

        tracing::debug!(
            has_entry_point = start_fn.is_some(),
            "entry point lookup complete"
        );
        tracing::info!(
            has_entry_point = start_fn.is_some(),
            "entry point lookup result"
        );

        match start_fn {
            Some(f) => {
                tracing::debug!("calling entry point");
                // The run function returns Result<(), ()> wrapped in a tuple
                let typed = f.typed::<(), (Result<(), ()>,)>(&self.store);
                match typed {
                    Ok(t) => match t.call(&mut self.store, ()) {
                        Ok((result,)) => match result {
                            Ok(()) => {
                                tracing::debug!("entry point completed successfully");
                            }
                            Err(()) => {
                                let err = "WASM app exited with error".to_string();
                                tracing::error!(instance_id = %self.id.0, "WASM app exited with error");
                                trap_msg = Some(err);
                            }
                        },
                        Err(e) => {
                            let err_msg = e.to_string();
                            tracing::error!(instance_id = %self.id.0, error = %err_msg, "WASM trap");
                            tracing::error!(instance = %self.id.0, error = %err_msg, "WASM function call failed");
                            trap_msg = Some(err_msg);
                        }
                    },
                    Err(typed_err) => {
                        // Fallback to untyped call
                        tracing::debug!(error = ?typed_err, "typed call failed, trying untyped");
                        if let Err(e) = f.call(&mut self.store, &[], &mut []) {
                            let err_msg = e.to_string();
                            tracing::error!(instance_id = %self.id.0, error = %err_msg, "WASM trap");
                            tracing::error!(instance = %self.id.0, error = %err_msg, "WASM function call failed");
                            trap_msg = Some(err_msg);
                        } else {
                            tracing::debug!("entry point called successfully");
                        }
                    }
                }
            }
            None => {
                tracing::error!(instance_id = %self.id.0, "No WASI entry point found");
                tracing::error!(instance = %self.id.0, "No entry point found (wasi:cli/run@0.2.x#run, run, or _start)");
                trap_msg = Some("export not found".to_string());
            }
        }

        let fuel_remaining = read_fuel_remaining(&self.store);
        let fuel_consumed = fuel_limit.saturating_sub(fuel_remaining);
        let ram_bytes = self.read_memory_usage();
        let wall_clock_ms = start.elapsed().as_millis() as u64;

        // Populate io_stats from policy counters
        let counters = &self.store.data().policy_enforcer.counters;
        let io_stats = IoStats {
            open_fds_peak: counters.open_fds.load(std::sync::atomic::Ordering::Relaxed),
            fs_bytes_written: counters
                .fs_write_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            net_egress_bytes: counters
                .egress_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            outbound_connections: counters
                .outbound_connections_total
                .load(std::sync::atomic::Ordering::Relaxed)
                as u32,
        };

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
        self.store.data().limiter.current_memory() as usize
    }
}
