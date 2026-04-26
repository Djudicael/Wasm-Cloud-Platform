// crates/common/src/health.rs
use serde::{Deserialize, Serialize};

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

/// Check the node process memory usage.
///
/// **Note:** This function performs blocking I/O (reads `/proc/self/status` on Linux)
/// and should be called via [`tokio::task::spawn_blocking`] in async contexts to avoid
/// blocking the tokio runtime.
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
                    format!(
                        "memory exceeded: {} MB / {} MB ({:.0}%)",
                        used_mb, max_mb, percent
                    ),
                )
            } else if used > max_memory_bytes * 9 / 10 {
                (
                    DependencyStatus::Degraded,
                    format!(
                        "memory high: {} MB / {} MB ({:.0}%)",
                        used_mb, max_mb, percent
                    ),
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
        // Fallback: not available on non-Linux platforms
        None
    }
}
