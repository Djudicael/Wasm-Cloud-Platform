pub mod audit;
pub mod config_validator;
pub mod db_proxy;
pub mod deployment;
pub mod env_resolver;
pub mod instance;
pub mod instance_manager;
pub mod network;
pub mod pool;
pub mod port_alloc;
pub mod scaling;

#[cfg(test)]
mod tests;

use crate::{
    instance::{BillingInfo, ManagedInstance}, network::LocalServiceRegistry, pool::InstancePool,
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
        })
    }

    pub fn set_log_dispatcher(&mut self, log_tx: mpsc::Sender<metrics::WasmLogRecord>) {
        self.log_tx = Some(log_tx);
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
            return Err(PlatformError::Runtime(
                "fuel_quota exceeds maximum allowed (10B units)".into(),
            ));
        }

        // Maximum memory: 512 MB (8192 pages)
        if config.memory_limit.0 > 8192 {
            return Err(PlatformError::Runtime(
                "memory_limit exceeds maximum allowed (512 MB)".into(),
            ));
        }

        // Maximum concurrent instances per app per node: 100
        if config.max_instances > 100 {
            return Err(PlatformError::Runtime(
                "max_instances exceeds node limit (100)".into(),
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

        // 3. Allocate a host port
        let host_port = self.port_alloc.allocate()?;
        let addr = self.port_alloc.socket_addr(host_port);

        // 4. Resolve env vars - note: we pass host_port, not wasm_bind_port
        // The app MUST bind to the allocated port so the proxy can reach it
        let env_vars = (self.env_resolver)(&config, host_port);

        // 5. Spawn the Wasm instance
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let app_id_clone = app_id.clone();

        let prepared_clone = prepared.clone();
        let instance_id = InstanceId(uuid::Uuid::new_v4());

        let task = tokio::task::spawn_blocking(move || {
            // Use the allocated host_port, NOT wasm_bind_port.
            // The app must bind to the allocated port so the proxy can connect.
            let (mut instance, _streams) = prepared_clone
                .spawn_instance(env_vars, host_port)
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

        // 8. Register with local service registry
        self.service_registry.register(app_id, addr).await;

        // Extract billing info before config is moved
        let tenant_id = config.tenant_id.clone().unwrap_or_else(|| {
            app_id.0.split(':').next().unwrap_or(&app_id.0).to_string()
        });
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

        info!(app = %app_id.0, %addr, "instance ready");
        Ok(addr)
    }

    /// Start the background health monitoring loop.
    pub fn start_health_loop(self: Arc<Self>) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
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
        let mut pools = self.pools.write().await;
        let pool = pools
            .get_mut(&app_id.0)
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

        self.kill_instance_internal(pool, app_id, id).await;
        Ok(())
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
                .ok_or_else(|| PlatformError::Runtime(format!("instance {} not found", id.0)))?;

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

    /// Kill all instances of an app immediately.
    pub async fn kill_all_instances(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get_mut(&app_id.0) {
            let instances = std::mem::take(&mut pool.instances);
            for inst in instances {
                if let InstanceState::Ready { addr: _ } | InstanceState::Starting = &inst.state {
                    // Already removed from upstream above in drain_app
                    self.port_alloc.release(inst.addr.port());
                }
                inst.shutdown_tx.send(()).ok();
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
