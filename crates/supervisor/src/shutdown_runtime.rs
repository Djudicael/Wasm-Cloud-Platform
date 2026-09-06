//! Instance shutdown and reap flow: fence from routing, wait for exit,
//! finalize billing and resource release, and bulk shutdown helpers.

use crate::{
    instance::{ManagedInstance, ShutdownOutcome},
    Supervisor,
};
use common::{
    error::PlatformError,
    types::{AppId, InstanceId, InstanceState},
};
use messaging::events::Event;
use runtime::executor::ExecutionStats;
use std::time::Duration;

impl Supervisor {
    async fn take_instance_for_shutdown(
        &self,
        app_id: &AppId,
        id: &InstanceId,
    ) -> Result<ManagedInstance, PlatformError> {
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

            pool.instances.remove(pos)
        };

        self.upstream_registry.remove(app_id, &instance.addr).await;
        self.service_registry
            .deregister(app_id, &instance.addr)
            .await;

        Ok(instance)
    }

    async fn reinsert_fenced_instance(
        &self,
        app_id: &AppId,
        instance: ManagedInstance,
    ) -> Result<(), PlatformError> {
        let mut pools = self.pools.write().await;
        let pool = pools
            .get_mut(&app_id.0)
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        pool.instances.push(instance);
        Ok(())
    }

    async fn finalize_instance_exit(
        &self,
        app_id: &AppId,
        id: &InstanceId,
        mut instance: ManagedInstance,
        stats: Option<ExecutionStats>,
    ) {
        self.flush_policy_metrics_for_instance(&mut instance);
        instance.state = InstanceState::Stopped;

        let wall_clock_ms = instance.spawned_at.elapsed().as_millis() as u64;
        let (fuel_consumed, ram_bytes, is_trap) = match &stats {
            Some(s) => (s.fuel_consumed, s.ram_bytes as u64, s.trap.is_some()),
            None => (
                instance.billing_info.fuel_quota,
                instance.billing_info.ram_bytes,
                true,
            ),
        };
        self.send_billing_record(billing::BillingInput {
            tenant_id: instance.billing_info.tenant_id.clone(),
            app_id: app_id.0.clone(),
            instance_id: id.0.to_string(),
            node_id: self.node_id().to_string(),
            fuel_consumed,
            fuel_quota: instance.billing_info.fuel_quota,
            ram_bytes,
            wall_clock_ms,
            status_code: if is_trap { 500 } else { 200 },
            is_trap,
        });

        self.service_registry
            .deregister(app_id, &instance.addr)
            .await;
        self.service_registry
            .release_source_port(instance.addr.port())
            .await;
        self.port_alloc.release(instance.addr.port());

        if let Some(tid) = instance.tid {
            if let Some(ns_map) = self.namespace_map() {
                let _ = ns_map.deregister_tid(tid);
            }
        }

        let _ = self
            .event_tx
            .send(Event::InstanceDead {
                app_id: app_id.clone(),
                addr: instance.addr,
                node_id: self.node_id().to_string(),
            })
            .await;

        self.refresh_policy_metric_gauges().await;
    }

    async fn handle_shutdown_outcome(
        &self,
        app_id: &AppId,
        id: &InstanceId,
        mut instance: ManagedInstance,
        outcome: ShutdownOutcome,
    ) -> Result<(), PlatformError> {
        match outcome {
            ShutdownOutcome::Exited(stats) => {
                self.finalize_instance_exit(app_id, id, instance, Some(stats))
                    .await;
                Ok(())
            }
            ShutdownOutcome::TaskPanicked(error) => {
                tracing::warn!(
                    app = %app_id.0,
                    instance = %id.0,
                    error = %error,
                    "instance task panicked during shutdown"
                );
                self.finalize_instance_exit(app_id, id, instance, None)
                    .await;
                Ok(())
            }
            ShutdownOutcome::TimedOut => {
                instance.state = InstanceState::ExitTimedOut {
                    addr: instance.addr,
                };
                self.reinsert_fenced_instance(app_id, instance).await?;
                Err(PlatformError::runtime(format!(
                    "instance {} shutdown timed out; instance remains fenced until exit is confirmed",
                    id.0
                )))
            }
        }
    }

    pub(crate) async fn reap_finished_shutdowns(&self) {
        let candidates: Vec<(AppId, InstanceId)> = {
            let pools = self.pools.read().await;
            pools
                .iter()
                .flat_map(|(app_id_str, pool)| {
                    pool.instances.iter().filter_map(|inst| {
                        let fenced = matches!(
                            inst.state,
                            InstanceState::Draining { .. }
                                | InstanceState::Stopping { .. }
                                | InstanceState::ExitTimedOut { .. }
                        );
                        let finished = inst
                            .task
                            .as_ref()
                            .map(|task| task.is_finished())
                            .unwrap_or(false);
                        if fenced && finished {
                            Some((AppId(app_id_str.clone()), inst.id.clone()))
                        } else {
                            None
                        }
                    })
                })
                .collect()
        };

        for (app_id, instance_id) in candidates {
            let mut instance = match self.take_instance_for_shutdown(&app_id, &instance_id).await {
                Ok(instance) => instance,
                Err(_) => continue,
            };
            let outcome = instance.await_exit(Duration::from_millis(1)).await;
            if let Err(e) = self
                .handle_shutdown_outcome(&app_id, &instance_id, instance, outcome)
                .await
            {
                tracing::warn!(
                    app = %app_id.0,
                    instance = %instance_id.0,
                    error = %e,
                    "failed to reap finished fenced instance"
                );
            }
        }
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

        let mut instance = self.take_instance_for_shutdown(app_id, id).await?;
        instance.state = InstanceState::Draining {
            addr: instance.addr,
        };

        tokio::time::sleep(drain_timeout).await;

        instance.state = InstanceState::Stopping {
            addr: instance.addr,
        };
        instance.begin_shutdown().await;
        let outcome = instance.await_exit(grace_timeout).await;
        self.handle_shutdown_outcome(app_id, id, instance, outcome)
            .await
    }

    pub(crate) async fn kill_instance_internal(&self, app_id: &AppId, id: &InstanceId) {
        let mut instance = match self.take_instance_for_shutdown(app_id, id).await {
            Ok(instance) => instance,
            Err(e) => {
                tracing::warn!(
                    app = %app_id.0,
                    instance = %id.0,
                    error = %e,
                    "failed to take instance for shutdown"
                );
                return;
            }
        };

        instance.state = InstanceState::Stopping {
            addr: instance.addr,
        };
        instance.begin_shutdown().await;
        let outcome = instance.await_exit(Duration::from_secs(5)).await;
        if let Err(e) = self
            .handle_shutdown_outcome(app_id, id, instance, outcome)
            .await
        {
            tracing::warn!(
                app = %app_id.0,
                instance = %id.0,
                error = %e,
                "instance shutdown timed out while killing internally"
            );
        } else {
            tracing::info!(app = %app_id.0, instance = %id.0, "instance killed");
        }
    }

    /// Fence all ready instances for an app from routing, then wait for the
    /// provided timeout so in-flight requests can complete.
    pub async fn drain_app(&self, app_id: &AppId, timeout: Duration) -> Result<(), PlatformError> {
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                for addr in pool.ready_addrs() {
                    self.upstream_registry.remove(app_id, &addr).await;
                }
            }
        }

        tokio::time::sleep(timeout).await;
        Ok(())
    }

    /// Kill every currently tracked instance for the target app.
    pub async fn kill_all_instances(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let instance_ids: Vec<_> = {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                pool.instances.iter().map(|i| i.id.clone()).collect()
            } else {
                Vec::new()
            }
        };

        for id in instance_ids {
            if let Err(e) = self.kill_instance(app_id, &id).await {
                tracing::warn!(
                    app = %app_id.0,
                    instance = %id.0,
                    error = %e,
                    "failed to kill instance"
                );
            }
        }
        Ok(())
    }

    /// Remove an undeployed application's now-empty pool from live state.
    /// Persisted artifacts remain available to the storage GC grace period.
    pub async fn forget_app(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let mut pools = self.pools.write().await;
        if pools
            .get(&app_id.0)
            .is_some_and(|pool| pool.active_count() != 0)
        {
            return Err(PlatformError::runtime(format!(
                "cannot forget app {} while instances are active",
                app_id.0
            )));
        }
        pools.remove(&app_id.0);
        Ok(())
    }

    /// Convert a runtime trap into the standard instance shutdown path.
    pub async fn handle_trap(&self, app_id: &AppId, instance_id: &InstanceId, reason: &str) {
        tracing::error!(
            app = %app_id.0,
            instance = %instance_id.0,
            reason,
            "Wasm trap - killing instance"
        );

        self.kill_instance(app_id, instance_id).await.ok();
    }
}
