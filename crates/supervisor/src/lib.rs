pub mod audit;
pub mod config_validator;
pub mod db_proxy;
pub mod deployment;
pub mod env_resolver;
pub mod instance;
pub mod instance_manager;
pub mod network;
pub mod network_interceptor;
pub mod pool;
pub mod port_alloc;
pub mod scaling;

#[cfg(test)]
mod tests;

use crate::{
    instance::{BillingInfo, ManagedInstance},
    network::LocalServiceRegistry,
    network_interceptor::{ConnectDecision, NetworkInterceptor},
    pool::InstancePool,
    port_alloc::PortAllocator,
};
use common::{
    error::PlatformError,
    types::{AppConfig, AppId, InstanceId, InstanceState},
};
use messaging::events::Event;
use proxy::{router::HostRouter, upstream::UpstreamRegistry};
use runtime::{executor::PreparedModule, WasmRuntime};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use storage::Store;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

// ── Supervisor Command Interface ──────────────────────────────────────────────

/// Commands that external subsystems (eBPF monitor, admin API) can send
/// to the supervisor for immediate action.
///
/// These commands are processed asynchronously by the supervisor's command
/// loop, which runs in a background Tokio task started via
/// [`Supervisor::start_command_loop`].
#[derive(Debug)]
pub enum SupervisorCommand {
    /// Kill the instance consuming the most memory.
    /// Used when eBPF detects critical memory pressure or OOM.
    KillLargestInstance { reason: String },

    /// Kill all idle instances (those with no recent requests).
    /// Used when eBPF detects FD exhaustion approaching hard limit.
    PruneIdleInstances {
        /// Only prune instances idle for more than this many seconds.
        idle_threshold_secs: u64,
    },

    /// Remove an app's instances from the upstream routing table.
    /// Used when eBPF detects a process exit (the instance is dead).
    RemoveAppFromUpstream { app_id: AppId },

    /// Kill a specific instance by its app_id and instance_id.
    KillInstance {
        app_id: AppId,
        instance_id: InstanceId,
        reason: String,
    },
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub struct Supervisor {
    store: Store,
    runtime: WasmRuntime,
    port_alloc: Arc<PortAllocator>,
    upstream_registry: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    service_registry: Arc<LocalServiceRegistry>,
    env_resolver: Arc<dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync>,

    /// Map of app_id → instance pool
    pools: Arc<RwLock<HashMap<String, InstancePool>>>,

    /// Channel to publish events to NATS
    event_tx: mpsc::Sender<Event>,

    /// Channel to send log records
    log_tx: Option<mpsc::Sender<metrics::WasmLogRecord>>,

    /// Channel to send billing records
    billing_tx: Option<mpsc::Sender<billing::BillingInput>>,

    /// Channel to receive commands from eBPF monitor and other subsystems.
    /// Wrapped in `Mutex` so the command loop can take it out via `Option::take`
    /// even though `Supervisor` is behind `Arc`.
    command_rx: std::sync::Mutex<Option<mpsc::Receiver<SupervisorCommand>>>,

    /// Sender clone for providing to eBPF monitor callbacks and admin API.
    command_tx: mpsc::Sender<SupervisorCommand>,

    /// Watch receiver for the health-check interval (hot-reloadable).
    /// When the operator changes `health.check_interval_secs` via the admin
    /// API, the sender side is updated and this receiver picks up the new
    /// value on the next loop iteration.
    health_interval_rx: Option<tokio::sync::watch::Receiver<u64>>,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn new(
        store: Store,
        runtime: WasmRuntime,
        port_alloc: Arc<PortAllocator>,
        upstream_registry: Arc<UpstreamRegistry>,
        host_router: Arc<HostRouter>,
        service_registry: Arc<LocalServiceRegistry>,
        env_resolver: Arc<dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync>,
        event_tx: mpsc::Sender<Event>,
        billing_tx: Option<mpsc::Sender<billing::BillingInput>>,
    ) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::channel::<SupervisorCommand>(256);
        Arc::new(Self {
            store,
            runtime,
            port_alloc,
            upstream_registry,
            host_router,
            service_registry,
            env_resolver,
            pools: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            log_tx: None,
            billing_tx,
            command_rx: std::sync::Mutex::new(Some(command_rx)),
            command_tx,
            health_interval_rx: None,
        })
    }

    /// Set the watch receiver for the health-check interval.
    ///
    /// Call this after construction, before `start_health_loop()`.
    /// The sender side is typically held by the config sync task in
    /// `main.rs` which reads from `HotConfigHandle`.
    pub fn set_health_interval_rx(&mut self, rx: tokio::sync::watch::Receiver<u64>) {
        self.health_interval_rx = Some(rx);
    }

    pub fn set_log_dispatcher(&mut self, log_tx: mpsc::Sender<metrics::WasmLogRecord>) {
        self.log_tx = Some(log_tx);
    }

    /// Get a sender for the supervisor command channel.
    ///
    /// This can be used by the eBPF monitor callbacks, admin API handlers,
    /// or any other subsystem that needs to request immediate supervisor
    /// actions (kill largest instance, prune idle, etc.).
    pub fn command_tx(&self) -> mpsc::Sender<SupervisorCommand> {
        self.command_tx.clone()
    }

    fn send_billing_record(&self, input: billing::BillingInput) {
        if let Some(ref tx) = self.billing_tx {
            if let Err(e) = tx.try_send(input) {
                warn!(error = %e, "billing channel full — dropping record");
            }
        }
    }

    fn node_id(&self) -> String {
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }

    pub fn check_resource_limits(&self, config: &AppConfig) -> Result<(), PlatformError> {
        // Maximum fuel quota: 10 billion units (prevents absurdly long compute)
        if config.fuel_quota.0 > 10_000_000_000 {
            return Err(PlatformError::runtime(
                "fuel_quota exceeds maximum allowed (10B units)",
            ));
        }

        // Maximum memory: 512 MB (8192 pages)
        if config.memory_limit.0 > 8192 {
            return Err(PlatformError::runtime(
                "memory_limit exceeds maximum allowed (512 MB)",
            ));
        }

        // Maximum concurrent instances per app per node: 100
        if config.max_instances > 100 {
            return Err(PlatformError::runtime(
                "max_instances exceeds node limit (100)",
            ));
        }

        Ok(())
    }

    /// Ensure at least one instance is available for the given app.
    pub async fn ensure_instance(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                if let Some(addr) = pool.ready_addrs().first() {
                    return Ok(*addr);
                }
            }
        }
        self.spawn(app_id).await
    }

    /// Spawn a new instance for the given app.
    /// Returns the SocketAddr where the instance is listening.
    pub async fn spawn(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        let (config, prepared) = {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                (pool.config.clone(), pool.prepared.clone())
            } else {
                let config = self
                    .store
                    .load_config(app_id)?
                    .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

                // 1. Load or compile the artifact
                let artifact = self.store.load_artifact(app_id)?.ok_or_else(|| {
                    PlatformError::AppNotFound(format!("no artifact for {}", app_id.0))
                })?;

                // 2. Prepare the module
                let prepared = Arc::new(self.runtime.prepare(&artifact, config.clone())?);

                (config, prepared)
            }
        };

        // Build a qualified AppId that includes the namespace from config.
        let version = app_id.bare_name().split(':').nth(1).unwrap_or("v1");
        let qualified_app_id =
            AppId::new_namespaced(&config.namespace, app_id.bare_app_name(), version);

        tracing::info!(
            app_id = %app_id.0,
            config_namespace = %config.namespace,
            qualified_app_id = %qualified_app_id.0,
            "[SPAWN] Building qualified AppId"
        );

        // 3. Allocate a host port
        let host_port = self.port_alloc.allocate()?;
        let addr = self.port_alloc.socket_addr(host_port);

        // 4. Resolve env vars - note: we pass host_port, not wasm_bind_port
        let mut env_vars = (self.env_resolver)(&config, host_port);

        // 4b. Inject service discovery env vars for other running apps in the same namespace
        let target_namespace = qualified_app_id.namespace();
        let ns_services = self
            .service_registry
            .get_namespace_services(target_namespace)
            .await;

        tracing::info!(
            target_namespace = %target_namespace,
            ns_services_count = ns_services.len(),
            "[SPAWN] Namespace services query"
        );

        for (bare_app_name, addrs) in &ns_services {
            // Skip self (don't inject env vars for own service)
            if bare_app_name == &app_id.bare_app_name() {
                continue;
            }
            if let Some(addr) = addrs.first() {
                let key = format!(
                    "{}_SERVICE_URL",
                    bare_app_name.to_uppercase().replace('-', "_")
                );

                let unqualified = format!("{}:v1", bare_app_name);
                let qualified =
                    AppId::new_namespaced(qualified_app_id.namespace(), bare_app_name, "v1").0;
                let gateway_config = self
                    .store
                    .load_gateway_config(&unqualified)
                    .ok()
                    .flatten()
                    .or_else(|| self.store.load_gateway_config(&qualified).ok().flatten());
                let has_endpoint_rules = gateway_config
                    .map(|cfg| !cfg.endpoints.is_empty())
                    .unwrap_or(false);

                let url = if has_endpoint_rules {
                    // Route through the internal gateway. Namespace isolation
                    // relies on service discovery: the Supervisor only injects
                    // service URLs for same-namespace apps. The gateway port
                    // is open to all namespaces.
                    format!(
                        "http://{}.{}.internal:{}",
                        bare_app_name,
                        qualified_app_id.namespace(),
                        common::INTERNAL_GATEWAY_PORT
                    )
                } else {
                    format!("http://127.0.0.1:{}", addr.port())
                };

                tracing::info!(
                    key = %key,
                    url = %url,
                    app_id = %app_id.0,
                    has_endpoint_rules,
                    "[SPAWN] Injecting service discovery env var"
                );
                env_vars.retain(|(k, _)| k != &key);
                env_vars.push((key, url));
            }
        }

        // 4c. Build socket address checker for namespace-aware outbound filtering.
        // This is called by wasmtime-wasi for every socket connect/bind. It blocks
        // connections to unknown loopback ports (defense in depth) and allows
        // connections to known same-namespace apps and the internal gateway.
        let allowed_ports = {
            let mut ports = std::collections::HashSet::new();
            // Allow connections to the internal gateway port
            ports.insert(common::INTERNAL_GATEWAY_PORT);
            for addrs in ns_services.values() {
                for addr in addrs {
                    ports.insert(addr.port());
                }
            }
            ports.insert(host_port); // own bind port
            ports
        };

        let registry = self.service_registry.clone();
        let source_app = qualified_app_id.clone();
        let socket_addr_check: runtime::executor::SocketAddrCheckFn = Box::new(
            move |dest: std::net::SocketAddr, use_type: runtime::executor::SocketAddrUse| {
                let allowed = allowed_ports.clone();
                let registry = registry.clone();
                let source_app = source_app.clone();
                Box::pin(async move {
                    tracing::info!(
                        source_app = %source_app.0,
                        dest = %dest,
                        use_type = ?use_type,
                        "[SOCKET DEBUG] socket_addr_check called"
                    );
                    match use_type {
                        runtime::executor::SocketAddrUse::TcpConnect
                        | runtime::executor::SocketAddrUse::UdpConnect => {
                            if !dest.ip().is_loopback() {
                                tracing::info!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] external connection — allowed"
                                );
                                return true;
                            }

                            // Block connections to unknown loopback ports
                            if !allowed.contains(&dest.port()) {
                                tracing::warn!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] BLOCKED: unknown loopback port"
                                );
                                return false;
                            }

                            // Defense-in-depth cross-namespace check for known app ports.
                            // Internal gateway port is allowed without namespace check.
                            // The gateway port (9080) is open to all namespaces —
                            // namespace isolation relies on service discovery only.
                            if dest.port() != common::INTERNAL_GATEWAY_PORT {
                                let interceptor =
                                    NetworkInterceptor::new(registry, source_app.clone());
                                match interceptor
                                    .check_connect(
                                        std::net::SocketAddr::new(
                                            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                                127, 0, 0, 1,
                                            )),
                                            0,
                                        ),
                                        dest,
                                    )
                                    .await
                                {
                                    ConnectDecision::Allow(_) => {
                                        tracing::info!(
                                            dest = %dest,
                                            "[SOCKET DEBUG] same-namespace connection — allowed"
                                        );
                                        true
                                    }
                                    ConnectDecision::Deny { reason } => {
                                        tracing::warn!(
                                            dest = %dest,
                                            reason,
                                            "[SOCKET DEBUG] BLOCKED: cross-namespace"
                                        );
                                        false
                                    }
                                }
                            } else {
                                tracing::info!(
                                    dest = %dest,
                                    "[SOCKET DEBUG] internal gateway port — allowed"
                                );
                                true
                            }
                        }
                        runtime::executor::SocketAddrUse::TcpBind
                        | runtime::executor::SocketAddrUse::UdpBind => {
                            let ok = allowed.contains(&dest.port());
                            tracing::info!(
                                dest = %dest,
                                allowed = ok,
                                "[SOCKET DEBUG] bind check"
                            );
                            ok
                        }
                        _ => {
                            tracing::info!(
                                use_type = ?use_type,
                                "[SOCKET DEBUG] other socket use — allowed"
                            );
                            true
                        }
                    }
                })
            },
        );

        // 4d. INTERNAL_APP_ID is NOT injected into the app's environment.
        // Per the "Blind App" principle, the app should never know its own
        // namespace. The socket_addr_check blocks cross-namespace connections
        // to direct app ports, but the gateway port is open to all namespaces.
        // Namespace isolation currently relies on service discovery filtering.
        // When wasmtime-wasi provides hooks for wrapping TCP output streams,
        // the Host will transparently inject identity metadata (e.g. a signed
        // JWT) without the app's involvement.

        // 5. Spawn the Wasm instance
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let app_id_clone = app_id.clone();

        let prepared_clone = prepared.clone();
        let instance_id = InstanceId(uuid::Uuid::new_v4());

        let task = tokio::task::spawn_blocking(move || {
            // Use the allocated host_port, NOT wasm_bind_port.
            // The app must bind to the allocated port so the proxy can connect.
            let (mut instance, _streams) = prepared_clone
                .spawn_instance(env_vars, host_port, Some(socket_addr_check))
                .expect("failed to spawn instance");

            // Note: WASI stdout/stderr streams are handled via inherit_* in the runtime
            // The eprintln! output goes to stderr of the wasm-node process

            // The run() call blocks until the Wasm module exits or is killed
            let stats = instance.run();
            if let Some(ref trap) = stats.trap {
                tracing::error!(
                    app = %app_id_clone.0,
                    fuel_consumed = stats.fuel_consumed,
                    ram_bytes = stats.ram_bytes,
                    trap = %trap,
                    "instance crashed with trap"
                );
            } else {
                tracing::info!(
                    app = %app_id_clone.0,
                    fuel_consumed = stats.fuel_consumed,
                    ram_bytes = stats.ram_bytes,
                    "instance exited"
                );
            }
            stats
        });

        // 6. Wait for the TCP port to be ready
        if let Err(e) = crate::instance::wait_for_ready(addr, Duration::from_millis(500)).await {
            self.port_alloc.release(host_port);
            return Err(e);
        }

        // 7. Register with the proxy upstream table
        self.upstream_registry.add(app_id, addr).await;

        // 8. Register with local service registry (using qualified AppId)
        self.service_registry
            .register(&qualified_app_id, addr)
            .await;

        // 8b. Register source port for network interceptor attribution
        self.service_registry
            .bind_source_port(host_port, qualified_app_id.clone())
            .await;

        // 8c. Build virtual DNS for this instance (available for future WASI DNS hooks)
        let mut virtual_dns =
            runtime::virtual_dns::VirtualDns::new(qualified_app_id.namespace().to_string());
        for (bare_name, _) in &ns_services {
            if bare_name != qualified_app_id.bare_app_name() {
                virtual_dns.register_service(bare_name);
            }
        }
        tracing::debug!(
            app = %app_id.0,
            records = ?virtual_dns,
            "virtual DNS configured for instance"
        );

        // Extract billing info before config is moved
        let tenant_id = config
            .tenant_id
            .clone()
            .unwrap_or_else(|| app_id.0.split(':').next().unwrap_or(&app_id.0).to_string());
        let fuel_quota = config.fuel_quota.0;
        let ram_bytes = config.memory_limit.to_bytes() as u64;

        let managed = ManagedInstance {
            id: instance_id.clone(),
            app_id: app_id.clone(),
            addr,
            state: InstanceState::Ready { addr },
            spawned_at: Instant::now(),
            last_request_at: Instant::now(),
            request_count: 0,
            task,
            shutdown_tx,
            billing_info: BillingInfo {
                tenant_id: tenant_id.clone(),
                fuel_quota,
                ram_bytes,
            },
        };

        {
            let mut pools = self.pools.write().await;
            let pool = pools
                .entry(app_id.0.clone())
                .or_insert_with(|| InstancePool {
                    config,
                    prepared,
                    instances: Vec::new(),
                });
            pool.instances.push(managed);
        }

        // 9. Publish READY event to NATS
        let _ = self
            .event_tx
            .send(Event::InstanceReady {
                app_id: app_id.clone(),
                addr,
                node_id: self.node_id(),
            })
            .await;

        // TODO(step-33): eBPF coordination — the instance's host PID should be
        // registered in the eBPF MONITORED_PIDS map so the kernel-level monitor
        // can enforce per-process limits (defense in depth behind WASI layer).
        // Additionally, the PolicyCounters from the instance's PolicyEnforcer
        // should be exported to the eBPF metrics pipeline so that kernel-level
        // observations (e.g. actual syscall counts) can be cross-referenced with
        // WASI-layer denials. This is an integration gap, not a library limitation
        // — both the eBPF monitor (Step 30) and the PolicyEnforcer exist, they
        // just need to be wired together. The PID is available from the
        // spawn_blocking task via std::thread::current().id() or by tracking the
        // tokio task's thread after it starts.

        info!(app = %app_id.0, %addr, "instance ready");
        Ok(addr)
    }

    /// Start the background health monitoring loop.
    /// Start the periodic health-check loop.
    ///
    /// If `health_interval_rx` was set via [`set_health_interval_rx`], the
    /// loop reads the interval from the watch channel on every tick and
    /// resets the timer when it changes — no restart required.
    ///
    /// If no watch receiver was provided, a default 5-second interval is
    /// used (backward-compatible behaviour).
    pub fn start_health_loop(self: Arc<Self>) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            // Determine the initial interval
            let initial_secs = supervisor
                .health_interval_rx
                .as_ref()
                .map(|rx| *rx.borrow())
                .unwrap_or(5);
            let mut last_secs = initial_secs;
            let mut interval = tokio::time::interval(Duration::from_secs(initial_secs));

            loop {
                interval.tick().await;

                // Check for interval updates from the watch channel
                if let Some(ref rx) = supervisor.health_interval_rx {
                    let new_secs = *rx.borrow();
                    if new_secs != last_secs && new_secs > 0 {
                        tracing::info!(
                            old_secs = last_secs,
                            new_secs,
                            "health loop interval updated via hot-reload"
                        );
                        interval = tokio::time::interval(Duration::from_secs(new_secs));
                        interval.tick().await; // consume the first immediate tick
                        last_secs = new_secs;
                    }
                }

                if let Err(e) = supervisor.health_tick().await {
                    error!(error = %e, "health tick failed");
                }
            }
        });
    }

    async fn health_tick(&self) -> Result<(), PlatformError> {
        let mut pools = self.pools.write().await;
        for (app_id_str, pool) in pools.iter_mut() {
            let app_id = AppId(app_id_str.clone());
            let mut dead_ids = Vec::new();

            for inst in &pool.instances {
                if let InstanceState::Ready { addr } = &inst.state {
                    let alive = tokio::net::TcpStream::connect(addr).await.is_ok();
                    if !alive {
                        warn!(app = app_id_str, %addr, "instance not responding, marking dead");
                        dead_ids.push(inst.id.clone());
                    }
                }
            }

            for id in dead_ids {
                self.kill_instance_internal(pool, &app_id, &id).await;
            }

            let idle_ids = pool.idle_instance_ids(pool.config.idle_timeout_secs);
            for id in idle_ids {
                self.kill_instance_internal(pool, &app_id, &id).await;
            }
        }
        Ok(())
    }

    pub async fn maybe_scale_up(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let pool_info = {
            let pools = self.pools.read().await;
            pools
                .get(&app_id.0)
                .map(|p| (p.active_count(), p.config.max_instances))
        };
        if let Some((active, max)) = pool_info {
            if active < max as usize {
                self.spawn(app_id).await?;
            }
        }
        Ok(())
    }

    pub async fn kill_instance(
        &self,
        app_id: &AppId,
        id: &InstanceId,
    ) -> Result<(), PlatformError> {
        self.kill_instance_gracefully(app_id, id, Duration::ZERO, Duration::from_secs(2))
            .await
    }

    /// Gracefully shutdown an instance with drain timeout.
    /// Steps:
    /// 1. Remove from upstream registry (no new requests)
    /// 2. Try HTTP /_platform/shutdown endpoint
    /// 3. Wait drain_timeout for in-flight requests
    /// 4. Wait for task to exit (with grace_timeout)
    /// 5. Release resources
    pub async fn kill_instance_gracefully(
        &self,
        app_id: &AppId,
        id: &InstanceId,
        drain_timeout: Duration,
        grace_timeout: Duration,
    ) -> Result<(), PlatformError> {
        tracing::info!(
            app = %app_id.0,
            instance = %id.0,
            drain = ?drain_timeout,
            "starting graceful shutdown"
        );

        // 1. Remove from upstream registry first (stop new requests)
        let instance = {
            let mut pools = self.pools.write().await;
            let pool = pools
                .get_mut(&app_id.0)
                .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

            let pos = pool
                .instances
                .iter()
                .position(|i| i.id == *id)
                .ok_or_else(|| PlatformError::runtime(format!("instance {} not found", id.0)))?;

            // Remove from upstream immediately
            let inst = &pool.instances[pos];
            if let InstanceState::Ready { addr } = &inst.state {
                self.upstream_registry.remove(app_id, addr).await;
                tracing::debug!(
                    app = %app_id.0,
                    instance = %id.0,
                    addr = %addr,
                    "removed from upstream registry"
                );
            }

            pool.instances.remove(pos)
        };

        // 2. Wait for in-flight requests to drain
        tokio::time::sleep(drain_timeout).await;

        // 3. Save data we need after consuming instance
        let addr = instance.addr;
        let state = instance.state.clone();
        let billing_info = instance.billing_info.clone();
        let wall_clock_start = instance.spawned_at;
        // 4. Initiate graceful shutdown (HTTP + channel signal + wait)
        let stats = instance.initiate_shutdown(grace_timeout).await;

        // 5. Record billing for this execution cycle
        let wall_clock_ms = wall_clock_start.elapsed().as_millis() as u64;
        let (fuel_consumed, ram_bytes, is_trap) = match &stats {
            Some(s) => (s.fuel_consumed, s.ram_bytes as u64, s.trap.is_some()),
            None => (billing_info.fuel_quota, billing_info.ram_bytes, true),
        };
        self.send_billing_record(billing::BillingInput {
            tenant_id: billing_info.tenant_id,
            app_id: app_id.0.clone(),
            instance_id: id.0.to_string(),
            node_id: self.node_id(),
            fuel_consumed,
            fuel_quota: billing_info.fuel_quota,
            ram_bytes,
            wall_clock_ms,
            status_code: if is_trap { 500 } else { 200 },
            is_trap,
        });

        // 6. Release resources
        if let InstanceState::Ready { addr } = &state {
            self.service_registry.deregister(app_id, addr).await;
            self.service_registry.release_source_port(addr.port()).await;
            self.port_alloc.release(addr.port());
        }

        // 7. Publish InstanceDead event
        let _ = self
            .event_tx
            .send(Event::InstanceDead {
                app_id: app_id.clone(),
                addr,
                node_id: self.node_id(),
            })
            .await;

        if stats.is_some() {
            tracing::info!(
                app = %app_id.0,
                instance = %id.0,
                "graceful shutdown complete"
            );
        } else {
            tracing::warn!(
                app = %app_id.0,
                instance = %id.0,
                "graceful shutdown timeout — instance was aborted"
            );
        }

        Ok(())
    }

    async fn kill_instance_internal(
        &self,
        pool: &mut InstancePool,
        app_id: &AppId,
        id: &InstanceId,
    ) {
        if let Some(pos) = pool.instances.iter().position(|i| i.id == *id) {
            let inst = pool.instances.remove(pos);
            if let InstanceState::Ready { addr } = &inst.state {
                self.upstream_registry.remove(app_id, addr).await;
                self.service_registry.deregister(app_id, addr).await;
                self.service_registry.release_source_port(addr.port()).await;
                self.port_alloc.release(addr.port());
            }
            inst.shutdown_tx.send(()).ok();

            let _ = self
                .event_tx
                .send(Event::InstanceDead {
                    app_id: app_id.clone(),
                    addr: inst.addr,
                    node_id: self.node_id(),
                })
                .await;

            tracing::info!(app = %app_id.0, instance = %id.0, "instance killed");
        }
    }

    pub async fn restore_from_storage(&self) -> Result<(), PlatformError> {
        let app_ids = self.store.list_apps()?;
        let mut pools = self.pools.write().await;

        for app_id in app_ids {
            let config = self
                .store
                .load_config(&app_id)?
                .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

            if self.store.artifact_exists(&app_id)? {
                info!(app = %app_id.0, "restored app from storage (waiting for first request)");
                pools.insert(
                    app_id.0.clone(),
                    InstancePool {
                        config,
                        prepared: Arc::new(self.get_prepared(&app_id).await?),
                        instances: Vec::new(),
                    },
                );
            } else {
                warn!(app = %app_id.0, "no compiled artifact found, skipping");
            }
        }
        Ok(())
    }

    async fn get_prepared(&self, app_id: &AppId) -> Result<PreparedModule, PlatformError> {
        let config = self
            .store
            .load_config(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        let artifact = self
            .store
            .load_artifact(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(format!("no artifact: {}", app_id.0)))?;
        self.runtime.prepare(&artifact, config)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn upstream(&self) -> &Arc<UpstreamRegistry> {
        &self.upstream_registry
    }

    pub async fn list_instances(&self, app_id: &AppId) -> Vec<InstanceId> {
        let pools = self.pools.read().await;
        if let Some(pool) = pools.get(&app_id.0) {
            pool.instances.iter().map(|i| i.id.clone()).collect()
        } else {
            vec![]
        }
    }

    pub async fn node_stats(&self) -> Result<scaling::NodeStats, PlatformError> {
        let mut total_instances = 0;
        let mut app_counts = HashMap::new();

        let pools = self.pools.read().await;
        for (app_id, pool) in pools.iter() {
            let count = pool.active_count();
            if count > 0 {
                app_counts.insert(app_id.clone(), count);
                total_instances += count;
            }
        }

        Ok(scaling::NodeStats {
            cpu_percent: 0.0, // placeholder
            fuel_per_sec: 0,  // placeholder
            total_instances,
            app_counts,
        })
    }

    /// Mark all instances of an app as DRAINING.
    /// Removes them from the upstream table (no new requests),
    /// then waits for in-flight requests to complete.
    pub async fn drain_app(&self, app_id: &AppId, timeout: Duration) -> Result<(), PlatformError> {
        // Remove from Pingora's upstream table immediately
        // (Pingora will stop routing new requests to this app's old instances)
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                for addr in pool.ready_addrs() {
                    self.upstream_registry.remove(app_id, &addr).await;
                }
            }
        }

        // Wait for in-flight requests to drain
        // Proxy-side: Pingora will wait for existing connections to close naturally.
        // We give a hard deadline.
        tokio::time::sleep(timeout).await;
        Ok(())
    }

    /// Kill all instances of an app immediately and record billing.
    pub async fn kill_all_instances(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let instance_ids: Vec<_> = {
            let mut pools = self.pools.write().await;
            if let Some(pool) = pools.get_mut(&app_id.0) {
                pool.instances.iter().map(|i| i.id.clone()).collect()
            } else {
                Vec::new()
            }
        };

        for id in instance_ids {
            if let Err(e) = self.kill_instance(app_id, &id).await {
                tracing::warn!(app = %app_id.0, instance = %id.0, error = %e, "failed to kill instance");
            }
        }
        Ok(())
    }

    /// Called when Wasmtime raises a Trap (OOM / out of fuel / illegal instruction).
    pub async fn handle_trap(&self, app_id: &AppId, instance_id: &InstanceId, reason: &str) {
        tracing::error!(
            app = %app_id.0,
            instance = %instance_id.0,
            reason,
            "Wasm trap — killing instance"
        );

        // 1. Kill the instance
        self.kill_instance(app_id, instance_id).await.ok();

        // 2. Increment trap counter in metrics
        // (handled by metrics module — see step 11)

        // 3. If trap rate exceeds threshold, suspend the app
        // (see step 12: scaling)
    }

    /// List all app IDs currently managed by the supervisor.
    pub async fn list_app_ids(&self) -> Vec<AppId> {
        let pools = self.pools.read().await;
        pools.keys().map(|k| AppId(k.clone())).collect()
    }

    /// Get all registered service addresses for service discovery.
    pub fn get_service_registry(&self) -> Arc<LocalServiceRegistry> {
        self.service_registry.clone()
    }

    // ── Command Loop (for eBPF monitor / admin API integration) ───────────

    /// Start the background command processing loop.
    ///
    /// Listens for [`SupervisorCommand`] messages on the command channel
    /// and dispatches them to the appropriate supervisor methods. This
    /// enables the eBPF monitor and admin API to request immediate actions
    /// (kill instances, prune idle, etc.) without needing direct `Arc<Supervisor>`
    /// access or async runtime handles.
    ///
    /// Should be called once during startup, after `start_health_loop`.
    pub fn start_command_loop(self: Arc<Self>) {
        let rx = self.command_rx.lock().unwrap().take();
        if rx.is_none() {
            warn!("supervisor command loop already started — ignoring duplicate call");
            return;
        }
        let mut rx = rx.unwrap();

        let supervisor = self.clone();
        tokio::spawn(async move {
            info!("supervisor command loop started");
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    SupervisorCommand::KillLargestInstance { reason } => {
                        supervisor.kill_largest_instance(&reason).await;
                    }
                    SupervisorCommand::PruneIdleInstances {
                        idle_threshold_secs,
                    } => {
                        supervisor.prune_idle_instances(idle_threshold_secs).await;
                    }
                    SupervisorCommand::RemoveAppFromUpstream { app_id } => {
                        let pools = supervisor.pools.read().await;
                        if let Some(pool) = pools.get(&app_id.0) {
                            for addr in pool.ready_addrs() {
                                supervisor.upstream_registry.remove(&app_id, &addr).await;
                            }
                        }
                        info!(app = %app_id.0, "removed app from upstream via command");
                    }
                    SupervisorCommand::KillInstance {
                        app_id,
                        instance_id,
                        reason,
                    } => {
                        info!(
                            app = %app_id.0,
                            instance = %instance_id.0,
                            reason,
                            "killing instance via supervisor command"
                        );
                        if let Err(e) = supervisor.kill_instance(&app_id, &instance_id).await {
                            warn!(
                                app = %app_id.0,
                                instance = %instance_id.0,
                                error = %e,
                                "failed to kill instance via command"
                            );
                        }
                    }
                }
            }
            warn!("supervisor command loop exited — no more senders");
        });
    }

    /// Kill the instance consuming the most memory.
    ///
    /// Scans all instance pools and finds the ready instance with the
    /// highest `ram_bytes` in its billing info. Used by the eBPF monitor
    /// when critical memory pressure or OOM is detected.
    pub async fn kill_largest_instance(&self, reason: &str) {
        let mut largest: Option<(AppId, InstanceId, u64)> = None;

        let pools = self.pools.read().await;
        for (app_id_str, pool) in pools.iter() {
            for inst in &pool.instances {
                if matches!(inst.state, InstanceState::Ready { .. }) {
                    let ram = inst.billing_info.ram_bytes;
                    if largest.is_none() || ram > largest.as_ref().unwrap().2 {
                        largest = Some((AppId(app_id_str.clone()), inst.id.clone(), ram));
                    }
                }
            }
        }
        drop(pools);

        match largest {
            Some((app_id, instance_id, ram)) => {
                warn!(
                    app = %app_id.0,
                    instance = %instance_id.0,
                    ram_bytes = ram,
                    reason,
                    "killing largest instance (memory pressure recovery)"
                );
                if let Err(e) = self.kill_instance(&app_id, &instance_id).await {
                    warn!(error = %e, "failed to kill largest instance");
                }
            }
            None => {
                info!(reason, "no instances to kill for memory pressure recovery");
            }
        }
    }

    /// Kill all idle instances across all apps.
    ///
    /// An instance is considered idle if it hasn't received a request in
    /// more than `idle_threshold_secs` seconds. Used by the eBPF monitor
    /// when FD exhaustion is approaching the hard limit.
    pub async fn prune_idle_instances(&self, idle_threshold_secs: u64) {
        let mut total_pruned = 0usize;

        let app_ids = {
            let pools = self.pools.read().await;
            pools.keys().cloned().collect::<Vec<_>>()
        };

        for app_id_str in app_ids {
            let app_id = AppId(app_id_str.clone());
            let idle_ids = {
                let pools = self.pools.read().await;
                pools
                    .get(&app_id_str)
                    .map(|p| p.idle_instance_ids(idle_threshold_secs))
                    .unwrap_or_default()
            };

            for instance_id in idle_ids {
                info!(
                    app = %app_id.0,
                    instance = %instance_id.0,
                    idle_threshold_secs,
                    "pruning idle instance (FD pressure recovery)"
                );
                if let Err(e) = self.kill_instance(&app_id, &instance_id).await {
                    warn!(
                        app = %app_id.0,
                        instance = %instance_id.0,
                        error = %e,
                        "failed to prune idle instance"
                    );
                } else {
                    total_pruned += 1;
                }
            }
        }

        if total_pruned > 0 {
            info!(total_pruned, idle_threshold_secs, "pruned idle instances");
        } else {
            info!(idle_threshold_secs, "no idle instances to prune");
        }
    }

    /// Gracefully shutdown all instances across all apps.
    /// Used during node shutdown (SIGTERM).
    pub async fn shutdown_all(&self, _timeout: Duration) {
        tracing::info!("shutting down all instances");

        let app_ids = self.list_app_ids().await;
        for app_id in app_ids {
            // Drain app (remove from upstream)
            if let Err(e) = self.drain_app(&app_id, Duration::from_secs(5)).await {
                tracing::warn!(app = %app_id.0, error = %e, "drain failed");
            }

            // Gracefully kill all instances
            let instance_ids: Vec<InstanceId> = {
                let pools = self.pools.read().await;
                pools
                    .get(&app_id.0)
                    .map(|p| p.instances.iter().map(|i| i.id.clone()).collect())
                    .unwrap_or_default()
            };

            for instance_id in instance_ids {
                if let Err(e) = self
                    .kill_instance_gracefully(
                        &app_id,
                        &instance_id,
                        Duration::from_secs(2), // drain timeout
                        Duration::from_secs(5), // grace timeout
                    )
                    .await
                {
                    tracing::warn!(
                        app = %app_id.0,
                        instance = %instance_id.0,
                        error = %e,
                        "graceful shutdown failed"
                    );
                }
            }
        }

        tracing::info!("all instances shutdown complete");
    }
}

// -----------------------------------------------------------------------------
// InstanceCountProvider — bridges Supervisor to health checks
// -----------------------------------------------------------------------------

use common::health::AppHealthSummary;
use proxy::health::InstanceCountProvider;

impl InstanceCountProvider for Supervisor {
    fn active_instance_count(&self) -> u32 {
        match self.pools.try_read() {
            Ok(pools) => pools.values().map(|p| p.instance_count() as u32).sum(),
            Err(_) => 0,
        }
    }

    fn deployed_app_count(&self) -> u32 {
        match self.pools.try_read() {
            Ok(pools) => pools.len() as u32,
            Err(_) => 0,
        }
    }

    fn app_health_summaries(&self) -> Vec<AppHealthSummary> {
        match self.pools.try_read() {
            Ok(pools) => pools
                .iter()
                .map(|(app_id, pool)| {
                    let instances = pool.instance_count() as u32;
                    AppHealthSummary {
                        app_id: app_id.clone(),
                        instances,
                        healthy_instances: instances,
                        serving: instances > 0,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}
