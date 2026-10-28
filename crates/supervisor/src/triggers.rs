//! Periodic node-local delivery. Application databases own distributed leases.
use crate::{
    audit::{write_audit_event, AuditEvent, AuditEventType},
    Supervisor,
};
use common::{
    triggers::{duration, ScheduleTrigger},
    types::{AppId, InstanceState},
};
use metrics::exporter::Metrics;
use proxy::{gateway::circuit_breaker::CircuitBreakerManager, rate_limiter::RateLimiter};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{watch, Semaphore},
    task::JoinSet,
};

struct Entry {
    declaration: ScheduleTrigger,
    bucket: Option<u64>,
    permits: Arc<Semaphore>,
    cancel: watch::Sender<bool>,
}

pub struct Scheduler {
    pub supervisor: Arc<Supervisor>,
    pub metrics: Arc<Metrics>,
    pub rate_limiter: Arc<RateLimiter>,
    pub circuit_breaker: Arc<CircuitBreakerManager>,
}

impl Supervisor {
    /// Terminal for this Supervisor: prevent scheduled work before draining/upgrading.
    pub async fn stop_schedules(&self) {
        self.trigger_stop.send_replace(true);
        let mut idle = self.trigger_idle.subscribe();
        while !*idle.borrow_and_update() {
            if idle.changed().await.is_err() {
                break;
            }
        }
    }

    async fn schedule_is_current(
        &self,
        app: &AppId,
        schedule: &ScheduleTrigger,
    ) -> Result<(), String> {
        if *self.trigger_stop.borrow()
            || self.paused_triggers.read().await.contains(&app.0)
            || self
                .store
                .is_undeployed(&app.0)
                .map_err(|e| e.to_string())?
        {
            return Err("application undeployed or draining".into());
        }
        let config = self
            .store
            .load_config(app)
            .map_err(|e| e.to_string())?
            .ok_or("application config missing")?;
        self.check_resource_limits(&config)
            .map_err(|e| e.to_string())?;
        if !config.triggers.iter().any(|t| t.schedule() == schedule) {
            return Err("schedule removed or changed".into());
        }
        Ok(())
    }

    async fn invoke_schedule(
        &self,
        app: &AppId,
        schedule: ScheduleTrigger,
        scheduled_at_ms: u64,
        addr: std::net::SocketAddr,
    ) -> Result<u16, String> {
        self.schedule_is_current(app, &schedule).await?;
        let handle = {
            let mut pools = self.pools.write().await;
            let pool = pools.get_mut(&app.0).ok_or("instance pool disappeared")?;
            let instance = pool
                .instances
                .iter_mut()
                .find(|i| i.addr == addr && matches!(i.state, InstanceState::Ready { .. }))
                .ok_or("instance no longer ready")?;
            instance.last_request_at = std::time::Instant::now();
            instance.request_count = instance.request_count.saturating_add(1);
            instance
                .trigger_handle
                .clone()
                .ok_or("schedule requires wasi:http")?
        };
        handle
            .invoke(schedule, scheduled_at_ms, self.node_id.clone())
            .await
    }
}

impl Scheduler {
    fn record(&self, app: &AppId, trigger: &str, outcome: &str, count: u64, scheduled_at_ms: u64) {
        self.metrics
            .schedule_events_total
            .with_label_values(&[outcome])
            .inc_by(count);
        write_audit_event(
            "/var/log/wasm-node/audit.jsonl",
            &AuditEvent {
                timestamp: now_ms(),
                node_id: self.supervisor.node_id.clone(),
                event_type: AuditEventType::ScheduleEvent,
                actor: "platform:scheduler".into(),
                app_id: app.0.clone(),
                details: serde_json::json!({ "trigger": trigger, "outcome": outcome, "count": count, "scheduled_at_ms": scheduled_at_ms }),
            },
        );
    }

    /// Cancelling/removing a schedule cancels pending delivery and its native Store.
    /// The fixed global bound also covers slow cold starts across many applications.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        self.supervisor.trigger_idle.send_replace(false);
        let mut terminal = self.supervisor.trigger_stop.subscribe();
        if *terminal.borrow() {
            self.supervisor.trigger_idle.send_replace(true);
            return;
        }
        let mut entries: HashMap<(AppId, String), Entry> = HashMap::new();
        let mut jobs = JoinSet::new();
        let global = Arc::new(Semaphore::new(256));
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                _ = terminal.changed() => break,
                _ = jobs.join_next(), if !jobs.is_empty() => {},
                _ = interval.tick() => {
                    if *stop.borrow() { break; }
                    if let Err(error) = self.tick(&mut entries, &mut jobs, &global).await {
                        // Fail closed: stop outstanding delivery when authoritative state is unavailable.
                        entries.clear();
                        tracing::error!(%error, "schedule reconciliation failed");
                    }
                }
            }
        }
        entries.clear();
        // Cold starts must reach Supervisor ownership before shutdown; cancelling
        // spawn midway could leave an untracked native instance. Native calls
        // observe the closed cancellation channel and drop their Store.
        while jobs.join_next().await.is_some() {}
        self.supervisor.trigger_idle.send_replace(true);
    }

    async fn tick(
        self: &Arc<Self>,
        entries: &mut HashMap<(AppId, String), Entry>,
        jobs: &mut JoinSet<()>,
        global: &Arc<Semaphore>,
    ) -> Result<(), String> {
        let now = now_ms();
        let apps = self
            .supervisor
            .store
            .list_deployed_apps()
            .map_err(|e| e.to_string())?;
        let mut present = HashSet::new();
        for app in apps {
            if self
                .supervisor
                .paused_triggers
                .read()
                .await
                .contains(&app.0)
            {
                continue;
            }
            let config = self
                .supervisor
                .store
                .load_config(&app)
                .map_err(|e| e.to_string())?
                .ok_or("application config disappeared")?;
            common::triggers::validate(&config.triggers)?;
            for trigger in &config.triggers {
                let schedule = trigger.schedule();
                let key = (app.clone(), schedule.id.clone());
                present.insert(key.clone());
                if entries
                    .get(&key)
                    .is_some_and(|e| e.declaration != *schedule)
                {
                    entries.remove(&key);
                }
                let entry = entries.entry(key).or_insert_with(|| {
                    self.record(&app, &schedule.id, "activated", 1, now);
                    Entry {
                        declaration: schedule.clone(),
                        bucket: None,
                        permits: Arc::new(Semaphore::new(schedule.max_concurrency as usize)),
                        cancel: watch::channel(false).0,
                    }
                });
                let bucket = now / duration(&schedule.every)?.as_millis() as u64;
                if entry.bucket.is_some_and(|old| bucket <= old) {
                    continue;
                }
                let tick = self
                    .supervisor
                    .store
                    .advance_trigger_cursor(&self.supervisor.node_id, &app, schedule, now)
                    .map_err(|e| e.to_string())?;
                entry.bucket = Some(bucket);
                let Some(tick) = tick else {
                    continue;
                };
                if tick.missed > 0 {
                    self.record(
                        &app,
                        &schedule.id,
                        "missed",
                        tick.missed,
                        tick.scheduled_at_ms,
                    );
                }
                let Ok(permit) = entry.permits.clone().try_acquire_owned() else {
                    self.record(
                        &app,
                        &schedule.id,
                        "skipped_concurrency",
                        1,
                        tick.scheduled_at_ms,
                    );
                    continue;
                };
                let Ok(global_permit) = global.clone().try_acquire_owned() else {
                    self.record(
                        &app,
                        &schedule.id,
                        "skipped_capacity",
                        1,
                        tick.scheduled_at_ms,
                    );
                    continue;
                };
                let cancel = entry.cancel.subscribe();
                let scheduler = self.clone();
                let app = app.clone();
                let schedule = schedule.clone();
                jobs.spawn(async move {
                    let (_permit, _global_permit) = (permit, global_permit);
                    scheduler
                        .deliver(&app, schedule, tick.scheduled_at_ms, cancel)
                        .await;
                });
            }
        }
        let obsolete: Vec<_> = entries
            .keys()
            .filter(|key| !present.contains(*key))
            .cloned()
            .collect();
        for (app, id) in obsolete {
            entries.remove(&(app.clone(), id.clone()));
            self.supervisor
                .store
                .delete_meta(&storage::triggers::cursor_key(
                    &self.supervisor.node_id,
                    &app,
                    &id,
                ))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn deliver(
        &self,
        app: &AppId,
        schedule: ScheduleTrigger,
        scheduled_at_ms: u64,
        mut cancel: watch::Receiver<bool>,
    ) {
        if cancel.has_changed().unwrap_or(true) {
            return;
        }
        let source_ip = std::net::Ipv4Addr::LOCALHOST.into();
        if self.rate_limiter.check_request(&app.0, source_ip).is_err() {
            self.record(app, &schedule.id, "rate_limited", 1, scheduled_at_ms);
            return;
        }
        if self.circuit_breaker.is_circuit_open(&app.0) {
            self.record(app, &schedule.id, "circuit_open", 1, scheduled_at_ms);
            return;
        }
        // Admission/cold start is allowed to finish so all spawned resources
        // are owned by Supervisor. The declared timeout bounds guest execution.
        if let Err(error) = self.supervisor.schedule_is_current(app, &schedule).await {
            tracing::warn!(%error, "schedule no longer current");
            self.record(app, &schedule.id, "cancelled", 1, scheduled_at_ms);
            return;
        }
        let addr = match self.supervisor.ensure_instance(app).await {
            Ok(addr) => addr,
            Err(error) => {
                tracing::warn!(%error, app = %app.0, "schedule cold start failed");
                self.record(app, &schedule.id, "failure", 1, scheduled_at_ms);
                return;
            }
        };
        self.record(app, &schedule.id, "invoked", 1, scheduled_at_ms);
        let deadline = duration(&schedule.timeout).expect("validated schedule timeout");
        let delivery = tokio::time::timeout(
            deadline,
            self.supervisor
                .invoke_schedule(app, schedule.clone(), scheduled_at_ms, addr),
        );
        let result = tokio::select! {
            biased;
            _ = cancel.changed() => {
                self.record(app, &schedule.id, "cancelled", 1, scheduled_at_ms); return;
            }
            result = delivery => result,
        };
        let outcome = match result {
            Ok(Ok(status)) if (200..300).contains(&status) => {
                self.rate_limiter.record_success(&app.0, source_ip);
                self.circuit_breaker.record_success(&app.0);
                "success"
            }
            Ok(Err(error)) => {
                tracing::warn!(app = %app.0, trigger = %schedule.id, %error, "schedule delivery failed");
                if error == "trigger execution timeout" {
                    "timeout"
                } else {
                    "failure"
                }
            }
            Ok(Ok(_)) => "failure",
            Err(_) => "timeout",
        };
        if outcome != "success" {
            self.circuit_breaker.record_failure(&app.0);
        }
        self.record(app, &schedule.id, outcome, 1, scheduled_at_ms);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{triggers::Trigger, types::AppConfig};

    struct GatedSecrets {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl secrets::SecretProvider for GatedSecrets {
        async fn get(&self, _: &AppId, _: &str) -> Result<String, common::error::PlatformError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok("test-value".into())
        }
        async fn set(
            &self,
            _: &AppId,
            _: &str,
            _: &str,
        ) -> Result<(), common::error::PlatformError> {
            unreachable!()
        }
        async fn delete(&self, _: &AppId, _: &str) -> Result<(), common::error::PlatformError> {
            unreachable!()
        }
        async fn list_keys(&self, _: &AppId) -> Result<Vec<String>, common::error::PlatformError> {
            unreachable!()
        }
    }

    #[tokio::test]
    #[ignore = "build http-hello-component for wasm32-wasip2 and set WCP_HTTP_PROBE_COMPONENT"]
    async fn schedule_cold_start_concurrency_update_undeploy_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let store = storage::Store::open(&dir.path().join("schedule.redb")).unwrap();
        let runtime = runtime::WasmRuntime::new().unwrap();
        let wasm = std::fs::read(std::env::var("WCP_HTTP_PROBE_COMPONENT").unwrap()).unwrap();
        let artifact = runtime.compile(&wasm).unwrap();
        let app = AppId::new("scheduled", "v1");
        let mut config = AppConfig::default_for(app.clone());
        config
            .env_vars
            .insert("TRIGGER_PROBE_DELAY_MS".into(), "250".into());
        let schedule = ScheduleTrigger {
            id: "tick".into(),
            route: "/_platform/triggers/tick".into(),
            method: "POST".into(),
            every: "100ms".into(),
            timeout: "1s".into(),
            max_concurrency: 1,
        };
        config.triggers.push(Trigger::Schedule(schedule));
        store.save_config(&config).unwrap();
        store.store_artifact(&app, &artifact).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.21:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (events, _rx) = tokio::sync::mpsc::channel(32);
        let supervisor = Supervisor::new(
            store.clone(),
            "test-node".into(),
            runtime,
            Arc::new(crate::port_alloc::PortAllocator::new(
                "127.0.0.21".parse().unwrap(),
                port,
                port,
            )),
            Arc::new(proxy::upstream::UpstreamRegistry::new()),
            Arc::new(proxy::router::HostRouter::default()),
            Arc::new(crate::network::LocalServiceRegistry::default()),
            common::INTERNAL_GATEWAY_PORT,
            4 * 1024 * 1024 * 1024,
            Arc::new(|config, _| config.env_vars.clone().into_iter().collect()),
            events,
            None,
        );
        let metrics = Arc::new(Metrics::new());
        // Deterministically hold a cold start after its initial admission
        // check. Undeploy must fence arrivals, wait for that start, and reap it.
        let secrets = Arc::new(GatedSecrets {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        supervisor.set_secret_provider(secrets.clone()).unwrap();
        config.secret_keys.push("RACE_TEST".into());
        store.save_config(&config).unwrap();
        let spawning = {
            let supervisor = supervisor.clone();
            let app = app.clone();
            tokio::spawn(async move { supervisor.spawn(&app).await })
        };
        tokio::time::timeout(Duration::from_secs(5), secrets.entered.notified())
            .await
            .unwrap();
        let removing = {
            let supervisor = supervisor.clone();
            let app = app.clone();
            tokio::spawn(async move { supervisor.undeploy_app(&app).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.is_undeployed(&app.0).unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !removing.is_finished(),
            "undeploy waits for the admitted cold start"
        );
        secrets.release.notify_one();
        tokio::time::timeout(Duration::from_secs(8), spawning)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), removing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(supervisor.list_instances(&app).await.is_empty());
        assert!(supervisor.ensure_instance(&app).await.is_err());
        config.secret_keys.clear();
        store.save_config(&config).unwrap();
        store.mark_deployed(&app.0).unwrap();

        let scheduler = Arc::new(Scheduler {
            supervisor: supervisor.clone(),
            metrics: metrics.clone(),
            rate_limiter: Arc::new(RateLimiter::new(Default::default())),
            circuit_breaker: Arc::new(CircuitBreakerManager::new()),
        });
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(scheduler.run(rx));
        let count = |outcome: &str| {
            metrics
                .schedule_events_total
                .with_label_values(&[outcome])
                .get()
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            while count("success") < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            supervisor.list_instances(&app).await.len(),
            1,
            "scheduler cold-starts the deployed revision"
        );
        assert!(
            count("skipped_concurrency") > 0,
            "slow calls must not create a backlog"
        );
        assert_eq!(count("failure"), 0);

        // Removing a declaration cancels pending delivery, without restarting the server.
        let declared = config.triggers.clone();
        config.triggers.clear();
        store.save_config(&config).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let invoked = count("invoked");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(count("invoked"), invoked);

        // Restore the schedule to exercise undeploy while a handler can be active.
        config.triggers = declared;
        store.save_config(&config).unwrap();
        let successes = count("success");
        tokio::time::timeout(Duration::from_secs(5), async {
            while count("success") <= successes {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        supervisor.undeploy_app(&app).await.unwrap();
        assert!(supervisor.list_instances(&app).await.is_empty());
        assert!(supervisor.ensure_instance(&app).await.is_err());
        assert!(supervisor.spawn(&app).await.is_err());
        tokio::time::sleep(Duration::from_millis(150)).await;
        let invoked = count("invoked");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(count("invoked"), invoked);

        // A restored deployment runs again; process-local draining fences it.
        store.mark_deployed(&app.0).unwrap();
        let successes = count("success");
        tokio::time::timeout(Duration::from_secs(5), async {
            while count("success") <= successes {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        supervisor.drain_app(&app, Duration::ZERO).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let invoked = count("invoked");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(count("invoked"), invoked);
        supervisor.stop_schedules().await;
        drop(stop);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        supervisor.shutdown_all(Duration::from_secs(1)).await;
    }
}
