// crates/runtime/src/executor.rs
use crate::limits::{configure_store, read_fuel_remaining, IoStats, MemoryLimiter};
use crate::policy_tracker::{PolicyCounters, PolicyEnforcer};
mod socket_policy;
use common::{
    error::PlatformError,
    policy::InstancePolicy,
    types::{AppConfig, InstanceId},
};
use hyper::body::{Body as HyperBodyTrait, Frame, SizeHint};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
pub(crate) use socket_policy::{compose_socket_addr_check, SocketPolicyCheck};
pub use socket_policy::{SocketAddrCheckFn, SocketAddrUse};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use wasmtime::component::{Component, Instance, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::add_to_linker_async as add_wasi_to_linker_async;
use wasmtime_wasi::p2::add_to_linker_sync as add_wasi_to_linker_sync;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::{
    add_only_http_to_linker_async, add_only_http_to_linker_sync, WasiHttpCtxView, WasiHttpView,
};
use wasmtime_wasi_http::WasiHttpCtx;

const WASI_CLI_RUN_INTERFACES: &[&str] = &[
    "wasi:cli/run@0.2.6",
    "wasi:cli/run@0.2.5",
    "wasi:cli/run@0.2.4",
    "wasi:cli/run@0.2.3",
    "wasi:cli/run@0.2.2",
    "wasi:cli/run@0.2.1",
    "wasi:cli/run@0.2.0",
];
const WASI_HTTP_INCOMING_HANDLER_INTERFACES: &[&str] = &[
    "wasi:http/incoming-handler@0.2.3",
    "wasi:http/incoming-handler@0.2.2",
    "wasi:http/incoming-handler@0.2.1",
    "wasi:http/incoming-handler@0.2.0",
];
const TOP_LEVEL_ENTRY_POINT_FALLBACKS: &[&str] = &["run", "_start"];
pub(crate) fn top_level_entry_point_candidates() -> &'static [&'static str] {
    TOP_LEVEL_ENTRY_POINT_FALLBACKS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentExecutionModel {
    WasiCli,
    WasiHttpIncomingHandler,
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

fn resolve_instance_policy(config: &AppConfig, port: u16) -> Result<InstancePolicy, PlatformError> {
    match config.policy.as_ref() {
        Some(p) => p.resolve(port),
        None => common::policy::PolicyConfig::default().resolve(port),
    }
    .map_err(|e| PlatformError::runtime(format!("invalid policy config: {e}")))
}

fn build_store_state(
    config: &AppConfig,
    env_vars: Vec<(String, String)>,
    port: u16,
    socket_addr_check: Option<SocketAddrCheckFn>,
    shared_policy_counters: Option<Arc<PolicyCounters>>,
) -> Result<StoreState, PlatformError> {
    let policy = resolve_instance_policy(config, port)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout();
    builder.inherit_stderr();
    builder.inherit_network();
    let allow_any_tcp = policy.network.allow_outbound_tcp || policy.network.allow_inbound;
    builder.allow_tcp(allow_any_tcp);
    builder.allow_udp(policy.network.allow_outbound_udp);
    builder.allow_ip_name_lookup(policy.network.allow_dns);

    let policy_enforcer = match shared_policy_counters {
        Some(counters) => PolicyEnforcer::with_counters(policy.clone(), counters),
        None => PolicyEnforcer::new(policy.clone()),
    };
    let policy_socket_check = SocketPolicyCheck::from_instance_policy(&policy);
    let combined_socket_check = compose_socket_addr_check(
        policy_socket_check,
        policy_enforcer.clone(),
        socket_addr_check,
    );
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

    for (k, v) in env_vars {
        builder.env(&k, &v);
    }
    let port_str = port.to_string();
    builder.env("PORT", &port_str);

    configure_filesystem_preopens(&mut builder, &policy)?;

    let extended_limits = config
        .extended_limits
        .clone()
        .map(|cfg| cfg.to_limits())
        .unwrap_or_default();

    Ok(StoreState {
        ctx: builder.build(),
        http: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        limiter: MemoryLimiter::new(
            config.memory_limit,
            extended_limits,
            Some(policy_enforcer.counters.clone()),
        ),
        policy_enforcer,
    })
}

fn build_runtime_linker(engine: &Engine) -> Result<Linker<StoreState>, PlatformError> {
    let mut linker = Linker::new(engine);
    add_wasi_to_linker_sync(&mut linker)
        .map_err(|e| PlatformError::runtime(format!("wasi linker error: {e}")))?;
    add_only_http_to_linker_sync(&mut linker)
        .map_err(|e| PlatformError::runtime(format!("wasi:http linker error: {e}")))?;
    Ok(linker)
}

fn build_async_http_linker(engine: &Engine) -> Result<Linker<StoreState>, PlatformError> {
    let mut linker = Linker::new(engine);
    add_wasi_to_linker_async(&mut linker)
        .map_err(|e| PlatformError::runtime(format!("wasi linker error: {e}")))?;
    add_only_http_to_linker_async(&mut linker)
        .map_err(|e| PlatformError::runtime(format!("wasi:http linker error: {e}")))?;
    Ok(linker)
}

/// Store state for WASI Preview 2
pub struct StoreState {
    pub ctx: WasiCtx,
    pub http: WasiHttpCtx,
    pub table: ResourceTable,
    pub limiter: MemoryLimiter,
    pub policy_enforcer: PolicyEnforcer,
}

impl Drop for StoreState {
    fn drop(&mut self) {
        self.policy_enforcer.release_tracked_outbound_connections();
    }
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

impl WasiHttpView for StoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
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
    pub execution_model: ComponentExecutionModel,
}

struct ManagedOutgoingBody {
    inner: Pin<Box<HyperOutgoingBody>>,
    completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ManagedOutgoingBody {
    fn new(inner: HyperOutgoingBody, completion_tx: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            inner: Box::pin(inner),
            completion_tx: Some(completion_tx),
        }
    }

    fn signal_complete(&mut self) {
        if let Some(tx) = self.completion_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ManagedOutgoingBody {
    fn drop(&mut self) {
        self.signal_complete();
    }
}

impl HyperBodyTrait for ManagedOutgoingBody {
    type Data = <HyperOutgoingBody as HyperBodyTrait>::Data;
    type Error = <HyperOutgoingBody as HyperBodyTrait>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.inner.as_mut().poll_frame(cx) {
            Poll::Ready(None) => {
                self.signal_complete();
                Poll::Ready(None)
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
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
        let execution_model = detect_component_execution_model(&engine, &module, &config)?;
        Ok(PreparedModule {
            engine,
            module,
            config,
            execution_model,
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
        if self.execution_model != ComponentExecutionModel::WasiCli {
            return Err(PlatformError::runtime(
                "spawn_instance called for non-CLI component; use wasi:http hosting path instead",
            ));
        }
        tracing::info!(app = %self.config.id.0, "spawn_instance called");
        let id = InstanceId::new();
        tracing::info!(instance_id = %id.0, "instance ID created");

        let state = build_store_state(&self.config, env_vars, port, socket_addr_check, None)?;
        let policy_counters = state.policy_enforcer.counters.clone();

        let mut store = Store::new(&self.engine, state);

        // Hook up the resource limiter for memory bounds
        store.limiter(|s| &mut s.limiter);

        // Apply CPU/fuel limits
        configure_store(&mut store, self.config.fuel_quota)?;

        // Link WASI host functions (Component Model Preview 2)
        let linker = build_runtime_linker(&self.engine)?;

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
            policy_counters,
            started_at: Instant::now(),
        })
    }

    pub fn execution_model(&self) -> ComponentExecutionModel {
        self.execution_model
    }

    pub fn spawn_http_server(
        &self,
        env_vars: Vec<(String, String)>,
        addr: SocketAddr,
        socket_addr_check: Option<SocketAddrCheckFn>,
        thread_start_hook: Option<HttpServerThreadStartHook>,
    ) -> Result<HttpServerInstance, PlatformError> {
        if self.execution_model != ComponentExecutionModel::WasiHttpIncomingHandler {
            return Err(PlatformError::runtime(
                "spawn_http_server called for non-wasi:http component",
            ));
        }

        let app_id = self.config.id.clone();
        let config = self.config.clone();
        let engine = self.engine.clone();
        let module = self.module.clone();
        let env_vars = Arc::new(env_vars);
        let socket_addr_check = socket_addr_check.clone();
        let policy = resolve_instance_policy(&self.config, addr.port())?;
        let policy_counters = Arc::new(PolicyCounters::new());
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task_policy_counters = policy_counters.clone();

        let runtime_error_config = config.clone();
        let task = spawn_dedicated_current_thread(
            thread_start_hook,
            move || async move {
                let started_at = Instant::now();
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        return ExecutionStats {
                            instance_id: InstanceId::new(),
                            fuel_limit: config.fuel_quota.0,
                            fuel_consumed: 0,
                            ram_bytes: 0,
                            wall_clock_ms: 0,
                            trap: Some(format!("failed to bind wasi:http adapter listener: {err}")),
                            io_stats: IoStats {
                                open_fds_peak: 0,
                                fs_bytes_written: 0,
                                net_egress_bytes: 0,
                                outbound_connections: 0,
                            },
                        };
                    }
                };

                let linker = match build_async_http_linker(&engine) {
                    Ok(linker) => linker,
                    Err(err) => {
                        return ExecutionStats {
                            instance_id: InstanceId::new(),
                            fuel_limit: config.fuel_quota.0,
                            fuel_consumed: 0,
                            ram_bytes: 0,
                            wall_clock_ms: started_at.elapsed().as_millis() as u64,
                            trap: Some(err.to_string()),
                            io_stats: IoStats {
                                open_fds_peak: 0,
                                fs_bytes_written: 0,
                                net_egress_bytes: 0,
                                outbound_connections: 0,
                            },
                        };
                    }
                };

                let instance_pre = match linker.instantiate_pre(&module) {
                    Ok(pre) => pre,
                    Err(err) => {
                        return ExecutionStats {
                            instance_id: InstanceId::new(),
                            fuel_limit: config.fuel_quota.0,
                            fuel_consumed: 0,
                            ram_bytes: 0,
                            wall_clock_ms: started_at.elapsed().as_millis() as u64,
                            trap: Some(format!(
                                "failed to pre-instantiate wasi:http component: {err}"
                            )),
                            io_stats: IoStats {
                                open_fds_peak: 0,
                                fs_bytes_written: 0,
                                net_egress_bytes: 0,
                                outbound_connections: 0,
                            },
                        };
                    }
                };
                let pre = match ProxyPre::new(instance_pre) {
                    Ok(pre) => pre,
                    Err(err) => {
                        return ExecutionStats {
                            instance_id: InstanceId::new(),
                            fuel_limit: config.fuel_quota.0,
                            fuel_consumed: 0,
                            ram_bytes: 0,
                            wall_clock_ms: started_at.elapsed().as_millis() as u64,
                            trap: Some(format!(
                                "component does not implement wasi:http/proxy world: {err}"
                            )),
                            io_stats: IoStats {
                                open_fds_peak: 0,
                                fs_bytes_written: 0,
                                net_egress_bytes: 0,
                                outbound_connections: 0,
                            },
                        };
                    }
                };
                let pre = Arc::new(pre);
                let mut trap: Option<String> = None;

                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            break;
                        }
                        accept = listener.accept() => {
                            match accept {
                                Ok((client, _peer_addr)) => {
                                    let pre = pre.clone();
                                    let engine = engine.clone();
                                    let config = config.clone();
                                    let env_vars = env_vars.clone();
                                    let socket_addr_check = socket_addr_check.clone();
                                    let request_policy_counters = task_policy_counters.clone();
                                    let app_id = app_id.clone();
                                    tokio::spawn(async move {
                                        let app_id_for_service = app_id.clone();
                                        let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                            let pre = pre.clone();
                                            let engine = engine.clone();
                                            let config = config.clone();
                                            let env_vars = env_vars.clone();
                                            let socket_addr_check = socket_addr_check.clone();
                                            let request_policy_counters = request_policy_counters.clone();
                                            let app_id_for_request = app_id.clone();
                                            async move {
                                                let (sender, receiver) = tokio::sync::oneshot::channel();
                                                let (response_started_tx, response_started_rx) =
                                                    tokio::sync::oneshot::channel::<bool>();
                                                let (body_complete_tx, body_complete_rx) =
                                                    tokio::sync::oneshot::channel::<()>();

                                                let app_id_for_handle = app_id_for_request.clone();
                                                let handle = tokio::spawn(async move {
                                                    let result = async {
                                                        let state = build_store_state(
                                                            &config,
                                                            env_vars.as_ref().clone(),
                                                            addr.port(),
                                                            socket_addr_check.clone(),
                                                            Some(request_policy_counters),
                                                        )
                                                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                                                        let mut store = Store::new(&engine, state);
                                                        store.limiter(|s| &mut s.limiter);
                                                        configure_store(&mut store, config.fuel_quota)
                                                            .map_err(|e| std::io::Error::other(e.to_string()))?;

                                                        let req = req;
                                                        let req = store
                                                            .data_mut()
                                                            .http()
                                                            .new_incoming_request(Scheme::Http, req)
                                                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                                                        let out = store
                                                            .data_mut()
                                                            .http()
                                                            .new_response_outparam(sender)
                                                            .map_err(|e| std::io::Error::other(e.to_string()))?;

                                                        let proxy = pre
                                                            .instantiate_async(&mut store)
                                                            .await
                                                            .map_err(|e| std::io::Error::other(e.to_string()))?;
                                                        proxy
                                                            .wasi_http_incoming_handler()
                                                            .call_handle(&mut store, req, out)
                                                            .await
                                                            .map_err(|e| std::io::Error::other(e.to_string()))?;

                                                        Ok::<(), std::io::Error>(())
                                                    }
                                                    .await;

                                                    if let Ok(true) = response_started_rx.await {
                                                        let _ = body_complete_rx.await;
                                                    }

                                                    if let Err(err) = &result {
                                                        tracing::warn!(
                                                            app = %app_id_for_handle.0,
                                                            error = %err,
                                                            "wasi:http request bridge failed"
                                                        );
                                                    }
                                                    result
                                                });

                                                let response =
                                                    match receiver.await {
                                                        Ok(Ok(resp)) => {
                                                            let _ = response_started_tx.send(true);
                                                            let (parts, body) = resp.into_parts();
                                                            Ok(hyper::Response::from_parts(
                                                                parts,
                                                                ManagedOutgoingBody::new(body, body_complete_tx),
                                                            ))
                                                        }
                                                        Ok(Err(err)) => {
                                                            let _ = response_started_tx.send(false);
                                                            Err(std::io::Error::other(err.to_string()))
                                                        }
                                                        Err(_) => {
                                                            let _ = response_started_tx.send(false);
                                                            match handle.await {
                                                                Ok(Ok(())) => Err(std::io::Error::other(
                                                                    "guest never invoked response-outparam::set",
                                                                )),
                                                                Ok(Err(err)) => Err(err),
                                                                Err(err) => Err(std::io::Error::other(err.to_string())),
                                                            }
                                                        }
                                                    };

                                                if let Err(err) = &response {
                                                    tracing::warn!(app = %app_id_for_request.0, error = %err, "wasi:http response bridge failed");
                                                }

                                                response
                                            }
                                        });

                                        let io = TokioIo::new(client);
                                        let result = http2::Builder::new(TokioExecutor::new())
                                            .serve_connection(io, service)
                                            .await;

                                        if let Err(err) = result {
                                            tracing::warn!(
                                                app = %app_id_for_service.0,
                                                error = %err,
                                                "wasi:http adapter connection failed"
                                            );
                                        }
                                    });
                                }
                                Err(err) => {
                                    trap = Some(format!("wasi:http adapter accept failed: {err}"));
                                    break;
                                }
                            }
                        }
                    }
                }

                ExecutionStats {
                    instance_id: InstanceId::new(),
                    fuel_limit: config.fuel_quota.0,
                    fuel_consumed: 0,
                    ram_bytes: task_policy_counters
                        .current_memory_bytes
                        .load(std::sync::atomic::Ordering::Relaxed)
                        as usize,
                    wall_clock_ms: started_at.elapsed().as_millis() as u64,
                    trap,
                    io_stats: IoStats {
                        open_fds_peak: task_policy_counters
                            .open_fds_peak
                            .load(std::sync::atomic::Ordering::Relaxed),
                        fs_bytes_written: task_policy_counters
                            .fs_write_bytes
                            .load(std::sync::atomic::Ordering::Relaxed),
                        net_egress_bytes: task_policy_counters
                            .egress_bytes
                            .load(std::sync::atomic::Ordering::Relaxed),
                        outbound_connections: task_policy_counters
                            .outbound_connections_total
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as u32,
                    },
                }
            },
            move |err| ExecutionStats {
                instance_id: InstanceId::new(),
                fuel_limit: runtime_error_config.fuel_quota.0,
                fuel_consumed: 0,
                ram_bytes: 0,
                wall_clock_ms: 0,
                trap: Some(format!(
                    "failed to create dedicated wasi:http executor: {err}"
                )),
                io_stats: IoStats {
                    open_fds_peak: 0,
                    fs_bytes_written: 0,
                    net_egress_bytes: 0,
                    outbound_connections: 0,
                },
            },
        );

        Ok(HttpServerInstance {
            task,
            shutdown_tx,
            policy_counters,
            policy,
        })
    }
}

/// Callback invoked once from the dedicated OS thread before its Tokio runtime starts.
///
/// The supervisor uses this point to register the thread's Linux TID in the eBPF
/// identity maps before any application code or network I/O can execute.
pub type HttpServerThreadStartHook = Box<dyn FnOnce() + Send + 'static>;

pub(crate) fn spawn_dedicated_current_thread<T, Factory, Task, OnRuntimeError>(
    thread_start_hook: Option<HttpServerThreadStartHook>,
    task_factory: Factory,
    on_runtime_error: OnRuntimeError,
) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
    Factory: FnOnce() -> Task + Send + 'static,
    Task: Future<Output = T> + 'static,
    OnRuntimeError: FnOnce(std::io::Error) -> T + Send + 'static,
{
    let spawn_result = std::thread::Builder::new()
        .name("wasi-http-instance".to_string())
        .spawn(move || -> Result<T, std::io::Error> {
            if let Some(hook) = thread_start_hook {
                hook();
            }

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            Ok(runtime.block_on(task_factory()))
        });

    match spawn_result {
        Ok(thread) => tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(move || thread.join()).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(std::io::Error::other("dedicated wasi:http thread panicked")),
                Err(error) => Err(std::io::Error::other(format!(
                    "failed to join dedicated wasi:http thread: {error}"
                ))),
            };
            match result {
                Ok(result) => result,
                Err(error) => on_runtime_error(error),
            }
        }),
        Err(err) => tokio::spawn(async move { on_runtime_error(err) }),
    }
}

pub struct HttpServerInstance {
    pub task: tokio::task::JoinHandle<ExecutionStats>,
    pub shutdown_tx: tokio::sync::oneshot::Sender<()>,
    pub policy_counters: Arc<PolicyCounters>,
    pub policy: InstancePolicy,
}

/// An instantiated, running Wasm module.
pub struct RunningInstance {
    pub id: InstanceId,
    instance: Instance,
    store: Store<StoreState>,
    config: AppConfig,
    #[cfg_attr(not(test), allow(dead_code))]
    policy_counters: Arc<PolicyCounters>,
    #[allow(dead_code)]
    started_at: Instant,
}

#[derive(Debug, Clone)]
struct ResolvedEntryPoint {
    name: String,
    func: wasmtime::component::Func,
}

#[derive(Debug, Clone)]
enum ResolvedExecutionModel {
    WasiCliRun(ResolvedEntryPoint),
    TopLevelEntryPoint(ResolvedEntryPoint),
    WasiHttpIncomingHandler { interface_name: String },
    Unknown,
}

fn detect_execution_model_from_instance(
    store: &mut Store<StoreState>,
    instance: &Instance,
) -> ResolvedExecutionModel {
    for interface_name in WASI_CLI_RUN_INTERFACES {
        let interface_idx = instance.get_export_index(&mut *store, None, interface_name);
        let Some(interface_idx) = interface_idx else {
            continue;
        };
        let func_idx = instance.get_export_index(&mut *store, Some(&interface_idx), "run");
        let Some(func_idx) = func_idx else {
            continue;
        };
        if let Some(func) = instance.get_func(&mut *store, func_idx) {
            return ResolvedExecutionModel::WasiCliRun(ResolvedEntryPoint {
                name: format!("{interface_name}#run"),
                func,
            });
        }
    }

    for export_name in top_level_entry_point_candidates() {
        let func_idx = instance.get_export_index(&mut *store, None, export_name);
        let Some(func_idx) = func_idx else {
            continue;
        };
        if let Some(func) = instance.get_func(&mut *store, func_idx) {
            return ResolvedExecutionModel::TopLevelEntryPoint(ResolvedEntryPoint {
                name: export_name.to_string(),
                func,
            });
        }
    }

    for interface_name in WASI_HTTP_INCOMING_HANDLER_INTERFACES {
        let interface_idx = instance.get_export_index(&mut *store, None, interface_name);
        let Some(interface_idx) = interface_idx else {
            continue;
        };
        let func_idx = instance.get_export_index(&mut *store, Some(&interface_idx), "handle");
        if func_idx.is_some() {
            return ResolvedExecutionModel::WasiHttpIncomingHandler {
                interface_name: interface_name.to_string(),
            };
        }
    }

    ResolvedExecutionModel::Unknown
}

fn detect_component_execution_model(
    engine: &Arc<Engine>,
    module: &Component,
    config: &AppConfig,
) -> Result<ComponentExecutionModel, PlatformError> {
    let state = build_store_state(config, Vec::new(), config.wasm_bind_port, None, None)?;
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limiter);
    configure_store(&mut store, config.fuel_quota)?;
    let linker = build_runtime_linker(engine)?;
    let instance = linker.instantiate(&mut store, module).map_err(|e| {
        PlatformError::runtime(format!("instantiation error during model detection: {e}"))
    })?;

    match detect_execution_model_from_instance(&mut store, &instance) {
        ResolvedExecutionModel::WasiCliRun(_) | ResolvedExecutionModel::TopLevelEntryPoint(_) => {
            Ok(ComponentExecutionModel::WasiCli)
        }
        ResolvedExecutionModel::WasiHttpIncomingHandler { .. } => {
            Ok(ComponentExecutionModel::WasiHttpIncomingHandler)
        }
        ResolvedExecutionModel::Unknown => Err(PlatformError::runtime(
            "component export not supported: expected wasi:cli/run, run, _start, or wasi:http/incoming-handler",
        )),
    }
}

impl Drop for RunningInstance {
    fn drop(&mut self) {
        self.store.data().policy_enforcer.reset_active_counters();
        tracing::debug!(instance_id = %self.id.0, "Dropping RunningInstance");
    }
}

impl RunningInstance {
    pub fn policy_counters(&self) -> Arc<PolicyCounters> {
        self.policy_counters.clone()
    }

    fn detect_execution_model(&mut self) -> ResolvedExecutionModel {
        detect_execution_model_from_instance(&mut self.store, &self.instance)
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

        let execution_model = self.detect_execution_model();
        let trap_msg = match execution_model {
            ResolvedExecutionModel::WasiCliRun(entry_point)
            | ResolvedExecutionModel::TopLevelEntryPoint(entry_point) => {
                tracing::info!(entry_point = %entry_point.name, "WASI entry point found and callable");
                tracing::debug!(entry_point = %entry_point.name, "entry point lookup complete");
                tracing::info!(entry_point = %entry_point.name, "entry point lookup result");
                self.invoke_entry_point(&entry_point)
            }
            ResolvedExecutionModel::WasiHttpIncomingHandler { interface_name } => {
                let message = format!(
                    "unsupported component model: {interface_name}#handle detected; current runtime only hosts CLI-style components"
                );
                tracing::error!(instance_id = %self.id.0, interface = %interface_name, "wasi:http component detected but runtime does not host incoming-handler components");
                Some(message)
            }
            ResolvedExecutionModel::Unknown => {
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
            open_fds_peak: counters
                .open_fds_peak
                .load(std::sync::atomic::Ordering::Relaxed),
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
