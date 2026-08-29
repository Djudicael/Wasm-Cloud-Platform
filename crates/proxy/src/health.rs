// crates/proxy/src/health.rs
use crate::backpressure::BackpressureSignal;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use common::health::{
    AppHealthSummary, DependencyHealth, NodeHealthReport, NodeHealthStatus, ProbeType,
};
use messaging::reconnect::NatsHealth;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

// -----------------------------------------------------------------------------
// Health Response Types
// -----------------------------------------------------------------------------

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: String,
    pub nats_connected: bool,
    pub active_instances: u32,
    pub accepting_requests: bool,
}

// -----------------------------------------------------------------------------
// Health State & Config
// -----------------------------------------------------------------------------

/// Shared state for all health check endpoints.
#[derive(Clone)]
pub struct HealthState {
    pub node_id: String,
    pub nats_health: Arc<NatsHealth>,
    pub backpressure: Arc<BackpressureSignal>,
    pub started_at: Instant,
    pub startup_complete: Arc<std::sync::atomic::AtomicBool>,
    pub instance_count_provider: Arc<dyn InstanceCountProvider + Send + Sync>,
    pub dependency_checkers: Arc<Vec<Box<dyn DependencyChecker + Send + Sync>>>,
    pub app_health_registry: Arc<RwLock<AppHealthRegistry>>,
    pub config: HealthCheckConfig,
}

/// Configuration for health check behavior.
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// Minimum free disk space in bytes (default: 1 GB).
    pub min_disk_free_bytes: u64,
    /// Minimum free filesystem inodes (default: 10,000).
    pub min_disk_free_inodes: u64,
    /// Maximum process memory in bytes (default: 4 GB).
    pub max_memory_bytes: u64,
    /// Number of consecutive failures before marking a dependency unhealthy (default: 3).
    pub failure_threshold: u32,
    /// Number of consecutive successes before marking a dependency healthy (default: 2).
    pub success_threshold: u32,
    /// Interval between background health checks.
    pub check_interval: std::time::Duration,
    /// Timeout for individual dependency checks.
    pub check_timeout: std::time::Duration,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        HealthCheckConfig {
            min_disk_free_bytes: 1024 * 1024 * 1024, // 1 GB
            min_disk_free_inodes: 10_000,
            max_memory_bytes: 4 * 1024 * 1024 * 1024, // 4 GB
            failure_threshold: 3,
            success_threshold: 2,
            check_interval: std::time::Duration::from_secs(10),
            check_timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// Trait for providing the current instance count.
/// Implemented by the Supervisor to avoid coupling health checks to Supervisor internals.
pub trait InstanceCountProvider: Send + Sync {
    fn active_instance_count(&self) -> u32;
    fn deployed_app_count(&self) -> u32;
    fn app_health_summaries(&self) -> Vec<AppHealthSummary>;
}

/// Trait for checking a dependency's health.
pub trait DependencyChecker: Send + Sync {
    fn name(&self) -> &str;
    fn check(&self) -> DependencyHealth;
}

/// Registry of per-app health status, updated by Pingora upstream health checks.
pub struct AppHealthRegistry {
    /// Map of app_id → (healthy_count, total_count).
    apps: std::collections::HashMap<String, (u32, u32)>,
}

impl AppHealthRegistry {
    pub fn new() -> Self {
        AppHealthRegistry {
            apps: std::collections::HashMap::new(),
        }
    }

    /// Update the health count for an app.
    pub fn update(&mut self, app_id: &str, healthy: u32, total: u32) {
        self.apps.insert(app_id.to_string(), (healthy, total));
    }

    /// Get the health summary for a specific app.
    pub fn get(&self, app_id: &str) -> Option<(u32, u32)> {
        self.apps.get(app_id).copied()
    }
}

impl Default for AppHealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------------
// Dependency Checker Implementations
// -----------------------------------------------------------------------------

/// NATS dependency checker.
pub struct NatsDependencyChecker {
    nats_health: Arc<NatsHealth>,
}

impl NatsDependencyChecker {
    pub fn new(nats_health: Arc<NatsHealth>) -> Self {
        NatsDependencyChecker { nats_health }
    }
}

impl DependencyChecker for NatsDependencyChecker {
    fn name(&self) -> &str {
        "nats"
    }

    fn check(&self) -> DependencyHealth {
        self.nats_health.check_health()
    }
}

/// Redb dependency checker.
pub struct RedbDependencyChecker {
    checker: storage::health::RedbHealthChecker,
}

impl RedbDependencyChecker {
    pub fn new(store: storage::Store) -> Self {
        RedbDependencyChecker {
            checker: storage::health::RedbHealthChecker::new(store),
        }
    }
}

impl DependencyChecker for RedbDependencyChecker {
    fn name(&self) -> &str {
        "redb"
    }

    fn check(&self) -> DependencyHealth {
        self.checker.check()
    }
}

/// Disk space dependency checker.
pub struct DiskDependencyChecker {
    db_path: std::path::PathBuf,
    min_free_bytes: u64,
    min_free_inodes: u64,
}

impl DiskDependencyChecker {
    pub fn new(db_path: std::path::PathBuf, min_free_bytes: u64, min_free_inodes: u64) -> Self {
        DiskDependencyChecker {
            db_path,
            min_free_bytes,
            min_free_inodes,
        }
    }
}

impl DependencyChecker for DiskDependencyChecker {
    fn name(&self) -> &str {
        "disk"
    }

    fn check(&self) -> DependencyHealth {
        storage::health::check_disk_space(&self.db_path, self.min_free_bytes, self.min_free_inodes)
    }
}

/// Memory dependency checker.
pub struct MemoryDependencyChecker {
    max_bytes: u64,
}

impl MemoryDependencyChecker {
    pub fn new(max_bytes: u64) -> Self {
        MemoryDependencyChecker { max_bytes }
    }
}

impl DependencyChecker for MemoryDependencyChecker {
    fn name(&self) -> &str {
        "memory"
    }

    fn check(&self) -> DependencyHealth {
        common::health::check_memory(self.max_bytes)
    }
}

// -----------------------------------------------------------------------------
// Router
// -----------------------------------------------------------------------------

pub fn health_router(state: HealthState) -> Router {
    Router::new()
        // Kubernetes-style probe endpoints
        .route("/healthz", get(liveness_probe))
        .route("/readyz", get(readiness_probe))
        .route("/livez", get(startup_probe))
        // Backward-compatible endpoint (maps to readiness)
        .route("/health", get(readiness_probe))
        // Detailed status (for operators and wasm-ctl)
        .route("/status", get(detailed_status))
        // Per-app health
        .route("/status/app/{app_id}", get(app_status))
        .with_state(Arc::new(state))
}

// -----------------------------------------------------------------------------
// Probe Handlers
// -----------------------------------------------------------------------------

/// Startup probe: has the node finished initializing?
/// Returns 503 until startup is complete, then 200 forever.
async fn startup_probe(State(state): State<Arc<HealthState>>) -> Response {
    let startup_complete = state
        .startup_complete
        .load(std::sync::atomic::Ordering::Relaxed);

    if startup_complete {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "node_id": state.node_id,
            })),
        )
            .into_response()
    } else {
        let uptime = state.started_at.elapsed().as_secs();
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "starting",
                "node_id": state.node_id,
                "uptime_secs": uptime,
                "message": "node initialization in progress",
            })),
        )
            .into_response()
    }
}

/// Liveness probe: is the node process alive and functional?
/// Checks: redb readable/writable, memory within limits.
/// Does NOT check: NATS (node can be alive without NATS).
async fn liveness_probe(State(state): State<Arc<HealthState>>) -> Response {
    let mut dependencies = Vec::new();

    // Check each registered dependency
    for checker in state.dependency_checkers.iter() {
        let health = checker.check();
        // Only include "local" dependencies for liveness (not NATS)
        if checker.name() != "nats" {
            dependencies.push(health);
        }
    }

    // Determine overall status based on local dependencies only
    let status = compute_status_for_probe(&dependencies, ProbeType::Liveness);

    let response = NodeHealthReport {
        status,
        node_id: state.node_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state
            .startup_complete
            .load(std::sync::atomic::Ordering::Relaxed),
        accepting_requests: state.backpressure.is_accepting(),
        active_instances: state.instance_count_provider.active_instance_count(),
        deployed_apps: state.instance_count_provider.deployed_app_count(),
        dependencies,
        apps: state.instance_count_provider.app_health_summaries(),
    };

    match status {
        NodeHealthStatus::Healthy | NodeHealthStatus::Degraded => {
            (StatusCode::OK, Json(response)).into_response()
        }
        NodeHealthStatus::Unhealthy => {
            (StatusCode::SERVICE_UNAVAILABLE, Json(response)).into_response()
        }
    }
}

/// Readiness probe: can the node serve traffic?
/// Checks: all liveness checks + NATS connected + not under backpressure.
async fn readiness_probe(State(state): State<Arc<HealthState>>) -> Response {
    let mut dependencies = Vec::new();

    // Check ALL dependencies (including NATS)
    for checker in state.dependency_checkers.iter() {
        dependencies.push(checker.check());
    }

    // Add backpressure as a virtual dependency
    let accepting = state.backpressure.is_accepting();
    dependencies.push(DependencyHealth {
        name: "backpressure".to_string(),
        status: if accepting {
            common::health::DependencyStatus::Healthy
        } else {
            common::health::DependencyStatus::Unhealthy
        },
        message: if accepting {
            "accepting requests".to_string()
        } else {
            "rejecting requests — node at capacity".to_string()
        },
        latency_ms: None,
        last_check: chrono::Utc::now().to_rfc3339(),
    });

    let status = compute_status_for_probe(&dependencies, ProbeType::Readiness);

    let response = NodeHealthReport {
        status,
        node_id: state.node_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state
            .startup_complete
            .load(std::sync::atomic::Ordering::Relaxed),
        accepting_requests: accepting,
        active_instances: state.instance_count_provider.active_instance_count(),
        deployed_apps: state.instance_count_provider.deployed_app_count(),
        dependencies,
        apps: state.instance_count_provider.app_health_summaries(),
    };

    match status {
        NodeHealthStatus::Healthy | NodeHealthStatus::Degraded => {
            (StatusCode::OK, Json(response)).into_response()
        }
        NodeHealthStatus::Unhealthy => {
            (StatusCode::SERVICE_UNAVAILABLE, Json(response)).into_response()
        }
    }
}

/// Detailed status endpoint for operators.
/// Always returns 200 with the full health report, regardless of health state.
async fn detailed_status(State(state): State<Arc<HealthState>>) -> Json<NodeHealthReport> {
    let mut dependencies = Vec::new();

    for checker in state.dependency_checkers.iter() {
        dependencies.push(checker.check());
    }

    dependencies.push(DependencyHealth {
        name: "backpressure".to_string(),
        status: if state.backpressure.is_accepting() {
            common::health::DependencyStatus::Healthy
        } else {
            common::health::DependencyStatus::Unhealthy
        },
        message: if state.backpressure.is_accepting() {
            "accepting requests".to_string()
        } else {
            "rejecting requests — node at capacity".to_string()
        },
        latency_ms: None,
        last_check: chrono::Utc::now().to_rfc3339(),
    });

    let status = compute_status_for_probe(&dependencies, ProbeType::Readiness);

    Json(NodeHealthReport {
        status,
        node_id: state.node_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state
            .startup_complete
            .load(std::sync::atomic::Ordering::Relaxed),
        accepting_requests: state.backpressure.is_accepting(),
        active_instances: state.instance_count_provider.active_instance_count(),
        deployed_apps: state.instance_count_provider.deployed_app_count(),
        dependencies,
        apps: state.instance_count_provider.app_health_summaries(),
    })
}

/// Per-app health status.
async fn app_status(Path(app_id): Path<String>, State(state): State<Arc<HealthState>>) -> Response {
    let registry = state.app_health_registry.read().await;

    match registry.get(&app_id) {
        Some((healthy, total)) => {
            let serving = healthy > 0;
            let summary = AppHealthSummary {
                app_id: app_id.clone(),
                instances: total,
                healthy_instances: healthy,
                serving,
            };
            (StatusCode::OK, Json(summary)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("app '{}' not found", app_id),
            })),
        )
            .into_response(),
    }
}

// -----------------------------------------------------------------------------
// Status Computation
// -----------------------------------------------------------------------------

/// Compute the overall node health status from dependency health reports.
pub fn compute_status_for_probe(
    dependencies: &[DependencyHealth],
    probe_type: ProbeType,
) -> NodeHealthStatus {
    use common::health::DependencyStatus::*;

    let mut has_degraded = false;

    for dep in dependencies {
        match dep.status {
            Unhealthy => {
                // For liveness, only "local" dependencies matter (redb, memory, disk).
                // NATS being unhealthy does not make the node "not alive".
                // For readiness, any unhealthy dependency makes the node unhealthy.
                match probe_type {
                    ProbeType::Liveness => {
                        if dep.name != "nats" && dep.name != "backpressure" {
                            return NodeHealthStatus::Unhealthy;
                        }
                        // NATS/backpressure unhealthy on liveness → degraded, not unhealthy
                        has_degraded = true;
                    }
                    ProbeType::Readiness | ProbeType::Startup => {
                        return NodeHealthStatus::Unhealthy;
                    }
                }
            }
            Degraded | Unknown => {
                has_degraded = true;
            }
            Healthy => {}
        }
    }

    if has_degraded {
        NodeHealthStatus::Degraded
    } else {
        NodeHealthStatus::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_status_healthy() {
        let deps = vec![DependencyHealth {
            name: "redb".to_string(),
            status: common::health::DependencyStatus::Healthy,
            message: "ok".to_string(),
            latency_ms: None,
            last_check: "".to_string(),
        }];
        assert_eq!(
            compute_status_for_probe(&deps, ProbeType::Readiness),
            NodeHealthStatus::Healthy
        );
    }

    #[test]
    fn test_compute_status_degraded_nats_on_liveness() {
        let deps = vec![DependencyHealth {
            name: "nats".to_string(),
            status: common::health::DependencyStatus::Unhealthy,
            message: "down".to_string(),
            latency_ms: None,
            last_check: "".to_string(),
        }];
        // Liveness: NATS down → degraded, not unhealthy
        assert_eq!(
            compute_status_for_probe(&deps, ProbeType::Liveness),
            NodeHealthStatus::Degraded
        );
        // Readiness: NATS down → unhealthy
        assert_eq!(
            compute_status_for_probe(&deps, ProbeType::Readiness),
            NodeHealthStatus::Unhealthy
        );
    }

    #[test]
    fn test_compute_status_unhealthy_local() {
        let deps = vec![DependencyHealth {
            name: "redb".to_string(),
            status: common::health::DependencyStatus::Unhealthy,
            message: "corrupted".to_string(),
            latency_ms: None,
            last_check: "".to_string(),
        }];
        assert_eq!(
            compute_status_for_probe(&deps, ProbeType::Liveness),
            NodeHealthStatus::Unhealthy
        );
    }
}
