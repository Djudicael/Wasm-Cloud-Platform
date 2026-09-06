//! Health checks, idle/dead instance cleanup, scale-up decisions, and
//! cold restore of app state from persistent storage.

use crate::{pool::InstancePool, Supervisor};
use common::{
    error::PlatformError,
    types::{AppId, InstanceId, InstanceState},
};
use runtime::executor::PreparedModule;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tracing::{error, info, warn};

impl Supervisor {
    /// Start the periodic health-check loop. The interval is hot-reloadable
    /// through the optional watch receiver configured at startup.
    pub fn start_health_loop(self: Arc<Self>) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let initial_secs = supervisor
                .health_interval_rx
                .as_ref()
                .map(|rx| *rx.borrow())
                .unwrap_or(5);
            let mut last_secs = initial_secs;
            let mut interval = tokio::time::interval(Duration::from_secs(initial_secs));

            loop {
                interval.tick().await;

                if let Some(ref rx) = supervisor.health_interval_rx {
                    let new_secs = *rx.borrow();
                    if new_secs != last_secs && new_secs > 0 {
                        tracing::info!(
                            old_secs = last_secs,
                            new_secs,
                            "health loop interval updated via hot-reload"
                        );
                        interval = tokio::time::interval(Duration::from_secs(new_secs));
                        interval.tick().await;
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
        if let Some(policy_metrics) = self
            .policy_metrics
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().cloned())
        {
            let mut pools = self.pools.write().await;
            Self::export_policy_metrics_from_pools(&policy_metrics, &mut pools);
        }

        if let Some(ns_map) = self.namespace_map() {
            let removed = ns_map.cleanup_stale_tids();
            if removed > 0 {
                tracing::info!(removed, "Cleaned up stale TIDs from namespace map");
            }
        }

        let health_checks: Vec<(String, InstanceId, SocketAddr)>;
        let idle_by_app: Vec<(String, AppId, Vec<InstanceId>)>;

        {
            let pools = self.pools.read().await;
            health_checks = pools
                .iter()
                .flat_map(|(app_id_str, pool)| {
                    pool.instances.iter().filter_map(|inst| match &inst.state {
                        InstanceState::Ready { addr } => {
                            Some((app_id_str.clone(), inst.id.clone(), *addr))
                        }
                        _ => None,
                    })
                })
                .collect();

            idle_by_app = pools
                .iter()
                .map(|(app_id_str, pool)| {
                    (
                        app_id_str.clone(),
                        AppId(app_id_str.clone()),
                        pool.idle_instance_ids(pool.config.idle_timeout_secs),
                    )
                })
                .collect();
        }

        let mut dead_by_app: HashMap<String, Vec<InstanceId>> = HashMap::new();
        for (app_id_str, instance_id, addr) in &health_checks {
            let alive = tokio::net::TcpStream::connect(addr).await.is_ok();
            if !alive {
                warn!(app = app_id_str, %addr, "instance not responding, marking dead");
                dead_by_app
                    .entry(app_id_str.clone())
                    .or_default()
                    .push(instance_id.clone());
            }
        }

        for (app_id_str, app_id, idle_ids) in idle_by_app {
            if let Some(dead_ids) = dead_by_app.remove(&app_id_str) {
                for id in dead_ids {
                    self.kill_instance_internal(&app_id, &id).await;
                }
            }

            for id in idle_ids {
                self.kill_instance_internal(&app_id, &id).await;
            }
        }

        self.reap_finished_shutdowns().await;

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

    /// Rehydrate app pools from persisted config and prepared artifacts so
    /// apps can cold-start again on first request after a node restart.
    pub async fn restore_from_storage(&self) -> Result<(), PlatformError> {
        let app_ids = self.store.list_deployed_apps()?;
        let mut pools = self.pools.write().await;

        for app_id in app_ids {
            let config = self
                .store
                .load_config(&app_id)?
                .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
            self.check_resource_limits(&config)?;

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
}
