// crates/runtime/src/executor.rs
use crate::limits::{configure_store, read_fuel_remaining, IoStats, MemoryLimiter};
use crate::policy_tracker::PolicyEnforcer;
use common::{
    error::PlatformError,
    policy::InstancePolicy,
    types::{AppConfig, InstanceId},
};
use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use wasmtime::component::{Component, Instance, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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

/// Snapshot of the instance network policy used by the WASI socket address checker.
#[derive(Debug, Clone)]
pub(crate) struct SocketPolicyCheck {
    allow_inbound: bool,
    allow_outbound_tcp: bool,
    allow_outbound_udp: bool,
    allowed_bind_ports: Arc<HashSet<u16>>,
    allowed_cidrs: Arc<Vec<ipnet::IpNet>>,
    denied_cidrs: Arc<Vec<ipnet::IpNet>>,
}

impl SocketPolicyCheck {
    pub(crate) fn from_instance_policy(policy: &InstancePolicy) -> Self {
        SocketPolicyCheck {
            allow_inbound: policy.network.allow_inbound,
            allow_outbound_tcp: policy.network.allow_outbound_tcp,
            allow_outbound_udp: policy.network.allow_outbound_udp,
            allowed_bind_ports: Arc::new(
                policy.network.allowed_bind_ports.iter().copied().collect(),
            ),
            allowed_cidrs: Arc::new(Self::parse_cidrs(&policy.network.allowed_cidrs)),
            denied_cidrs: Arc::new(Self::parse_cidrs(&policy.network.denied_cidrs)),
        }
    }

    fn parse_cidrs(cidrs: &[String]) -> Vec<ipnet::IpNet> {
        cidrs
            .iter()
            .filter_map(|cidr| match cidr.parse::<ipnet::IpNet>() {
                Ok(net) => Some(net),
                Err(err) => {
                    tracing::warn!(cidr, error = %err, "ignoring invalid CIDR in socket policy snapshot");
                    None
                }
            })
            .collect()
    }

    fn outbound_ip_allowed(&self, ip: IpAddr) -> Result<(), &'static str> {
        if self.denied_cidrs.iter().any(|cidr| cidr.contains(&ip)) {
            return Err("destination in denied_cidrs");
        }

        if !self.allowed_cidrs.is_empty()
            && !self.allowed_cidrs.iter().any(|cidr| cidr.contains(&ip))
        {
            return Err("destination not in allowed_cidrs");
        }

        Ok(())
    }

    pub(crate) fn check(
        &self,
        addr: SocketAddr,
        use_type: SocketAddrUse,
    ) -> Result<(), &'static str> {
        match use_type {
            SocketAddrUse::TcpBind => {
                if !self.allow_inbound {
                    return Err("inbound tcp bind disabled");
                }
                if !self.allowed_bind_ports.contains(&addr.port()) {
                    return Err("bind port not allowed");
                }
                Ok(())
            }
            SocketAddrUse::TcpConnect => {
                if !self.allow_outbound_tcp {
                    return Err("outbound tcp disabled");
                }
                self.outbound_ip_allowed(addr.ip())
            }
            SocketAddrUse::UdpBind
            | SocketAddrUse::UdpConnect
            | SocketAddrUse::UdpOutgoingDatagram => {
                if !self.allow_outbound_udp {
                    return Err("outbound udp disabled");
                }
                self.outbound_ip_allowed(addr.ip())
            }
        }
    }
}

pub(crate) fn compose_socket_addr_check(
    policy_check: SocketPolicyCheck,
    extra_check: Option<SocketAddrCheckFn>,
) -> SocketAddrCheckFn {
    Box::new(move |addr, use_type| {
        if let Err(reason) = policy_check.check(addr, use_type) {
            tracing::warn!(dest = %addr, use_type = ?use_type, reason, "socket operation denied by runtime policy");
            return Box::pin(async { false });
        }

        if let Some(check) = extra_check.as_ref() {
            check(addr, use_type)
        } else {
            Box::pin(async { true })
        }
    })
}

const TOP_LEVEL_ENTRY_POINT_FALLBACKS: &[&str] = &["run", "_start"];

pub(crate) fn top_level_entry_point_candidates() -> &'static [&'static str] {
    TOP_LEVEL_ENTRY_POINT_FALLBACKS
}

fn configure_filesystem_preopens(
    builder: &mut WasiCtxBuilder,
    policy: &common::policy::InstancePolicy,
) -> Result<(), PlatformError> {
    if policy.filesystem.allowed_paths.is_empty() {
        return Ok(());
    }

    let mut dir_perms = DirPerms::READ;
    let mut file_perms = FilePerms::READ;
    if policy.filesystem.allow_file_create || policy.filesystem.allow_file_delete {
        dir_perms |= DirPerms::MUTATE;
        file_perms |= FilePerms::WRITE;
    }

    for path in &policy.filesystem.allowed_paths {
        let host_path = Path::new(path);
        builder
            .preopened_dir(host_path, path, dir_perms, file_perms)
            .map_err(|e| {
                PlatformError::runtime(format!("failed to preopen allowed path {}: {}", path, e))
            })?;
    }

    Ok(())
}

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
        // Wasmtime exposes coarse protocol-level toggles (`allow_tcp`, `allow_udp`) and a
        // per-operation `socket_addr_check` hook. We enable TCP if the instance is allowed
        // to either bind/listen or initiate outbound TCP, then use the policy-aware socket
        // checker to keep inbound bind permission separate from outbound connect permission.
        builder.inherit_network();
        let allow_any_tcp = policy.network.allow_outbound_tcp || policy.network.allow_inbound;
        builder.allow_tcp(allow_any_tcp);
        builder.allow_udp(policy.network.allow_outbound_udp);
        builder.allow_ip_name_lookup(policy.network.allow_dns);

        let policy_socket_check = SocketPolicyCheck::from_instance_policy(&policy);
        let combined_socket_check =
            compose_socket_addr_check(policy_socket_check, socket_addr_check);
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
            combined_socket_check(addr, use_enum)
        });
        tracing::debug!("policy-aware socket_addr_check installed");

        tracing::debug!(
            allow_tcp = %allow_any_tcp,
            allow_udp = %policy.network.allow_outbound_udp,
            allow_ip_name_lookup = %policy.network.allow_dns,
            allow_inbound = %policy.network.allow_inbound,
            allow_outbound_tcp = %policy.network.allow_outbound_tcp,
            "WASI config built from policy"
        );

        for (k, v) in env_vars {
            builder.env(&k, &v);
        }
        // The app is expected to bind to the injected runtime bind address on
        // the allocated host port; the Supervisor enforces the allowed bind IP
        // and port via the WASI socket address checker.
        let port_str = port.to_string();
        builder.env("PORT", &port_str);

        // Configure filesystem preopens from policy. The current policy model uses the
        // configured path both as the host path and the guest-visible mount path. When file
        // create/delete is allowed we grant write/mutate permissions; otherwise the directory
        // is exposed as read-only.
        configure_filesystem_preopens(&mut builder, &policy)?;

        let extended_limits = self
            .config
            .extended_limits
            .clone()
            .map(|cfg| cfg.to_limits())
            .unwrap_or_default();

        let state = StoreState {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(self.config.memory_limit, extended_limits),
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

#[derive(Debug, Clone)]
struct ResolvedEntryPoint {
    name: String,
    func: wasmtime::component::Func,
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
    fn resolve_entry_point(&mut self) -> Option<ResolvedEntryPoint> {
        // WASI Preview 2 components typically export `wasi:cli/run@0.2.x` as an interface
        // containing `run`. For compatibility with older/minimal components and tests, also
        // support top-level `run` and `_start` exports.
        let wasi_versions = [
            "0.2.6", "0.2.5", "0.2.4", "0.2.3", "0.2.2", "0.2.1", "0.2.0",
        ];

        for ver in wasi_versions {
            let interface_name = format!("wasi:cli/run@{ver}");
            let interface_idx =
                self.instance
                    .get_export_index(&mut self.store, None, &interface_name);

            tracing::trace!(interface = %interface_name, "checking for entry point");

            let Some(interface_idx) = interface_idx else {
                continue;
            };

            let func_idx =
                self.instance
                    .get_export_index(&mut self.store, Some(&interface_idx), "run");

            tracing::trace!(interface = %interface_name, has_run = func_idx.is_some(), "checking for run function");

            let Some(func_idx) = func_idx else {
                continue;
            };

            if let Some(func) = self.instance.get_func(&mut self.store, func_idx) {
                return Some(ResolvedEntryPoint {
                    name: format!("{interface_name}#run"),
                    func,
                });
            }
        }

        for export_name in top_level_entry_point_candidates() {
            tracing::trace!(
                export = export_name,
                "checking top-level entry point fallback"
            );
            let func_idx = self
                .instance
                .get_export_index(&mut self.store, None, export_name);
            let Some(func_idx) = func_idx else {
                continue;
            };

            if let Some(func) = self.instance.get_func(&mut self.store, func_idx) {
                return Some(ResolvedEntryPoint {
                    name: export_name.to_string(),
                    func,
                });
            }
        }

        None
    }

    fn invoke_entry_point(&mut self, entry_point: &ResolvedEntryPoint) -> Option<String> {
        tracing::debug!(entry_point = %entry_point.name, "calling entry point");

        // Preferred WASI Preview 2 signature: run() -> result<(), ()>
        if let Ok(typed) = entry_point.func.typed::<(), (Result<(), ()>,)>(&self.store) {
            return match typed.call(&mut self.store, ()) {
                Ok((result,)) => match result {
                    Ok(()) => {
                        tracing::debug!(entry_point = %entry_point.name, "entry point completed successfully");
                        None
                    }
                    Err(()) => {
                        tracing::error!(instance_id = %self.id.0, entry_point = %entry_point.name, "WASM app exited with error");
                        Some("WASM app exited with error".to_string())
                    }
                },
                Err(e) => {
                    let err_msg = e.to_string();
                    tracing::error!(instance_id = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM trap");
                    tracing::error!(instance = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM function call failed");
                    Some(err_msg)
                }
            };
        }

        // Compatibility fallback: minimal components often export a top-level `run` or `_start`
        // with no result payload.
        if let Ok(typed) = entry_point.func.typed::<(), ()>(&self.store) {
            return match typed.call(&mut self.store, ()) {
                Ok(()) => {
                    tracing::debug!(entry_point = %entry_point.name, "entry point completed successfully via no-result fallback");
                    None
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    tracing::error!(instance_id = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM trap");
                    tracing::error!(instance = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM function call failed");
                    Some(err_msg)
                }
            };
        }

        tracing::debug!(entry_point = %entry_point.name, "typed entry point invocation failed, trying untyped fallback");
        if let Err(e) = entry_point.func.call(&mut self.store, &[], &mut []) {
            let err_msg = e.to_string();
            tracing::error!(instance_id = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM trap");
            tracing::error!(instance = %self.id.0, entry_point = %entry_point.name, error = %err_msg, "WASM function call failed");
            Some(err_msg)
        } else {
            tracing::debug!(entry_point = %entry_point.name, "entry point called successfully via untyped fallback");
            None
        }
    }

    /// Call `_start` (the WASI entry point). This blocks until the Wasm app exits.
    /// For a server like Axum, this runs indefinitely until the Supervisor kills it.
    pub fn run(&mut self) -> ExecutionStats {
        tracing::info!(instance_id = %self.id.0, "RunningInstance::run() called");
        let fuel_limit = self.config.fuel_quota.0;
        let start = Instant::now();

        let entry_point = self.resolve_entry_point();

        if let Some(ref entry_point) = entry_point {
            tracing::info!(entry_point = %entry_point.name, "WASI entry point found and callable");
        }

        tracing::debug!(
            has_entry_point = entry_point.is_some(),
            entry_point = entry_point
                .as_ref()
                .map(|e| e.name.as_str())
                .unwrap_or("<none>"),
            "entry point lookup complete"
        );
        tracing::info!(
            has_entry_point = entry_point.is_some(),
            entry_point = entry_point
                .as_ref()
                .map(|e| e.name.as_str())
                .unwrap_or("<none>"),
            "entry point lookup result"
        );

        let trap_msg = match entry_point {
            Some(entry_point) => self.invoke_entry_point(&entry_point),
            None => {
                tracing::error!(instance_id = %self.id.0, "No WASI entry point found");
                tracing::error!(instance = %self.id.0, "No entry point found (wasi:cli/run@0.2.x#run, run, or _start)");
                Some("export not found".to_string())
            }
        };

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
