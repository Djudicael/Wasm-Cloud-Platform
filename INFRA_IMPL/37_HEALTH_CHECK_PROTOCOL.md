# Step 37 — Health Check Protocol

## Goal
Implement a comprehensive health check protocol that distinguishes between liveness,
readiness, and startup states, checks all critical dependencies, and provides the
information that external load balancers, DNS providers, and cluster operators need
to make routing decisions. The system must:
- Expose three distinct probe endpoints: `/healthz` (liveness), `/readyz` (readiness),
  and `/livez` (startup)
- Check all critical dependencies (NATS, redb, disk space, memory) with configurable
  timeouts
- Report per-app health by probing each app's `health_check_path` via Pingora's
  upstream health checking
- Distinguish between "healthy", "degraded", and "unhealthy" states with clear semantics
- Provide a detailed JSON response format that external load balancers and DNS providers
  can parse
- Publish health state changes via NATS for cluster-wide visibility
- Expose health check results as Prometheus metrics
- Support configurable health check intervals, timeouts, and failure thresholds
- Integrate with the backpressure signal (Step 24) and NATS health (Step 08)
- Replace the current hardcoded `active_instances: 0` with real data from the Supervisor
- Require no external health check agent — the node is self-sufficient

---

## Context & Rationale

### The Problem This Solves

The current health check endpoint (`/health`) has several critical gaps:

**1. No liveness vs. readiness distinction.** A single `/health` endpoint conflates two
different questions:
- "Is this node process alive?" (liveness — should the orchestrator restart it?)
- "Is this node ready to serve traffic?" (readiness — should the load balancer route
  to it?)

A node that has lost its NATS connection is still alive (it can serve cached responses
in degraded mode) but may not be ready (it cannot receive new deployments). Restarting
it would not help — the NATS connection would still be down. But removing it from the
load balancer pool is correct, because new deployments won't reach it.

**2. `active_instances` is hardcoded to 0.** The current `HealthResponse` struct has
`active_instances: 0` — it never reports the actual number of running Wasm instances.
An operator cannot tell from the health endpoint whether the node is doing any work.

**3. No dependency depth checking.** The current endpoint checks NATS connectivity and
backpressure, but not:
- Whether redb is writable (a full disk makes redb read-only)
- Whether disk space is above a minimum threshold
- Whether the artifact server is responding
- Whether any Wasm instances are trapped or unhealthy

**4. No per-app health checking.** The `AppConfig` struct has a `health_check_path`
field (e.g., `"/health"`) that is never used. Each Wasm app may define its own health
endpoint, but Pingora never probes it. A Wasm app that returns 500 on `/health` is
unhealthy, but the platform continues routing traffic to it.

**5. No startup probe.** When a node first starts, it takes 5–30 seconds to restore
state from redb, subscribe to NATS, and load routes. During this window, the node is
alive but not ready. Without a startup probe, the orchestrator may kill and restart the
node repeatedly (crash loop) because the readiness check fails before initialization
completes.

### Why Three Probes (Not One)

Kubernetes, AWS ALB, and most orchestrators distinguish between liveness and readiness.
The three-probe model comes from Kubernetes:

| Probe     │ Question                              │ Failure Action              │ Endpoint
|───────────│───────────────────────────────────────│─────────────────────────────│──────────
| Startup   │ "Has the node finished initializing?" │ Keep waiting (don't kill)   │ `/livez`
| Liveness  │ "Is the node process alive?"          │ Restart the node            │ `/healthz`
| Readiness │ "Can the node serve traffic?"          │ Remove from LB pool         │ `/readyz`

The startup probe prevents the liveness probe from killing a node that is simply slow
to start. Once the startup probe succeeds, the liveness probe takes over.

The readiness probe is separate from liveness because a node can be alive but not ready
(e.g., NATS is down, disk is full, backpressure is active). Killing the node won't fix
a NATS outage, but removing it from the load balancer prevents user-visible errors.

### Why "Healthy / Degraded / Unhealthy" (Not Just "OK / Not OK")

Binary health (OK or Not OK) loses important information:

- **Healthy**: All dependencies are up, all instances are running, the node can serve
  traffic at full capacity. Load balancer should route to this node.
- **Degraded**: Some dependencies are down (e.g., NATS disconnected), but the node can
  still serve existing apps from cached state. New deployments won't work, but existing
  traffic is fine. Load balancer should route to this node with reduced weight.
- **Unhealthy**: A critical dependency is down (e.g., redb is corrupted, disk is full),
  or the node is under backpressure. The node cannot reliably serve traffic. Load
  balancer should not route to this node.

This three-state model maps naturally to HTTP status codes:
- Healthy → 200 OK
- Degraded → 200 OK with `"status": "degraded"` (load balancers see 200 and route traffic)
- Unhealthy → 503 Service Unavailable

The degraded state returns 200 because most load balancers only check the HTTP status
code. A 503 would remove the node from the pool entirely, which is wrong for a node
that can still serve existing apps.

### Why Per-App Health Checks

Without per-app health checks, the platform treats all instances of an app as healthy
as long as they are running. But a running instance can be unhealthy:
- The app's database connection pool is exhausted → `/health` returns 500
- The app's memory is fragmented → `/health` returns 200 but response times spike
- The app's dependency is down → `/health` returns 503

Pingora has built-in health check support for upstreams. When an upstream fails its
health check, Pingora stops routing to it. This is the standard mechanism for removing
unhealthy backends from the pool.

The `health_check_path` field in `AppConfig` already exists but is never used. This
step connects it to Pingora's health checking system.

### Why NATS Health Events

In a multi-node cluster, each node knows its own health but not the health of other
nodes. When node-0 becomes unhealthy, node-1 doesn't know — it continues trying to
steer traffic to node-0 via cross-node routing (Step 12).

Publishing health state changes via NATS allows:
- Other nodes to update their `NodeLoadTable` with health information
- `wasm-ctl cluster-health` to show the health of all nodes
- Automated remediation (e.g., a controller that restarts unhealthy nodes)
- DNS providers to remove unhealthy nodes from DNS records

---

## 1. Health State Model

### 1.1 Node Health State

```rust
// crates/common/src/health.rs (new file)
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// The overall health state of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeHealthStatus {
    /// All dependencies healthy, accepting traffic.
    Healthy,
    /// Some dependencies degraded, still serving existing apps.
    Degraded,
    /// Critical dependency failed, cannot reliably serve traffic.
    Unhealthy,
}

/// The health status of an individual dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStatus {
    /// Dependency is responding normally.
    Healthy,
    /// Dependency is responding but with elevated latency or errors.
    Degraded,
    /// Dependency is not responding or returning errors.
    Unhealthy,
    /// Dependency has not been checked yet (startup).
    Unknown,
}

/// Detailed health information about a single dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyHealth {
    /// The name of the dependency (e.g., "nats", "redb", "disk").
    pub name: String,
    /// Current status.
    pub status: DependencyStatus,
    /// Human-readable message (e.g., "connected", "last write failed: disk full").
    pub message: String,
    /// Latency of the last health check in milliseconds (if applicable).
    pub latency_ms: Option<u64>,
    /// Timestamp of the last successful check (ISO-8601).
    pub last_check: String,
}

/// The complete health report for a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealthReport {
    /// The node's overall health status.
    pub status: NodeHealthStatus,

    /// The node's identity.
    pub node_id: String,

    /// ISO-8601 timestamp of this report.
    pub timestamp: String,

    /// How long the node has been running (seconds).
    pub uptime_secs: u64,

    /// Whether the node has completed startup initialization.
    pub startup_complete: bool,

    /// Whether the node is accepting new requests.
    pub accepting_requests: bool,

    /// Number of active Wasm instances across all apps.
    pub active_instances: u32,

    /// Number of deployed apps.
    pub deployed_apps: u32,

    /// Individual dependency health reports.
    pub dependencies: Vec<DependencyHealth>,

    /// Per-app health summaries.
    pub apps: Vec<AppHealthSummary>,
}

/// Health summary for a single app deployed on this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppHealthSummary {
    /// The app identifier (e.g., "api-users:v2").
    pub app_id: String,
    /// Number of running instances.
    pub instances: u32,
    /// Number of healthy instances (passing health checks).
    pub healthy_instances: u32,
    /// Whether at least one instance is accepting traffic.
    pub serving: bool,
}

/// The type of health probe being performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeType {
    /// Startup probe: has the node finished initializing?
    Startup,
    /// Liveness probe: is the node process alive?
    Liveness,
    /// Readiness probe: can the node serve traffic?
    Readiness,
}
```

### 1.2 Probe Semantics

Each probe type checks a different subset of dependencies:

```
Probe Type   │ What It Checks                                          │ Failure → HTTP
─────────────│─────────────────────────────────────────────────────────│────────────────
Startup      │ Process started, redb opened, routes loaded             │ 503 + Retry-After
Liveness     │ Process not deadlocked, redb readable, memory OK        │ 503
Readiness    │ All of liveness + NATS connected + not backpressure     │ 503
```

The startup probe is more lenient — it only checks that the node has completed its
initialization sequence. It does not require NATS to be connected (the node may still
be connecting). Once startup succeeds, the liveness and readiness probes take over.

The liveness probe checks that the node process is not deadlocked or out of memory.
It does not require NATS — a node can be alive without NATS (degraded mode).

The readiness probe is the strictest — it requires all dependencies to be healthy
(or degraded) for the node to accept traffic.

---

## 2. Dependency Health Checkers

### 2.1 NATS Health Checker

Extends the existing `NatsHealth` (Step 08) with a proper health check method.

```rust
// crates/messaging/src/reconnect.rs (extend)

impl NatsHealth {
    /// Perform a health check on the NATS connection.
    pub fn check_health(&self) -> DependencyHealth {
        let connected = self.is_connected();
        let degraded = self.is_degraded();
        let last_msg_age = self.last_message_age_secs();

        let (status, message) = if connected && !degraded {
            (DependencyStatus::Healthy, "connected".to_string())
        } else if connected && degraded {
            (
                DependencyStatus::Degraded,
                format!("connected but degraded (last message {}s ago)", last_msg_age),
            )
        } else {
            (
                DependencyStatus::Unhealthy,
                format!("disconnected (last message {}s ago)", last_msg_age),
            )
        };

        DependencyHealth {
            name: "nats".to_string(),
            status,
            message,
            latency_ms: None, // NATS doesn't expose round-trip time
            last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }
}
```

### 2.2 Redb Health Checker

Checks that redb is readable and writable. A full disk makes redb read-only, which
breaks the platform (cannot save configs, billing records, or route updates).

```rust
// crates/storage/src/health.rs (new file)

use common::health::{DependencyHealth, DependencyStatus};

/// Health checker for the redb database.
pub struct RedbHealthChecker {
    store: Store,
}

impl RedbHealthChecker {
    pub fn new(store: Store) -> Self {
        RedbHealthChecker { store }
    }

    /// Check that redb is readable and writable.
    pub fn check(&self) -> DependencyHealth {
        let start = std::time::Instant::now();

        // 1. Read check: list apps (exercises the ARTIFACTS and CONFIGS tables)
        let read_result = self.store.list_apps();

        // 2. Write check: write and delete a health check probe key
        let write_result = self.store.write_health_probe();

        let latency_ms = start.elapsed().as_millis() as u64;

        let (status, message) = match (read_result, write_result) {
            (Ok(_), Ok(())) => (DependencyStatus::Healthy, "read/write OK".to_string()),
            (Ok(_), Err(e)) => (
                DependencyStatus::Degraded,
                format!("readable but write failed: {}", e),
            ),
            (Err(e), _) => (
                DependencyStatus::Unhealthy,
                format!("read failed: {}", e),
            ),
        };

        DependencyHealth {
            name: "redb".to_string(),
            status,
            message,
            latency_ms: Some(latency_ms),
            last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }
}
```

```rust
// crates/storage/src/lib.rs (add to Store impl)

impl Store {
    /// Write a health probe key to verify that redb is writable.
    /// This writes a small record and immediately deletes it.
    pub fn write_health_probe(&self) -> Result<(), PlatformError> {
        let write_txn = self.db.begin_write().map_err(|e| {
            PlatformError::Storage(format!("health probe write begin: {}", e))
        })?;

        {
            let mut table = write_txn
                .open_table(self.health_probe_table_definition())
                .map_err(|e| PlatformError::Storage(format!("health probe open: {}", e)))?;
            table
                .insert("probe", chrono::Utc::now().to_rfc3339())
                .map_err(|e| PlatformError::Storage(format!("health probe insert: {}", e)))?;
        }

        write_txn.commit().map_err(|e| {
            PlatformError::Storage(format!("health probe commit: {}", e))
        })?;

        // Clean up the probe key
        let delete_txn = self.db.begin_write().map_err(|e| {
            PlatformError::Storage(format!("health probe delete begin: {}", e))
        })?;

        {
            let mut table = delete_txn
                .open_table(self.health_probe_table_definition())
                .map_err(|e| PlatformError::Storage(format!("health probe open: {}", e)))?;
            table
                .remove("probe")
                .map_err(|e| PlatformError::Storage(format!("health probe remove: {}", e)))?;
        }

        delete_txn.commit().map_err(|e| {
            PlatformError::Storage(format!("health probe delete commit: {}", e))
        })?;

        Ok(())
    }

    fn health_probe_table_definition(
        &self,
    ) -> redb::TableDefinition<'static, &'static str, &'static str> {
        redb::TableDefinition::new("__health_probe")
    }
}
```

### 2.3 Disk Space Health Checker

Checks that the filesystem containing the redb file has sufficient free space.

```rust
// crates/storage/src/health.rs (continued)

/// Check available disk space for the database.
pub fn check_disk_space(db_path: &std::path::Path, min_free_bytes: u64) -> DependencyHealth {
    let (status, message) = match fs2::available_space(db_path) {
        Ok(available) => {
            if available < min_free_bytes {
                (
                    DependencyStatus::Unhealthy,
                    format!(
                        "only {} MB free (minimum {} MB)",
                        available / (1024 * 1024),
                        min_free_bytes / (1024 * 1024)
                    ),
                )
            } else if available < min_free_bytes * 2 {
                (
                    DependencyStatus::Degraded,
                    format!(
                        "low disk space: {} MB free (minimum {} MB)",
                        available / (1024 * 1024),
                        min_free_bytes / (1024 * 1024)
                    ),
                )
            } else {
                (
                    DependencyStatus::Healthy,
                    format!("{} MB free", available / (1024 * 1024)),
                )
            }
        }
        Err(e) => (
            DependencyStatus::Unknown,
            format!("cannot check disk space: {}", e),
        ),
    };

    DependencyHealth {
        name: "disk".to_string(),
        status,
        message,
        latency_ms: None,
        last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}
```

### 2.4 Memory Health Checker

Checks that the node process has not exceeded its memory budget.

```rust
// crates/common/src/health.rs (continued)

/// Check the node process memory usage.
pub fn check_memory(max_memory_bytes: u64) -> DependencyHealth {
    let usage = get_process_memory_usage();

    let (status, message) = match usage {
        Some(used) => {
            let used_mb = used / (1024 * 1024);
            let max_mb = max_memory_bytes / (1024 * 1024);
            let percent = (used as f64 / max_memory_bytes as f64) * 100.0;

            if used > max_memory_bytes {
                (
                    DependencyStatus::Unhealthy,
                    format!("memory exceeded: {} MB / {} MB ({:.0}%)", used_mb, max_mb, percent),
                )
            } else if used > max_memory_bytes * 9 / 10 {
                (
                    DependencyStatus::Degraded,
                    format!("memory high: {} MB / {} MB ({:.0}%)", used_mb, max_mb, percent),
                )
            } else {
                (
                    DependencyStatus::Healthy,
                    format!("{} MB / {} MB ({:.0}%)", used_mb, max_mb, percent),
                )
            }
        }
        None => (
            DependencyStatus::Unknown,
            "cannot determine memory usage".to_string(),
        ),
    };

    DependencyHealth {
        name: "memory".to_string(),
        status,
        message,
        latency_ms: None,
        last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

/// Get the current process memory usage in bytes (RSS).
fn get_process_memory_usage() -> Option<u64> {
    // On Linux, read /proc/self/status VmRSS
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let kb: u64 = parts[1].parse().ok()?;
                    return Some(kb * 1024);
                }
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Fallback: use sysinfo or just return None
        None
    }
}
```

---

## 3. Health Check Endpoints

### 3.1 Route Definitions

Replace the current single `/health` endpoint with three probe endpoints plus a
detailed status endpoint.

```rust
// crates/proxy/src/health.rs (rewritten)

use crate::backpressure::BackpressureSignal;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
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
    /// Maximum process memory in bytes (default: 4 GB).
    pub max_memory_bytes: u64,
    /// Number of consecutive failures before marking a dependency unhealthy (default: 3).
    pub failure_threshold: u32,
    /// Number of consecutive successes before marking a dependency healthy (default: 2).
    pub success_threshold: u32,
    /// Interval between background health checks (default: 10 seconds).
    pub check_interval: std::time::Duration,
    /// Timeout for individual dependency checks (default: 5 seconds).
    pub check_timeout: std::time::Duration,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        HealthCheckConfig {
            min_disk_free_bytes: 1024 * 1024 * 1024, // 1 GB
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
```

### 3.2 Startup Probe (`/livez`)

The startup probe checks that the node has completed its initialization sequence.
It succeeds once and then always returns OK (the liveness probe takes over).

```rust
// crates/proxy/src/health.rs (continued)

/// Startup probe: has the node finished initializing?
/// Returns 503 until startup is complete, then 200 forever.
async fn startup_probe(State(state): State<Arc<HealthState>>) -> Response {
    let startup_complete = state.startup_complete.load(std::sync::atomic::Ordering::Relaxed);

    if startup_complete {
        (StatusCode::OK, Json(serde_json::json!({
            "status": "ok",
            "node_id": state.node_id,
        }))).into_response()
    } else {
        let uptime = state.started_at.elapsed().as_secs();
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "status": "starting",
            "node_id": state.node_id,
            "uptime_secs": uptime,
            "message": "node initialization in progress",
        }))).into_response()
    }
}
```

### 3.3 Liveness Probe (`/healthz`)

The liveness probe checks that the node process is not deadlocked and that critical
local dependencies (redb, memory) are functional. It does NOT require NATS — a node
can be alive without NATS (degraded mode).

```rust
// crates/proxy/src/health.rs (continued)

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
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state.startup_complete.load(std::sync::atomic::Ordering::Relaxed),
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
```

### 3.4 Readiness Probe (`/readyz`)

The readiness probe checks ALL dependencies including NATS. A node that is not ready
should be removed from the load balancer pool.

```rust
// crates/proxy/src/health.rs (continued)

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
        last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });

    let status = compute_status_for_probe(&dependencies, ProbeType::Readiness);

    let response = NodeHealthReport {
        status,
        node_id: state.node_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state.startup_complete.load(std::sync::atomic::Ordering::Relaxed),
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
```

### 3.5 Detailed Status (`/status`)

The detailed status endpoint returns the full `NodeHealthReport` with all dependency
details and per-app health. This is for operators and `wasm-ctl`, not for load balancers.

```rust
// crates/proxy/src/health.rs (continued)

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
        last_check: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });

    let status = compute_status_for_probe(&dependencies, ProbeType::Readiness);

    Json(NodeHealthReport {
        status,
        node_id: state.node_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        uptime_secs: state.started_at.elapsed().as_secs(),
        startup_complete: state.startup_complete.load(std::sync::atomic::Ordering::Relaxed),
        accepting_requests: state.backpressure.is_accepting(),
        active_instances: state.instance_count_provider.active_instance_count(),
        deployed_apps: state.instance_count_provider.deployed_app_count(),
        dependencies,
        apps: state.instance_count_provider.app_health_summaries(),
    })
}
```

### 3.6 Per-App Status (`/status/app/{app_id}`)

```rust
// crates/proxy/src/health.rs (continued)

/// Per-app health status.
async fn app_status(
    Path(app_id): Path<String>,
    State(state): State<Arc<HealthState>>,
) -> Response {
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
```

### 3.7 Status Computation

```rust
// crates/proxy/src/health.rs (continued)

/// Compute the overall node health status from dependency health reports.
fn compute_status_for_probe(
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
```

---

## 4. Per-App Health Checks via Pingora

Pingora has built-in health check support for upstream backends. This step connects
the `health_check_path` field in `AppConfig` to Pingora's health checking system.

### 4.1 Health Check Configuration per App

```rust
// crates/proxy/src/upstream.rs (extend)

use pingora_core::upstreams::health_check::{HealthCheck, HttpHealthCheck};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-app health check configuration.
#[derive(Debug, Clone)]
pub struct AppHealthCheckConfig {
    /// The path to probe (e.g., "/health").
    pub path: String,
    /// Expected HTTP status code (default: 200).
    pub expected_status: u16,
    /// Interval between health checks (default: 10 seconds).
    pub interval: std::time::Duration,
    /// Timeout for each health check request (default: 5 seconds).
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
        config.health_check_path.as_ref().map(|path| AppHealthCheckConfig {
            path: path.clone(),
            ..Default::default()
        })
    }
}
```

### 4.2 Upstream Health Check Integration

```rust
// crates/proxy/src/upstream.rs (continued)

/// Extended UpstreamRegistry with health check tracking.
pub struct UpstreamRegistry {
    inner: RwLock<HashMap<String, (AppId, Vec<std::net::SocketAddr>)>>,
    /// Per-app health check configuration.
    health_configs: RwLock<HashMap<String, AppHealthCheckConfig>>,
    /// Per-app health state: (consecutive_successes, consecutive_failures, healthy).
    health_state: RwLock<HashMap<String, AppHealthState>>,
    /// Callback to update the app health registry.
    app_health_registry: Arc<RwLock<crate::health::AppHealthRegistry>>,
}

#[derive(Debug, Clone)]
struct AppHealthState {
    consecutive_successes: u32,
    consecutive_failures: u32,
    healthy: bool,
}

impl UpstreamRegistry {
    /// Register health check configuration for an app.
    pub async fn register_health_check(
        &self,
        app_id: &str,
        config: AppHealthCheckConfig,
    ) {
        self.health_configs
            .write()
            .await
            .insert(app_id.to_string(), config);
        self.health_state
            .write()
            .await
            .insert(app_id.to_string(), AppHealthState {
                consecutive_successes: 0,
                consecutive_failures: 0,
                healthy: true, // Assume healthy until proven otherwise
            });
    }

    /// Remove health check configuration for an app.
    pub async fn remove_health_check(&self, app_id: &str) {
        self.health_configs.write().await.remove(app_id);
        self.health_state.write().await.remove(app_id);
    }

    /// Record a health check result for an app instance.
    pub async fn record_health_result(&self, app_id: &str, success: bool) {
        let mut states = self.health_state.write().await;
        let mut configs = self.health_configs.read().await;

        if let (Some(state), Some(config)) =
            (states.get_mut(app_id), configs.get(app_id))
        {
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
    pub async fn next_healthy(&self, app_id: &AppId) -> Option<std::net::SocketAddr> {
        let inner = self.inner.read().await;
        let addrs = inner.get(&app_id.0)?;

        // If no health check is configured, use round-robin as before
        if !self.health_configs.read().await.contains_key(&app_id.0) {
            // Fall through to existing next() logic
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
```

### 4.3 Background Health Check Loop

A background task that periodically probes each app's health check endpoint.

```rust
// crates/proxy/src/upstream_health.rs (new file)

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
        let configs = self.registry.health_configs.read().await.clone;
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
                self.registry
                    .app_health_registry
                    .write()
                    .await
                    .update(app_id, healthy_count, total);

                // Record the overall result
                let app_healthy = healthy_count > 0;
                self.registry
                    .record_health_result(app_id, app_healthy)
                    .await;
            }
        }
    }

    /// Probe a single health check endpoint.
    async fn probe(
        &self,
        url: &str,
        config: &AppHealthCheckConfig,
    ) -> Result<(), String> {
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
```

---

## 5. NATS Health Events

Publish health state changes via NATS so other nodes and operators can react.

### 5.1 Health Event Definition

```rust
// crates/messaging/src/events.rs (add to Event enum)

/// Published when a node's health status changes.
NodeHealthChanged {
    node_id: String,
    /// The new health status: "healthy", "degraded", or "unhealthy".
    status: String,
    /// Which dependency caused the change (if applicable).
    cause: Option<String>,
    /// ISO-8601 timestamp.
    timestamp: String,
    /// Number of active instances.
    active_instances: u32,
    /// Whether the node is accepting requests.
    accepting_requests: bool,
},

/// Published periodically with the node's current health snapshot.
NodeHealthSnapshot {
    node_id: String,
    status: String,
    active_instances: u32,
    deployed_apps: u32,
    nats_connected: bool,
    disk_free_mb: u64,
    memory_used_mb: u64,
    timestamp: String,
},
```

```rust
// crates/messaging/src/events.rs (add to subject())

Event::NodeHealthChanged { node_id, .. } => {
    format!("cluster.health.changed.{}", node_id)
}
Event::NodeHealthSnapshot { node_id, .. } => {
    format!("cluster.health.snapshot.{}", node_id)
}
```

### 5.2 Health Event Publisher

```rust
// crates/proxy/src/health_events.rs (new file)

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
```

### 5.3 Background Health Check Loop

A background task that periodically evaluates all dependencies and publishes changes.

```rust
// crates/proxy/src/health_events.rs (continued)

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
                        .and_then(|d| d.message.strip_suffix(" MB free"))
                        .and_then(|s| s.parse::<u64>().ok())
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
```

---

## 6. Instance Count Provider (Supervisor Integration)

The Supervisor implements `InstanceCountProvider` to provide real instance counts
to the health check system, replacing the hardcoded `active_instances: 0`.

```rust
// crates/supervisor/src/lib.rs (add)

use proxy::health::InstanceCountProvider;
use common::health::AppHealthSummary;

impl InstanceCountProvider for Supervisor {
    fn active_instance_count(&self) -> u32 {
        // Use try_read to avoid blocking the health check on the RwLock.
        // If the lock is contended, return 0 (conservative).
        match self.pools.try_read() {
            Ok(pools) => pools.values().map(|p| p.instance_count() as u32).sum(),
            Err(_) => 0,
        }
    }

    fn deployed_app_count(&self) -> u32 {
        match self.pools.try_read() {
            Ok(pools) => pools.len() as u32,
            Err(_) => 0,
        }
    }

    fn app_health_summaries(&self) -> Vec<AppHealthSummary> {
        match self.pools.try_read() {
            Ok(pools) => pools
                .iter()
                .map(|(app_id, pool)| {
                    let instances = pool.instance_count() as u32;
                    AppHealthSummary {
                        app_id: app_id.clone(),
                        instances,
                        healthy_instances: instances, // Updated by upstream health checks
                        serving: instances > 0,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}
```

```rust
// crates/supervisor/src/pool.rs (add to InstancePool)

impl InstancePool {
    /// Get the number of running instances in this pool.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }
}
```

---

## 7. Startup Completion Signal

The node must signal when startup is complete so the startup probe can succeed.

```rust
// crates/node/src/main.rs (modified)

async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // ... existing initialization ...

    let startup_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Build the health state with the startup_complete flag
    let health_state = proxy::health::HealthState {
        node_id: args.node_id.clone(),
        nats_health: nats_health.clone(),
        backpressure: Arc::new(backpressure.clone()),
        started_at: std::time::Instant::now(),
        startup_complete: startup_complete.clone(),
        instance_count_provider: supervisor.clone() as Arc<dyn InstanceCountProvider + Send + Sync>,
        dependency_checkers: Arc::new(vec![
            Box::new(NatsDependencyChecker::new(nats_health.clone())),
            Box::new(RedbDependencyChecker::new(store.clone())),
            Box::new(DiskDependencyChecker::new(
                std::path::PathBuf::from(&args.db_path),
                1024 * 1024 * 1024, // 1 GB minimum
            )),
            Box::new(MemoryDependencyChecker::new(
                4 * 1024 * 1024 * 1024, // 4 GB maximum
            )),
        ]),
        app_health_registry: Arc::new(RwLock::new(AppHealthRegistry::new())),
        config: proxy::health::HealthCheckConfig::default(),
    };

    // ... existing startup code ...

    // After all initialization is complete, signal startup completion
    startup_complete.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(node_id = %args.node_id, "node startup complete — all probes active");

    // ... rest of main ...
}
```

---

## 8. Dependency Checker Implementations

Concrete implementations of the `DependencyChecker` trait for each dependency.

```rust
// crates/proxy/src/health.rs (continued)

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
}

impl DiskDependencyChecker {
    pub fn new(db_path: std::path::PathBuf, min_free_bytes: u64) -> Self {
        DiskDependencyChecker { db_path, min_free_bytes }
    }
}

impl DependencyChecker for DiskDependencyChecker {
    fn name(&self) -> &str {
        "disk"
    }

    fn check(&self) -> DependencyHealth {
        storage::health::check_disk_space(&self.db_path, self.min_free_bytes)
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
```

---

## 9. Prometheus Health Metrics

```rust
// crates/metrics/src/health_metrics.rs (new file)

use prometheus::{IntGauge, Registry};
use std::sync::Arc;

/// Prometheus metrics for the health check subsystem.
pub struct HealthMetrics {
    /// Current node health status: 0=unhealthy, 1=degraded, 2=healthy.
    pub node_health_status: IntGauge,

    /// Number of active Wasm instances.
    pub active_instances: IntGauge,

    /// Number of deployed apps.
    pub deployed_apps: IntGauge,

    /// Whether NATS is connected: 0=disconnected, 1=connected.
    pub nats_connected: IntGauge,

    /// Whether the node is accepting requests: 0=rejecting, 1=accepting.
    pub accepting_requests: IntGauge,

    /// Available disk space in MB.
    pub disk_free_mb: IntGauge,

    /// Process memory usage in MB.
    pub memory_used_mb: IntGauge,

    /// Per-app healthy instance count.
    pub app_healthy_instances: IntGauge,

    /// Per-app total instance count.
    pub app_total_instances: IntGauge,
}

impl HealthMetrics {
    pub fn new(registry: &Registry) -> Self {
        let node_health_status = IntGauge::new(
            "wasm_node_health_status",
            "Node health status: 0=unhealthy, 1=degraded, 2=healthy",
        ).unwrap();
        registry.register(Box::new(node_health_status.clone())).unwrap();

        let active_instances = IntGauge::new(
            "wasm_node_active_instances",
            "Number of active Wasm instances",
        ).unwrap();
        registry.register(Box::new(active_instances.clone())).unwrap();

        let deployed_apps = IntGauge::new(
            "wasm_node_deployed_apps",
            "Number of deployed applications",
        ).unwrap();
        registry.register(Box::new(deployed_apps.clone())).unwrap();

        let nats_connected = IntGauge::new(
            "wasm_node_nats_connected",
            "NATS connection status: 0=disconnected, 1=connected",
        ).unwrap();
        registry.register(Box::new(nats_connected.clone())).unwrap();

        let accepting_requests = IntGauge::new(
            "wasm_node_accepting_requests",
            "Whether the node is accepting requests: 0=rejecting, 1=accepting",
        ).unwrap();
        registry.register(Box::new(accepting_requests.clone())).unwrap();

        let disk_free_mb = IntGauge::new(
            "wasm_node_disk_free_mb",
            "Available disk space in MB",
        ).unwrap();
        registry.register(Box::new(disk_free_mb.clone())).unwrap();

        let memory_used_mb = IntGauge::new(
            "wasm_node_memory_used_mb",
            "Process memory usage in MB",
        ).unwrap();
        registry.register(Box::new(memory_used_mb.clone())).unwrap();

        let app_healthy_instances = IntGauge::new(
            "wasm_node_app_healthy_instances",
            "Number of healthy instances per app",
        ).unwrap();
        registry.register(Box::new(app_healthy_instances.clone())).unwrap();

        let app_total_instances = IntGauge::new(
            "wasm_node_app_total_instances",
            "Total number of instances per app",
        ).unwrap();
        registry.register(Box::new(app_total_instances.clone())).unwrap();

        HealthMetrics {
            node_health_status,
            active_instances,
            deployed_apps,
            nats_connected,
            accepting_requests,
            disk_free_mb,
            memory_used_mb,
            app_healthy_instances,
            app_total_instances,
        }
    }

    /// Update all metrics from a health report.
    pub fn update_from_report(&self, report: &common::health::NodeHealthReport) {
        self.node_health_status.set(match report.status {
            common::health::NodeHealthStatus::Healthy => 2,
            common::health::NodeHealthStatus::Degraded => 1,
            common::health::NodeHealthStatus::Unhealthy => 0,
        });
        self.active_instances.set(report.active_instances as i64);
        self.deployed_apps.set(report.deployed_apps as i64);
        self.accepting_requests.set(if report.accepting_requests { 1 } else { 0 });

        for dep in &report.dependencies {
            match dep.name.as_str() {
                "nats" => self.nats_connected.set(
                    if dep.status == common::health::DependencyStatus::Healthy { 1 } else { 0 }
                ),
                "disk" => {
                    // Parse "XXX MB free" from the message
                    if let Some(mb) = dep.message.strip_suffix(" MB free") {
                        if let Ok(val) = mb.parse::<i64>() {
                            self.disk_free_mb.set(val);
                        }
                    }
                }
                "memory" => {
                    // Parse "XXX MB / YYY MB" from the message
                    if let Some(part) = dep.message.split('/').next() {
                        if let Ok(val) = part.trim().strip_suffix(" MB").and_then(|s| s.parse::<i64>()) {
                            self.memory_used_mb.set(val);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
```

### 9.1 Alerting Rules

```yaml
# Health check alerting rules
groups:
  - name: wasm_platform_health
    rules:
      - alert: NodeUnhealthy
        expr: wasm_node_health_status == 0
        for: 1m
        annotations:
          summary: "Node {{ $labels.instance }} is unhealthy"
          description: "The node has been unhealthy for more than 1 minute. Check dependencies."

      - alert: NodeDegraded
        expr: wasm_node_health_status == 1
        for: 5m
        annotations:
          summary: "Node {{ $labels.instance }} is degraded"
          description: "The node has been in degraded state for more than 5 minutes. Some dependencies are not fully healthy."

      - alert: NATSDisconnected
        expr: wasm_node_nats_connected == 0
        for: 30s
        annotations:
          summary: "NATS disconnected on {{ $labels.instance }}"
          description: "The node has lost its NATS connection. It is operating in degraded mode."

      - alert: LowDiskSpace
        expr: wasm_node_disk_free_mb < 500
        for: 5m
        annotations:
          summary: "Low disk space on {{ $labels.instance }}"
          description: "Only {{ $value }} MB of disk space remaining. redb may become read-only."

      - alert: HighMemoryUsage
        expr: wasm_node_memory_used_mb > 3600
        for: 5m
        annotations:
          summary: "High memory usage on {{ $labels.instance }}"
          description: "Process is using {{ $value }} MB of memory (90% of 4 GB limit)."

      - alert: AllInstancesDown
        expr: wasm_node_active_instances == 0
        for: 2m
        annotations:
          summary: "No active instances on {{ $labels.instance }}"
          description: "The node has zero active Wasm instances. It may need investigation."
```

---

## 10. `wasm-ctl` Health Commands

### 10.1 Enhanced `wasm-ctl node health`

```rust
// crates/ctl/src/cmds/node.rs (rewritten)

use anyhow::Result;
use common::health::NodeHealthReport;

/// Check node health with detailed output.
pub async fn health(node_api: &str, http: &reqwest::Client) -> Result<()> {
    let url = format!("{}/status", node_api);
    let resp = http.get(&url).send().await?;

    if resp.status().is_success() {
        let report: NodeHealthReport = resp.json().await?;

        println!("{}", "Node Health Report".bold());
        println!("{}", "==================");
        println!();

        // Overall status
        let status_str = match report.status {
            common::health::NodeHealthStatus::Healthy => "HEALTHY".green().to_string(),
            common::health::NodeHealthStatus::Degraded => "DEGRADED".yellow().to_string(),
            common::health::NodeHealthStatus::Unhealthy => "UNHEALTHY".red().to_string(),
        };
        println!("  Status:            {}", status_str);
        println!("  Node ID:           {}", report.node_id);
        println!("  Uptime:            {}s", report.uptime_secs);
        println!("  Active instances:  {}", report.active_instances);
        println!("  Deployed apps:     {}", report.deployed_apps);
        println!("  Accepting traffic: {}", if report.accepting_requests { "yes".green() } else { "no".red() });
        println!();

        // Dependencies
        println!("{}", "Dependencies".bold());
        for dep in &report.dependencies {
            let status_icon = match dep.status {
                common::health::DependencyStatus::Healthy => "✓".green(),
                common::health::DependencyStatus::Degraded => "⚠".yellow(),
                common::health::DependencyStatus::Unhealthy => "✗".red(),
                common::health::DependencyStatus::Unknown => "?".dimmed(),
            };
            let latency = dep.latency_ms
                .map(|ms| format!(" ({}ms)", ms))
                .unwrap_or_default();
            println!("  {} {:12} {}{}", status_icon, dep.name, dep.message, latency);
        }
        println!();

        // Per-app health
        if !report.apps.is_empty() {
            println!("{}", "Applications".bold());
            for app in &report.apps {
                let serving = if app.serving {
                    "serving".green().to_string()
                } else {
                    "not serving".red().to_string()
                };
                println!(
                    "  {:30} {}/{} instances  {}",
                    app.app_id,
                    app.healthy_instances.to_string().green(),
                    app.instances,
                    serving,
                );
            }
        }
    } else {
        println!("Node health: UNHEALTHY (status {})", resp.status());
    }

    Ok(())
}

/// Check the startup probe (for orchestrators).
pub async fn startup_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/livez", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}

/// Check the liveness probe.
pub async fn liveness_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/healthz", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}

/// Check the readiness probe.
pub async fn readiness_probe(node_api: &str, http: &reqwest::Client) -> Result<bool> {
    let url = format!("{}/readyz", node_api);
    let resp = http.get(&url).send().await?;
    Ok(resp.status().is_success())
}
```

### 10.2 Enhanced `wasm-ctl cluster-health`

```rust
// crates/ctl/src/cmds/node.rs (continued)

/// Show cluster-wide health by reading NodeHealthSnapshot events from NATS.
pub async fn cluster_health(bus: &NatsBus) -> Result<()> {
    // Subscribe to health snapshot events
    let js = async_nats::jetstream::new(bus.client().clone());

    // Ensure the HEALTH stream exists
    let stream = match js.get_stream("HEALTH").await {
        Ok(s) => s,
        Err(_) => {
            println!("HEALTH stream not found — cluster may not be initialized");
            return Ok(());
        }
    };

    // Read the latest snapshot from each node
    println!("{}", "Cluster Health Status".bold());
    println!("{}", "=====================");

    // Use a pull consumer to read recent messages
    let consumer = stream.get_consumer("ctl-cluster-health").await;

    // ... (similar to existing cluster_health implementation but with health data)

    Ok(())
}
```

---

## 11. node.toml: Health Check Configuration

```toml
# ── Health Check Configuration ─────────────────────────────────────

[health_check]
# Interval between background health checks (seconds)
check_interval_secs = 10

# Timeout for individual dependency checks (seconds)
check_timeout_secs = 5

# Consecutive failures before marking a dependency unhealthy
failure_threshold = 3

# Consecutive successes before marking a dependency healthy
success_threshold = 2

# Minimum free disk space (bytes). Below this, disk is "unhealthy".
min_disk_free_bytes = 1073741824  # 1 GB

# Maximum process memory (bytes). Above this, memory is "unhealthy".
max_memory_bytes = 4294967296  # 4 GB

# Interval for publishing health snapshots via NATS (seconds)
snapshot_interval_secs = 60

# Default per-app health check configuration
[health_check.app_defaults]
path = "/health"
expected_status = 200
interval_secs = 10
timeout_secs = 5
failure_threshold = 3
success_threshold = 2
```

---

## 12. JetStream Stream for Health Events

The HEALTH stream must be created during JetStream setup to persist health events.

```rust
// crates/messaging/src/lib.rs (extend setup_jetstream)

impl NatsBus {
    pub async fn setup_jetstream(&self) -> Result<(), PlatformError> {
        let js = async_nats::jetstream::new(self.client.clone());

        // ... existing stream creation (DEPLOY, etc.) ...

        // Create HEALTH stream for health events
        let health_stream = async_nats::jetstream::stream::Config {
            name: "HEALTH".to_string(),
            subjects: vec![
                "cluster.health.changed.>".to_string(),
                "cluster.health.snapshot.>".to_string(),
            ],
            retention: async_nats::jetstream::stream::RetentionPolicy::Limits,
            max_age: std::time::Duration::from_secs(3600), // 1 hour retention
            max_messages_per_subject: 100,
            ..Default::default()
        };

        js.create_stream(health_stream)
            .await
            .map_err(|e| PlatformError::Messaging(format!("HEALTH stream: {e}")))?;

        tracing::info!("HEALTH JetStream stream created");
        Ok(())
    }
}
```

---

## 13. Integration with Cross-Node Routing

Other nodes use health information to make routing decisions in the `NodeLoadTable`.

```rust
// crates/proxy/src/node_table.rs (extend)

/// A node in the cluster with health information.
pub struct NodeInfo {
    pub node_id: String,
    pub supervisor_addr: SocketAddr,
    pub active_instances: u32,
    pub health_status: NodeHealthStatus,
    pub last_health_update: std::time::Instant,
}

impl NodeLoadTable {
    /// Update a node's health status from a NATS health event.
    pub async fn update_health(
        &self,
        node_id: &str,
        status: NodeHealthStatus,
    ) {
        let mut table = self.inner.write().await;
        if let Some(node) = table.get_mut(node_id) {
            node.health_status = status;
            node.last_health_update = std::time::Instant::now();
        }
    }

    /// Find the least loaded healthy node.
    /// Skips nodes that are unhealthy.
    pub async fn least_loaded_healthy_node(&self) -> Option<NodeInfo> {
        let table = self.inner.read().await;
        table
            .values()
            .filter(|n| n.health_status != NodeHealthStatus::Unhealthy)
            .min_by_key(|n| n.active_instances)
            .cloned()
    }
}
```

---

## 14. Backward Compatibility

The existing `/health` endpoint is preserved as an alias for `/readyz`. This ensures
that any existing load balancer configurations, DNS health checks, or monitoring
systems that use `/health` continue to work without changes.

```rust
// crates/proxy/src/health.rs (route definition)

Router::new()
    // New Kubernetes-style probe endpoints
    .route("/healthz", get(liveness_probe))
    .route("/readyz", get(readiness_probe))
    .route("/livez", get(startup_probe))
    // Backward-compatible: /health maps to readiness
    .route("/health", get(readiness_probe))
    // Detailed status (for operators)
    .route("/status", get(detailed_status))
    .route("/status/app/{app_id}", get(app_status))
    .with_state(Arc::new(state))
```

The response format is also backward-compatible. The existing `HealthResponse` fields
(`status`, `node_id`, `nats_connected`, `active_instances`, `accepting_requests`) are
all present in `NodeHealthReport`. The new fields (`uptime_secs`, `startup_complete`,
`deployed_apps`, `dependencies`, `apps`) are additive.

---

## 15. Example Health Check Responses

### 15.1 Healthy Node (`/readyz` → 200)

```json
{
  "status": "healthy",
  "node_id": "node-0",
  "timestamp": "2026-04-05T12:00:00Z",
  "uptime_secs": 86400,
  "startup_complete": true,
  "accepting_requests": true,
  "active_instances": 12,
  "deployed_apps": 5,
  "dependencies": [
    { "name": "nats", "status": "healthy", "message": "connected", "latency_ms": null, "last_check": "2026-04-05T12:00:00Z" },
    { "name": "redb", "status": "healthy", "message": "read/write OK", "latency_ms": 2, "last_check": "2026-04-05T12:00:00Z" },
    { "name": "disk", "status": "healthy", "message": "51234 MB free", "latency_ms": null, "last_check": "2026-04-05T12:00:00Z" },
    { "name": "memory", "status": "healthy", "message": "1024 MB / 4096 MB (25%)", "latency_ms": null, "last_check": "2026-04-05T12:00:00Z" },
    { "name": "backpressure", "status": "healthy", "message": "accepting requests", "latency_ms": null, "last_check": "2026-04-05T12:00:00Z" }
  ],
  "apps": [
    { "app_id": "api-users:v2", "instances": 3, "healthy_instances": 3, "serving": true },
    { "app_id": "payments:v1", "instances": 2, "healthy_instances": 2, "serving": true },
    { "app_id": "web-frontend:v3", "instances": 4, "healthy_instances": 4, "serving": true },
    { "app_id": "background-worker:v1", "instances": 2, "healthy_instances": 2, "serving": true },
    { "app_id": "metrics-scraper:v1", "instances": 1, "healthy_instances": 1, "serving": true }
  ]
}
```

### 15.2 Degraded Node (`/readyz` → 200)

```json
{
  "status": "degraded",
  "node_id": "node-1",
  "timestamp": "2026-04-05T12:05:00Z",
  "uptime_secs": 86400,
  "startup_complete": true,
  "accepting_requests": true,
  "active_instances": 8,
  "deployed_apps": 5,
  "dependencies": [
    { "name": "nats", "status": "unhealthy", "message": "disconnected (last message 45s ago)", "latency_ms": null, "last_check": "2026-04-05T12:05:00Z" },
    { "name": "redb", "status": "healthy", "message": "read/write OK", "latency_ms": 3, "last_check": "2026-04-05T12:05:00Z" },
    { "name": "disk", "status": "degraded", "message": "low disk space: 800 MB free (minimum 1024 MB)", "latency_ms": null, "last_check": "2026-04-05T12:05:00Z" },
    { "name": "memory", "status": "healthy", "message": "2048 MB / 4096 MB (50%)", "latency_ms": null, "last_check": "2026-04-05T12:05:00Z" },
    { "name": "backpressure", "status": "healthy", "message": "accepting requests", "latency_ms": null, "last_check": "2026-04-05T12:05:00Z" }
  ],
  "apps": [
    { "app_id": "api-users:v2", "instances": 3, "healthy_instances": 3, "serving": true },
    { "app_id": "payments:v1", "instances": 2, "healthy_instances": 1, "serving": true },
    { "app_id": "web-frontend:v3", "instances": 2, "healthy_instances": 2, "serving": true },
    { "app_id": "background-worker:v1", "instances": 1, "healthy_instances": 1, "serving": true },
    { "app_id": "metrics-scraper:v1", "instances": 0, "healthy_instances": 0, "serving": false }
  ]
}
```

### 15.3 Unhealthy Node (`/readyz` → 503)

```json
{
  "status": "unhealthy",
  "node_id": "node-2",
  "timestamp": "2026-04-05T12:10:00Z",
  "uptime_secs": 86400,
  "startup_complete": true,
  "accepting_requests": false,
  "active_instances": 0,
  "deployed_apps": 5,
  "dependencies": [
    { "name": "nats", "status": "unhealthy", "message": "disconnected (last message 300s ago)", "latency_ms": null, "last_check": "2026-04-05T12:10:00Z" },
    { "name": "redb", "status": "unhealthy", "message": "readable but write failed: disk full", "latency_ms": 15, "last_check": "2026-04-05T12:10:00Z" },
    { "name": "disk", "status": "unhealthy", "message": "only 12 MB free (minimum 1024 MB)", "latency_ms": null, "last_check": "2026-04-05T12:10:00Z" },
    { "name": "memory", "status": "healthy", "message": "3072 MB / 4096 MB (75%)", "latency_ms": null, "last_check": "2026-04-05T12:10:00Z" },
    { "name": "backpressure", "status": "unhealthy", "message": "rejecting requests — node at capacity", "latency_ms": null, "last_check": "2026-04-05T12:10:00Z" }
  ],
  "apps": []
}
```

### 15.4 Starting Node (`/livez` → 503)

```json
{
  "status": "starting",
  "node_id": "node-3",
  "uptime_secs": 5,
  "message": "node initialization in progress"
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Probe Endpoints
- [ ] `/livez` (startup probe) returns 503 until initialization is complete, then 200
- [ ] `/healthz` (liveness probe) checks redb, memory, disk — but NOT NATS
- [ ] `/readyz` (readiness probe) checks all dependencies including NATS and backpressure
- [ ] `/health` preserved as backward-compatible alias for `/readyz`
- [ ] `/status` returns full `NodeHealthReport` with all dependency details
- [ ] `/status/app/{app_id}` returns per-app health summary

### Health State Model
- [ ] `NodeHealthStatus` enum: Healthy, Degraded, Unhealthy
- [ ] `DependencyStatus` enum: Healthy, Degraded, Unhealthy, Unknown
- [ ] `NodeHealthReport` includes all required fields (status, node_id, uptime, dependencies, apps)
- [ ] `AppHealthSummary` includes app_id, instances, healthy_instances, serving

### Dependency Checkers
- [ ] NATS checker uses existing `NatsHealth.check_health()` method
- [ ] Redb checker verifies read/write with a health probe key
- [ ] Disk checker uses `fs2::available_space()` with configurable minimum
- [ ] Memory checker reads `/proc/self/status` VmRSS on Linux
- [ ] All checkers implement the `DependencyChecker` trait
- [ ] Checkers are registered at startup and run on the configured interval

### Instance Count Provider
- [ ] `Supervisor` implements `InstanceCountProvider` trait
- [ ] `active_instance_count()` returns real count from instance pools
- [ ] `deployed_app_count()` returns real count from pool map
- [ ] `app_health_summaries()` returns per-app instance counts
- [ ] `try_read()` used on the RwLock to avoid blocking health checks

### Per-App Health Checks
- [ ] `AppHealthCheckConfig` created from `AppConfig.health_check_path`
- [ ] `UpstreamHealthChecker` probes each app's health endpoint on a background loop
- [ ] Health check results update the `AppHealthRegistry`
- [ ] Unhealthy apps are skipped in `UpstreamRegistry.next_healthy()`
- [ ] Health check configuration is registered when an app is deployed
- [ ] Health check configuration is removed when an app is removed

### NATS Health Events
- [ ] `NodeHealthChanged` event published when status transitions
- [ ] `NodeHealthSnapshot` event published every 60 seconds
- [ ] HEALTH JetStream stream created during `setup_jetstream()`
- [ ] Other nodes update `NodeLoadTable` from health events
- [ ] `least_loaded_healthy_node()` skips unhealthy nodes

### Startup Completion
- [ ] `startup_complete` AtomicBool set to true after all initialization
- [ ] Startup probe returns 503 until the flag is set
- [ ] Startup probe returns 200 forever after the flag is set
- [ ] Liveness and readiness probes are active only after startup completes

### Prometheus Metrics
- [ ] `wasm_node_health_status` gauge: 0=unhealthy, 1=degraded, 2=healthy
- [ ] `wasm_node_active_instances` gauge with real instance count
- [ ] `wasm_node_deployed_apps` gauge
- [ ] `wasm_node_nats_connected` gauge
- [ ] `wasm_node_accepting_requests` gauge
- [ ] `wasm_node_disk_free_mb` gauge
- [ ] `wasm_node_memory_used_mb` gauge
- [ ] Alerting rules for NodeUnhealthy, NodeDegraded, NATSDisconnected, LowDiskSpace

### CLI Integration
- [ ] `wasm-ctl node health` shows detailed health report with dependency status
- [ ] `wasm-ctl node health` shows per-app instance counts and serving status
- [ ] `wasm-ctl cluster-health` reads HEALTH stream for cluster-wide status
- [ ] Color-coded output: green=healthy, yellow=degraded, red=unhealthy

### Configuration
- [ ] `node.toml` `[health_check]` section configures intervals, thresholds, limits
- [ ] `node.toml` `[health_check.app_defaults]` configures default per-app health checks
- [ ] CLI flags for health check parameters override TOML config

### Backward Compatibility
- [ ] `/health` endpoint returns the same fields as before (status, node_id, nats_connected)
- [ ] `active_instances` field now returns real count instead of hardcoded 0
- [ ] New fields are additive — existing parsers are not broken
- [ ] HTTP status codes match: 200 for healthy/degraded, 503 for unhealthy

### Tests
- [ ] Unit test: `compute_status_for_probe` correctly classifies healthy/degraded/unhealthy
- [ ] Unit test: liveness probe ignores NATS status
- [ ] Unit test: readiness probe includes NATS status
- [ ] Unit test: startup probe returns 503 until `startup_complete` is set
- [ ] Unit test: `RedbHealthChecker` detects write failures
- [ ] Unit test: `AppHealthState` transitions after correct number of successes/failures
- [ ] Integration test: `/readyz` returns 503 when NATS is disconnected
- [ ] Integration test: `/readyz` returns 200 when NATS reconnects
- [ ] E2E test: health status changes are published via NATS
- [ ] E2E test: unhealthy app is removed from upstream routing
