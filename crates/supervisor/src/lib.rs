pub mod config_validator;
pub mod env_resolver;
pub mod instance;
pub mod instance_manager;
pub mod network;
pub mod pool;
pub mod port_alloc;

#[cfg(test)]
mod tests;

use crate::{
    env_resolver::EnvResolver, instance::ManagedInstance, network::LocalServiceRegistry,
    pool::InstancePool, port_alloc::PortAllocator,
};
use common::{
    error::PlatformError,
    types::{AppConfig, AppId, InstanceId, InstanceState},
};
use messaging::events::Event;
use proxy::upstream::UpstreamRegistry;
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

pub struct Supervisor {
    store: Store,
    runtime: WasmRuntime,
    port_alloc: Arc<PortAllocator>,
    upstream_registry: Arc<UpstreamRegistry>,
    service_registry: Arc<LocalServiceRegistry>,
    env_resolver: Arc<dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync>,

    /// Map of app_id → instance pool
    pools: Arc<RwLock<HashMap<String, InstancePool>>>,

    /// Channel to publish events to NATS
    event_tx: mpsc::Sender<Event>,
}

impl Supervisor {
    pub fn new(
        store: Store,
        runtime: WasmRuntime,
        port_alloc: Arc<PortAllocator>,
        upstream_registry: Arc<UpstreamRegistry>,
        service_registry: Arc<LocalServiceRegistry>,
        env_resolver: Arc<dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync>,
        event_tx: mpsc::Sender<Event>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime,
            port_alloc,
            upstream_registry,
            service_registry,
            env_resolver,
            pools: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
        })
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

        // 4. Resolve env vars
        let env_vars = (self.env_resolver)(&config, host_port);

        // 5. Spawn the Wasm instance
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let app_id_clone = app_id.clone();
        let config_clone = config.clone();

        let prepared_clone = prepared.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut instance = prepared_clone
                .spawn_instance(env_vars, config_clone.wasm_bind_port)
                .expect("failed to spawn instance");

            // The run() call blocks until the Wasm module exits or is killed
            let stats = instance.run();
            tracing::info!(
                app = %app_id_clone.0,
                fuel_consumed = stats.fuel_consumed,
                ram_bytes = stats.ram_bytes,
                "instance exited"
            );
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

        let instance_id = InstanceId(uuid::Uuid::new_v4());
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
                    instance_id: id.clone(),
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

    fn node_id(&self) -> String {
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }
}
