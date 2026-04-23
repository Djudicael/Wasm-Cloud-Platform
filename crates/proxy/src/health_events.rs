// crates/proxy/src/health_events.rs
use common::health::NodeHealthStatus;
use messaging::events::Event;
use messaging::NatsBus;
use std::sync::Arc;

/// Publishes health state changes via NATS.
pub struct HealthEventPublisher {
    bus: NatsBus,
    node_id: String,
    last_status: Arc<tokio::sync::RwLock<NodeHealthStatus>>,
}

impl HealthEventPublisher {
    pub fn new(bus: NatsBus, node_id: String) -> Self {
        HealthEventPublisher {
            bus,
            node_id,
            last_status: Arc::new(tokio::sync::RwLock::new(NodeHealthStatus::Healthy)),
        }
    }

    /// Check if the health status has changed and publish an event if so.
    pub async fn on_status_change(
        &self,
        new_status: NodeHealthStatus,
        cause: Option<String>,
        active_instances: u32,
        accepting_requests: bool,
    ) {
        let mut last = self.last_status.write().await;

        if *last != new_status {
            tracing::info!(
                node_id = %self.node_id,
                old_status = ?*last,
                new_status = ?new_status,
                cause = cause.as_deref().unwrap_or("none"),
                "node health status changed"
            );

            let event = Event::NodeHealthChanged {
                node_id: self.node_id.clone(),
                status: match new_status {
                    NodeHealthStatus::Healthy => "healthy".to_string(),
                    NodeHealthStatus::Degraded => "degraded".to_string(),
                    NodeHealthStatus::Unhealthy => "unhealthy".to_string(),
                },
                cause,
                timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                active_instances,
                accepting_requests,
            };

            if let Err(e) = self.bus.publish(&event).await {
                tracing::warn!(error = %e, "failed to publish health change event");
            }

            *last = new_status;
        }
    }

    /// Publish a periodic health snapshot.
    pub async fn publish_snapshot(
        &self,
        status: NodeHealthStatus,
        active_instances: u32,
        deployed_apps: u32,
        nats_connected: bool,
        disk_free_mb: u64,
        memory_used_mb: u64,
    ) {
        let event = Event::NodeHealthSnapshot {
            node_id: self.node_id.clone(),
            status: match status {
                NodeHealthStatus::Healthy => "healthy".to_string(),
                NodeHealthStatus::Degraded => "degraded".to_string(),
                NodeHealthStatus::Unhealthy => "unhealthy".to_string(),
            },
            active_instances,
            deployed_apps,
            nats_connected,
            disk_free_mb,
            memory_used_mb,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };

        if let Err(e) = self.bus.publish(&event).await {
            tracing::debug!(error = %e, "failed to publish health snapshot");
        }
    }
}

/// Start the background health evaluation loop.
pub fn start_health_loop(
    state: Arc<crate::health::HealthState>,
    publisher: Arc<HealthEventPublisher>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.config.check_interval);
        let mut snapshot_interval = tokio::time::interval(std::time::Duration::from_secs(60));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Evaluate all dependencies
                    let mut dependencies = Vec::new();
                    for checker in state.dependency_checkers.iter() {
                        dependencies.push(checker.check());
                    }

                    let status = crate::health::compute_status_for_probe(
                        &dependencies,
                        common::health::ProbeType::Readiness,
                    );

                    // Find the cause of any unhealthy dependency
                    let cause = dependencies
                        .iter()
                        .find(|d| d.status != common::health::DependencyStatus::Healthy)
                        .map(|d| format!("{}: {}", d.name, d.message));

                    let active_instances = state.instance_count_provider.active_instance_count();
                    let accepting = state.backpressure.is_accepting();

                    publisher.on_status_change(
                        status,
                        cause,
                        active_instances,
                        accepting,
                    ).await;
                }
                _ = snapshot_interval.tick() => {
                    // Publish periodic snapshot
                    let mut dependencies = Vec::new();
                    for checker in state.dependency_checkers.iter() {
                        dependencies.push(checker.check());
                    }

                    let status = crate::health::compute_status_for_probe(
                        &dependencies,
                        common::health::ProbeType::Readiness,
                    );

                    let disk_free_mb = dependencies
                        .iter()
                        .find(|d| d.name == "disk")
                        .and_then(|d| {
                            // Extract the first number from the disk message
                            d.message.split_whitespace()
                                .find_map(|word| word.parse::<u64>().ok())
                        })
                        .unwrap_or(0);

                    let memory_used_mb = dependencies
                        .iter()
                        .find(|d| d.name == "memory")
                        .and_then(|d| d.message.split('/').next())
                        .and_then(|s| s.trim().strip_suffix(" MB"))
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);

                    publisher.publish_snapshot(
                        status,
                        state.instance_count_provider.active_instance_count(),
                        state.instance_count_provider.deployed_app_count(),
                        state.nats_health.is_connected(),
                        disk_free_mb,
                        memory_used_mb,
                    ).await;
                }
            }
        }
    })
}
