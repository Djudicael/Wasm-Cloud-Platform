// crates/storage/src/health.rs
use crate::Store;
use common::health::{DependencyHealth, DependencyStatus};

/// Health checker for the redb database.
#[derive(Clone)]
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
            (Err(e), _) => (DependencyStatus::Unhealthy, format!("read failed: {}", e)),
        };

        DependencyHealth {
            name: "redb".to_string(),
            status,
            message,
            latency_ms: Some(latency_ms),
            last_check: chrono::Utc::now().to_rfc3339(),
        }
    }
}

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
        last_check: chrono::Utc::now().to_rfc3339(),
    }
}
