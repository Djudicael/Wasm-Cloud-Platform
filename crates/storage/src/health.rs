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
pub fn check_disk_space(
    db_path: &std::path::Path,
    min_free_bytes: u64,
    min_free_inodes: u64,
) -> DependencyHealth {
    let (status, message) = match filesystem_capacity(db_path) {
        Ok((available_bytes, available_inodes)) => classify_capacity(
            available_bytes,
            available_inodes,
            min_free_bytes,
            min_free_inodes,
        ),
        Err(error) => (
            DependencyStatus::Unknown,
            format!("cannot check filesystem capacity: {error}"),
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

fn classify_capacity(
    available_bytes: u64,
    available_inodes: Option<u64>,
    min_free_bytes: u64,
    min_free_inodes: u64,
) -> (DependencyStatus, String) {
    let byte_unhealthy = available_bytes < min_free_bytes;
    let byte_degraded = available_bytes < min_free_bytes.saturating_mul(2);
    let inode_unhealthy = available_inodes.is_some_and(|value| value < min_free_inodes);
    let inode_degraded =
        available_inodes.is_some_and(|value| value < min_free_inodes.saturating_mul(2));
    let status = if byte_unhealthy || inode_unhealthy {
        DependencyStatus::Unhealthy
    } else if byte_degraded || inode_degraded {
        DependencyStatus::Degraded
    } else {
        DependencyStatus::Healthy
    };
    let prefix = match status {
        DependencyStatus::Unhealthy => "filesystem capacity exhausted: ",
        DependencyStatus::Degraded => "low filesystem capacity: ",
        _ => "",
    };
    let inode_message = available_inodes.map_or_else(
        || "inode availability unavailable".to_string(),
        |value| format!("{value} inodes free"),
    );
    (
        status,
        format!(
            "{prefix}{} MB free, {inode_message} (minimum {} MB / {min_free_inodes} inodes)",
            available_bytes / (1024 * 1024),
            min_free_bytes / (1024 * 1024),
        ),
    )
}

#[cfg(unix)]
fn filesystem_capacity(path: &std::path::Path) -> std::io::Result<(u64, Option<u64>)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        stat.f_bavail as u64 * stat.f_frsize as u64,
        Some(stat.f_favail as u64),
    ))
}

#[cfg(not(unix))]
fn filesystem_capacity(path: &std::path::Path) -> std::io::Result<(u64, Option<u64>)> {
    fs2::available_space(path).map(|bytes| (bytes, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn capacity_is_unhealthy_when_bytes_or_inodes_cross_the_hard_limit() {
        assert_eq!(
            classify_capacity(GIB - 1, Some(50_000), GIB, 10_000).0,
            DependencyStatus::Unhealthy
        );
        assert_eq!(
            classify_capacity(3 * GIB, Some(9_999), GIB, 10_000).0,
            DependencyStatus::Unhealthy
        );
    }

    #[test]
    fn capacity_warns_before_the_hard_limit() {
        let (status, message) = classify_capacity(3 * GIB, Some(15_000), GIB, 10_000);
        assert_eq!(status, DependencyStatus::Degraded);
        assert!(message.contains("15000 inodes free"));
    }

    #[test]
    fn capacity_is_healthy_above_both_warning_limits() {
        assert_eq!(
            classify_capacity(3 * GIB, Some(30_000), GIB, 10_000).0,
            DependencyStatus::Healthy
        );
    }
}
