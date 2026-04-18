use crate::Supervisor;
use common::error::PlatformError;
use common::types::{AppConfig, AppId};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub async fn hot_swap(
    supervisor: Arc<Supervisor>,
    old_app_id: AppId,
    new_app_id: AppId,
    _new_config: AppConfig,
    drain_timeout: Duration,
) -> Result<(), PlatformError> {
    info!(
        old = %old_app_id.0,
        new = %new_app_id.0,
        "starting hot-swap"
    );

    // 1. Ensure the new version is running (cold-start or pre-warm)
    let new_addr = supervisor.spawn(&new_app_id).await?;
    info!(new = %new_app_id.0, %new_addr, "new version is ready");

    // 2. Wait a moment to confirm stability (optional pre-flight)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Update routes: point all routes for the old app to the new app
    update_routes_for_swap(&supervisor, &old_app_id, &new_app_id).await;

    // 4. Drain old instances (stop sending new requests to them)
    supervisor.drain_app(&old_app_id, drain_timeout).await?;

    // 5. Kill the drained old instances
    supervisor.kill_all_instances(&old_app_id).await?;

    info!(old = %old_app_id.0, "hot-swap complete");
    Ok(())
}

/// Find all routes currently pointing to `old_app_id` and repoint them to `new_app_id`.
///
/// This updates both the in-memory `HostRouter` and the persistent `Store` so that
/// incoming requests are immediately routed to the new version and the change
/// survives a node restart.
async fn update_routes_for_swap(supervisor: &Supervisor, old_app_id: &AppId, new_app_id: &AppId) {
    let routes = match supervisor.store().list_routes() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to list routes during hot-swap — routes NOT updated");
            return;
        }
    };

    let matching: Vec<_> = routes.iter().filter(|r| r.app_id == *old_app_id).collect();

    if matching.is_empty() {
        tracing::info!(
            old = %old_app_id.0,
            "no routes found for old app — nothing to repoint"
        );
        return;
    }

    for route in matching {
        let host = &route.host;
        let prefix = &route.path_prefix;
        let strip = route.strip_prefix;

        // Update in-memory router
        supervisor
            .host_router
            .add_route(host.clone(), prefix.clone(), new_app_id.clone(), strip)
            .await;

        // Update persistent store
        let updated_route = common::types::Route {
            host: host.clone(),
            app_id: new_app_id.clone(),
            path_prefix: prefix.clone(),
            strip_prefix: strip,
            created_at: route.created_at,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Err(e) = supervisor.store().save_route(&updated_route) {
            tracing::error!(
                host,
                error = %e,
                "failed to persist route update during hot-swap"
            );
        }

        tracing::info!(
            host,
            prefix,
            old = %old_app_id.0,
            new = %new_app_id.0,
            "route repointed to new app version"
        );
    }
}

/// Configuration for automatic rollback detection.
pub struct RollbackPolicy {
    /// If trap rate exceeds this fraction within the observation window, rollback.
    /// Default: 0.50 (50% of requests trap).
    pub trap_rate_threshold: f64,

    /// How long to observe the new version before declaring it stable.
    /// Default: 30 seconds.
    pub observation_window: Duration,

    /// Number of consecutive health check failures before rollback.
    /// Default: 3 (= 15 seconds at 5s health tick interval).
    pub health_failure_threshold: u32,

    /// If true, automatic rollback is enabled. Operators can disable per-app.
    pub auto_rollback_enabled: bool,
}

impl Default for RollbackPolicy {
    fn default() -> Self {
        RollbackPolicy {
            trap_rate_threshold: 0.50,
            observation_window: Duration::from_secs(30),
            health_failure_threshold: 3,
            auto_rollback_enabled: true,
        }
    }
}

// CLI / Control Plane
pub async fn rollback(
    supervisor: Arc<Supervisor>,
    current: AppId,
    previous: AppId,
    drain_timeout: Duration,
) -> Result<(), PlatformError> {
    // Verify the previous artifact still exists in redb
    if !supervisor.store().artifact_exists(&previous)? {
        return Err(PlatformError::AppNotFound(format!(
            "rollback target {} not found — artifact may have been garbage collected",
            previous.0
        )));
    }

    // The previous artifact is still in redb (we don't delete old versions immediately)
    // In a real scenario, we would load the old config from redb instead of using default.
    let config = AppConfig::default_for(previous.clone());
    hot_swap(supervisor, current, previous, config, drain_timeout).await
}
