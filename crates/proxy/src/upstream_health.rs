// crates/proxy/src/upstream_health.rs
use crate::upstream::{AppHealthCheckConfig, UpstreamRegistry};
use std::sync::Arc;

/// Background health checker that probes each app's health endpoint.
pub struct UpstreamHealthChecker {
    registry: Arc<UpstreamRegistry>,
    http_client: reqwest::Client,
}

impl UpstreamHealthChecker {
    pub fn new(registry: Arc<UpstreamRegistry>) -> Self {
        UpstreamHealthChecker {
            registry,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Start the background health check loop.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));

            loop {
                interval.tick().await;
                self.check_all_apps().await;
            }
        })
    }

    /// Check all registered apps.
    async fn check_all_apps(&self) {
        let configs = self.registry.health_configs.read().await.clone();
        let inner = self.registry.inner.read().await;

        for (app_id, config) in configs.iter() {
            // Get the upstream addresses for this app
            if let Some((_, addrs)) = inner.get(app_id) {
                let mut healthy_count = 0u32;
                let total = addrs.len() as u32;

                for addr in addrs {
                    let url = format!("http://{}{}", addr, config.path);
                    match self.probe(&url, config).await {
                        Ok(()) => healthy_count += 1,
                        Err(e) => {
                            tracing::debug!(
                                app_id = app_id,
                                addr = %addr,
                                error = %e,
                                "health check failed for instance"
                            );
                        }
                    }
                }

                // Update the app health registry
                self.registry.app_health_registry.write().await.update(
                    app_id,
                    healthy_count,
                    total,
                );

                // Record the overall result
                let app_healthy = healthy_count > 0;
                self.registry
                    .record_health_result(app_id, app_healthy)
                    .await;
            }
        }
    }

    /// Probe a single health check endpoint.
    async fn probe(&self, url: &str, config: &AppHealthCheckConfig) -> Result<(), String> {
        let resp = self
            .http_client
            .get(url)
            .timeout(config.timeout)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if resp.status().as_u16() == config.expected_status {
            Ok(())
        } else {
            Err(format!(
                "expected status {}, got {}",
                config.expected_status,
                resp.status()
            ))
        }
    }
}
