// crates/proxy/src/health_events.rs
use common::health::NodeHealthStatus;
use messaging::events::Event;
use messaging::NatsBus;
use std::sync::Arc;
use std::time::Duration;

const HEALTH_EVENT_PUBLISH_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

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

            if let Err(e) = self.publish_and_confirm(&event).await {
                tracing::warn!(error = %e, "failed to publish health change event");
                return;
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

    async fn publish_and_confirm(&self, event: &Event) -> Result<(), String> {
        let connection_state = self.bus.client().connection_state();
        if connection_state != async_nats::connection::State::Connected {
            return Err(format!(
                "NATS client is {connection_state}; skipping health transition publish"
            ));
        }

        self.bus.publish(event).await.map_err(|e| e.to_string())?;

        tokio::time::timeout(
            HEALTH_EVENT_PUBLISH_FLUSH_TIMEOUT,
            self.bus.client().flush(),
        )
        .await
        .map_err(|_| {
            format!(
                "timed out waiting {:?} for NATS publish confirmation",
                HEALTH_EVENT_PUBLISH_FLUSH_TIMEOUT
            )
        })?
        .map_err(|e| format!("failed to confirm NATS publish: {e}"))?;

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use e2e::NatsContainer;
    use std::net::TcpListener;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral test port")
            .local_addr()
            .expect("read ephemeral test port")
            .port()
    }

    #[tokio::test]
    async fn test_health_transition_retries_after_publish_failure() {
        let nats = NatsContainer::start(free_port())
            .await
            .expect("start NATS test container");

        let mut failed_bus = nats.connect().await.expect("connect failed bus");
        failed_bus.set_node_id("health-test-node".to_string());

        let publisher = HealthEventPublisher::new(failed_bus, "health-test-node".to_string());

        nats.stop().expect("stop NATS to force publish failure");
        tokio::time::timeout(Duration::from_secs(5), async {
            while publisher.bus.client().connection_state()
                == async_nats::connection::State::Connected
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("wait for disconnected NATS client state");

        publisher
            .on_status_change(
                NodeHealthStatus::Degraded,
                Some("nats unavailable".to_string()),
                3,
                false,
            )
            .await;

        assert_eq!(
            *publisher.last_status.read().await,
            NodeHealthStatus::Healthy,
            "failed publish must not advance the cached last_status"
        );

        nats.resume()
            .await
            .expect("restart NATS after forced failure");

        let observer_bus = nats.connect().await.expect("connect observer bus");
        let wait_for_event = tokio::spawn(async move {
            observer_bus
                .wait_for_event("cluster.health.changed.>")
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut recovered_bus = nats.connect().await.expect("connect recovered bus");
        recovered_bus.set_node_id("health-test-node".to_string());
        let recovered_publisher = HealthEventPublisher {
            bus: recovered_bus,
            node_id: "health-test-node".to_string(),
            last_status: Arc::clone(&publisher.last_status),
        };

        recovered_publisher
            .on_status_change(
                NodeHealthStatus::Degraded,
                Some("nats recovered".to_string()),
                3,
                false,
            )
            .await;

        let event = tokio::time::timeout(Duration::from_secs(5), wait_for_event)
            .await
            .expect("wait for health event task")
            .expect("health event task join")
            .expect("receive health event");

        match event {
            Event::NodeHealthChanged {
                node_id,
                status,
                active_instances,
                accepting_requests,
                ..
            } => {
                assert_eq!(node_id, "health-test-node");
                assert_eq!(status, "degraded");
                assert_eq!(active_instances, 3);
                assert!(!accepting_requests);
            }
            other => panic!("expected NodeHealthChanged event, got {other:?}"),
        }

        assert_eq!(
            *recovered_publisher.last_status.read().await,
            NodeHealthStatus::Degraded,
            "successful publish should advance the cached last_status"
        );
    }
}
