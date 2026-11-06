//! Generic fenced execution changes and safe runtime projections.
use crate::Supervisor;
use common::{
    error::PlatformError,
    types::{AppConfig, AppId, InstanceState},
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
impl Supervisor {
    /// Restore only persisted operator fences; never clear critical backpressure.
    pub fn restore_node_admission(
        &self,
        signal: &proxy::backpressure::BackpressureSignal,
    ) -> Result<(), PlatformError> {
        let cordoned = self
            .store
            .load_meta("control-node-cordoned")
            .map_err(PlatformError::storage_source)?;
        let draining = self
            .store
            .load_meta("control-node-draining")
            .map_err(PlatformError::storage_source)?;
        signal.set_operator_cordoned(cordoned.as_deref() == Some("true"));
        signal.set_operator_draining(draining.as_deref() == Some("true"));
        Ok(())
    }
    pub async fn node_execution_fence(
        &self,
    ) -> std::collections::BTreeMap<String, BTreeSet<String>> {
        let pools = self.pools.read().await;
        pools
            .iter()
            .map(|(app, p)| {
                (
                    app.clone(),
                    p.instances.iter().map(|i| i.id.0.to_string()).collect(),
                )
            })
            .collect()
    }
    pub async fn change_node_admission_if(
        self: &Arc<Self>,
        expected: &std::collections::BTreeMap<String, BTreeSet<String>>,
        draining: Option<bool>,
        cordoned: Option<bool>,
        backpressure: &proxy::backpressure::BackpressureSignal,
    ) -> Result<bool, PlatformError> {
        let _node_guard = self.node_control_gate.write().await;
        if &self.node_execution_fence().await != expected {
            return Ok(false);
        }
        if let Some(value) = cordoned {
            self.store
                .save_meta(
                    "control-node-cordoned",
                    if value { "true" } else { "false" },
                )
                .map_err(PlatformError::storage_source)?;
            backpressure.set_operator_cordoned(value);
        }
        if let Some(value) = draining {
            self.store
                .save_meta(
                    "control-node-draining",
                    if value { "true" } else { "false" },
                )
                .map_err(PlatformError::storage_source)?;
            backpressure.set_operator_draining(value);
        }
        if draining == Some(true) {
            for (app, instances) in expected {
                if !instances.is_empty()
                    && !self
                        .evict_instances_if(&AppId(app.clone()), instances)
                        .await?
                {
                    return Err(PlatformError::runtime(
                        "execution changed during node drain",
                    ));
                }
            }
        }
        Ok(true)
    }
    pub async fn execution_snapshot(&self, app: &AppId) -> Value {
        let pools = self.pools.read().await;
        let mut instances: Vec<_> = pools.get(&app.0).map(|p| p.instances.iter().map(|i| {
            let state = match i.state {InstanceState::Starting=>"starting",InstanceState::Ready {..}=>"ready",InstanceState::Busy=>"busy",InstanceState::Draining {..}=>"draining",InstanceState::Stopping {..}=>"stopping",InstanceState::ExitTimedOut {..}=>"exit_timed_out",InstanceState::Stopped=>"stopped"};
            let finished=i.task.as_ref().is_some_and(|t| t.is_finished());
            json!({"id":i.id.0,"state":state,"healthy":!finished && matches!(i.state,InstanceState::Ready {..}|InstanceState::Busy),"taskFinished":finished,"ageMs":i.spawned_at.elapsed().as_millis() as u64,"idleMs":i.last_request_at.elapsed().as_millis() as u64,"requestCount":i.request_count})
        }).collect()).unwrap_or_default();
        let count = instances.len();
        let healthy = instances.iter().filter(|i| i["healthy"] == true).count();
        instances.truncate(16);
        json!({"instances":instances,"instanceCount":count,"instancesTruncated":count>16,"healthyInstances":healthy})
    }
    /// Compare the exact live set under the same gate as spawn. Persist the
    /// replacement before detaching the old prepared pool so future cold starts
    /// cannot use stale configuration. Unknown shutdown remains an error.
    pub async fn replace_execution_if(
        self: &Arc<Self>,
        app: &AppId,
        expected: &BTreeSet<String>,
        config: AppConfig,
        compiled: Option<(Vec<u8>, String)>,
        desired: Option<u32>,
    ) -> Result<bool, PlatformError> {
        self.check_resource_limits(&config)?;
        if config.id != *app || expected.len() > 128 {
            return Ok(false);
        }
        let bytes = if let Some((bytes, _)) = &compiled {
            bytes.clone()
        } else {
            self.store
                .load_artifact(app)?
                .ok_or_else(|| PlatformError::AppNotFound(app.0.clone()))?
        };
        let prepared = Arc::new(self.runtime.prepare(&bytes, config.clone())?);
        let gate = self.spawn_gate(app).await;
        let guard = gate.lock_owned().await;
        let instances = {
            let mut pools = self.pools.write().await;
            let actual: BTreeSet<_> = pools
                .get(&app.0)
                .map(|p| p.instances.iter().map(|i| i.id.0.to_string()).collect())
                .unwrap_or_default();
            if &actual != expected {
                return Ok(false);
            }
            if let Some((bytes, hash)) = compiled {
                self.store.persist_deployment(app, &bytes, &config, &hash)?;
            } else {
                self.store.save_config(&config)?;
            }
            if let Some(desired) = desired {
                self.store
                    .save_meta(
                        &format!("control-desired-instances:{}", app.0),
                        &desired.to_string(),
                    )
                    .map_err(PlatformError::storage_source)?;
            }
            let pool = pools
                .entry(app.0.clone())
                .or_insert_with(|| crate::pool::InstancePool {
                    config: config.clone(),
                    prepared: prepared.clone(),
                    instances: Vec::new(),
                });
            pool.config = config;
            pool.prepared = prepared;
            let instances = std::mem::take(&mut pool.instances);
            for instance in &instances {
                if let Some(c) = &instance.policy_counters {
                    c.http_activity.retire();
                }
            }
            instances
        };
        self.finish_control_shutdown(app, instances, guard).await?;
        Ok(true)
    }
    /// The owned task keeps shutdown handles even when an HTTP waiter goes
    /// away. All grace waits run concurrently, bounded by the pool ceiling.
    pub(crate) async fn finish_control_shutdown(
        self: &Arc<Self>,
        app: &AppId,
        instances: Vec<crate::instance::ManagedInstance>,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), PlatformError> {
        let owner = self.clone();
        let app = app.clone();
        tokio::spawn(async move {
            // Preserve both handles and capacity fencing when the HTTP
            // waiter disappears. Starts wait until the old guests are reaped
            // or reinserted in their fenced timed-out state.
            let _guard = guard;
            let mut jobs = tokio::task::JoinSet::new();
            for mut instance in instances {
                owner.upstream_registry.remove(&app, &instance.addr).await;
                owner
                    .service_registry
                    .deregister(&app, &instance.addr)
                    .await;
                jobs.spawn(async move {
                    instance.state = InstanceState::Stopping {
                        addr: instance.addr,
                    };
                    instance.begin_shutdown().await;
                    let outcome = instance.await_exit(Duration::from_secs(2)).await;
                    (instance, outcome)
                });
            }
            let mut failure = None;
            while let Some(result) = jobs.join_next().await {
                match result {
                    Ok((instance, outcome)) => {
                        let id = instance.id.clone();
                        if let Err(e) = owner
                            .handle_shutdown_outcome(&app, &id, instance, outcome)
                            .await
                        {
                            failure = Some(e);
                        }
                    }
                    Err(e) => {
                        failure = Some(PlatformError::runtime(format!(
                            "shutdown worker failed: {e}"
                        )))
                    }
                }
            }
            match failure {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
        .await
        .map_err(|e| PlatformError::runtime(format!("shutdown ownership task failed: {e}")))?
    }
}
