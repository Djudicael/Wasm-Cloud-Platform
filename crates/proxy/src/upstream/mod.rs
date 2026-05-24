// crates/proxy/src/upstream/mod.rs
use common::types::AppId;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamEndpoint {
    pub addr: SocketAddr,
    pub h2c: bool,
}

impl std::fmt::Display for UpstreamEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

type UpstreamMap = HashMap<String, (AtomicUsize, Vec<UpstreamEndpoint>)>;

/// Per-app health check configuration.
#[derive(Debug, Clone)]
pub struct AppHealthCheckConfig {
    /// The path to probe (e.g., "/health").
    pub path: String,
    /// Expected HTTP status code (default: 200).
    pub expected_status: u16,
    /// Interval between health checks.
    pub interval: std::time::Duration,
    /// Timeout for each health check request.
    pub timeout: std::time::Duration,
    /// Number of consecutive failures before marking unhealthy (default: 3).
    pub failure_threshold: u32,
    /// Number of consecutive successes before marking healthy (default: 2).
    pub success_threshold: u32,
}

impl Default for AppHealthCheckConfig {
    fn default() -> Self {
        AppHealthCheckConfig {
            path: "/health".to_string(),
            expected_status: 200,
            interval: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(5),
            failure_threshold: 3,
            success_threshold: 2,
        }
    }
}

impl AppHealthCheckConfig {
    /// Create from the AppConfig's health_check_path field.
    pub fn from_app_config(config: &common::types::AppConfig) -> Option<Self> {
        config
            .health_check_path
            .as_ref()
            .map(|path| AppHealthCheckConfig {
                path: path.clone(),
                ..Default::default()
            })
    }
}

#[derive(Debug, Clone)]
struct AppHealthState {
    consecutive_successes: u32,
    consecutive_failures: u32,
    healthy: bool,
}

/// Thread-safe registry of all live instance addresses, per app.
#[derive(Clone, Default)]
pub struct UpstreamRegistry {
    /// app_id → (round-robin counter, list of addresses)
    pub inner: Arc<RwLock<UpstreamMap>>,
    /// Per-app health check configuration.
    pub(crate) health_configs: Arc<RwLock<HashMap<String, AppHealthCheckConfig>>>,
    /// Per-app health state.
    health_state: Arc<RwLock<HashMap<String, AppHealthState>>>,
    /// Callback to update the app health registry.
    pub app_health_registry: Arc<RwLock<crate::health::AppHealthRegistry>>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a new upstream instance address for the given app.
    pub async fn add(&self, app_id: &AppId, endpoint: UpstreamEndpoint) {
        let mut map = self.inner.write().await;
        let entry = map
            .entry(app_id.0.clone())
            .or_insert_with(|| (AtomicUsize::new(0), Vec::new()));
        if !entry
            .1
            .iter()
            .any(|existing| existing.addr == endpoint.addr)
        {
            entry.1.push(endpoint);
            tracing::info!(
                app = %app_id.0,
                addr = %endpoint.addr,
                h2c = endpoint.h2c,
                "upstream added"
            );
        }
    }

    /// Remove an upstream instance address for the given app.
    /// If the address list becomes empty, the entry is removed from the map entirely.
    pub async fn remove(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.inner.write().await;
        if let Some(entry) = map.get_mut(&app_id.0) {
            entry.1.retain(|endpoint| endpoint.addr != *addr);
            tracing::info!(app = %app_id.0, %addr, "upstream removed");
            // Remove the app entry entirely if no addresses remain
            if entry.1.is_empty() {
                map.remove(&app_id.0);
                tracing::info!(app = %app_id.0, "app removed from upstream registry (no instances left)");
            }
        }
    }

    /// Get the next upstream address using round-robin.
    /// Returns None if no instances are available (cold start needed).
    pub async fn next(&self, app_id: &AppId) -> Option<UpstreamEndpoint> {
        let map = self.inner.read().await;
        let (counter, addrs) = map.get(&app_id.0)?;
        if addrs.is_empty() {
            return None;
        }
        let idx = counter.fetch_add(1, Ordering::Relaxed) % addrs.len();
        Some(addrs[idx])
    }

    /// Get the number of live upstream instances for the given app.
    pub async fn count(&self, app_id: &AppId) -> usize {
        let map = self.inner.read().await;
        map.get(&app_id.0).map(|(_, v)| v.len()).unwrap_or(0)
    }

    // ── Health Check Integration ─────────────────────────────────────

    /// Register health check configuration for an app.
    pub async fn register_health_check(&self, app_id: &str, config: AppHealthCheckConfig) {
        self.health_configs
            .write()
            .await
            .insert(app_id.to_string(), config);
        self.health_state.write().await.insert(
            app_id.to_string(),
            AppHealthState {
                consecutive_successes: 0,
                consecutive_failures: 0,
                healthy: true, // Assume healthy until proven otherwise
            },
        );
    }

    /// Remove health check configuration for an app.
    pub async fn remove_health_check(&self, app_id: &str) {
        self.health_configs.write().await.remove(app_id);
        self.health_state.write().await.remove(app_id);
    }

    /// Record a health check result for an app instance.
    pub async fn record_health_result(&self, app_id: &str, success: bool) {
        let mut states = self.health_state.write().await;
        let configs = self.health_configs.read().await;

        if let (Some(state), Some(config)) = (states.get_mut(app_id), configs.get(app_id)) {
            if success {
                state.consecutive_successes += 1;
                state.consecutive_failures = 0;

                if state.consecutive_successes >= config.success_threshold && !state.healthy {
                    tracing::info!(app_id = app_id, "app health check: now HEALTHY");
                    state.healthy = true;
                }
            } else {
                state.consecutive_failures += 1;
                state.consecutive_successes = 0;

                if state.consecutive_failures >= config.failure_threshold && state.healthy {
                    tracing::warn!(app_id = app_id, "app health check: now UNHEALTHY");
                    state.healthy = false;
                }
            }
        }
    }

    /// Check if an app is currently healthy (has at least one healthy instance).
    pub async fn is_app_healthy(&self, app_id: &str) -> bool {
        self.health_state
            .read()
            .await
            .get(app_id)
            .map(|s| s.healthy)
            .unwrap_or(true) // Default to healthy if no health check configured
    }

    /// Get the next healthy upstream address for an app.
    /// Skips instances that have failed health checks.
    pub async fn next_healthy(&self, app_id: &AppId) -> Option<UpstreamEndpoint> {
        // If no health check is configured, use round-robin as before
        if !self.health_configs.read().await.contains_key(&app_id.0) {
            return self.next(app_id).await;
        }

        // Only return addresses for healthy apps
        if self.is_app_healthy(&app_id.0).await {
            self.next(app_id).await
        } else {
            tracing::warn!(app_id = %app_id.0, "skipping unhealthy app");
            None
        }
    }
}

#[cfg(test)]
mod tests;
