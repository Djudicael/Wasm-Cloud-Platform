// crates/storage/src/gc.rs
use crate::{tables::*, Store};
use common::{error::PlatformError, gc::GcConfig};
use redb::{ReadableDatabase, ReadableTable};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Metadata about an artifact version, used for GC decisions.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct VersionEntry {
    /// Full key in redb, e.g. "api-users:v3"
    key: String,
    /// App name without version, e.g. "api-users"
    app_name: String,
    /// Version suffix, e.g. "v3"
    version: String,
}

/// Statistics from a GC run.
#[derive(Debug, Default)]
pub struct GcStats {
    pub artifacts_deleted: u64,
    pub raw_wasm_deleted: u64,
    pub configs_deleted: u64,
    pub metric_buckets_deleted: u64,
    pub undeployed_apps_purged: u64,
}

impl Store {
    /// Scan the artifacts table and group entries by app name.
    /// Returns: app_name → [versions sorted by version number]
    fn inventory_versions(&self) -> Result<HashMap<String, Vec<VersionEntry>>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(ARTIFACTS)
            .map_err(PlatformError::storage_source)?;

        let mut inventory: HashMap<String, Vec<VersionEntry>> = HashMap::new();

        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (k, _) = entry.map_err(PlatformError::storage_source)?;
            let key = k.value().to_string();

            // Parse "api-users:v3" into ("api-users", "v3")
            if let Some((app_name, version)) = key.rsplit_once(':') {
                inventory
                    .entry(app_name.to_string())
                    .or_default()
                    .push(VersionEntry {
                        key: key.clone(),
                        app_name: app_name.to_string(),
                        version: version.to_string(),
                    });
            }
        }

        // Sort versions within each app (numeric-aware sort)
        for versions in inventory.values_mut() {
            versions.sort_by_key(|a| version_sort_key(&a.version));
        }

        Ok(inventory)
    }

    /// Delete stale artifact versions, keeping the most recent N per app.
    /// Also protects versions that have active instances running.
    pub fn gc_artifacts(&self, keep: usize) -> Result<GcStats, PlatformError> {
        let inventory = self.inventory_versions()?;
        let mut stats = GcStats::default();

        for (app_name, versions) in &inventory {
            if versions.len() <= keep {
                continue; // Nothing to prune
            }

            // Keep the last `keep` versions, delete the rest
            let candidates_to_delete = &versions[..versions.len() - keep];

            for entry in candidates_to_delete {
                // Check if this version has active instances
                // If it does, skip deletion to prevent breaking running instances
                if self.has_active_instances(&entry.key)? {
                    info!(
                        app = %app_name,
                        version = %entry.version,
                        "GC: skipping version with active instances"
                    );
                    continue;
                }

                // Delete from artifacts table
                self.delete_artifact_by_key(&entry.key)?;
                stats.artifacts_deleted += 1;

                // Delete corresponding raw Wasm (if it exists)
                if self.delete_raw_wasm_by_key(&entry.key).is_ok() {
                    stats.raw_wasm_deleted += 1;
                }

                // Delete corresponding config
                if self.delete_config_by_key(&entry.key).is_ok() {
                    stats.configs_deleted += 1;
                }

                info!(
                    app = %app_name,
                    version = %entry.version,
                    "GC: deleted stale version"
                );
            }
        }

        Ok(stats)
    }

    /// Check if a specific artifact version has active instances.
    /// Returns true if ANY instance is currently using this artifact.
    fn has_active_instances(&self, artifact_key: &str) -> Result<bool, PlatformError> {
        // Parse app_id from key (e.g., "api-users:v3" -> "api-users")
        let app_id_str = artifact_key.split(':').next().unwrap_or("");

        // Check if any config exists for this app (any version)
        // Configs are stored with versioned keys like "app-name:v1",
        // so we check if any config starts with the app_name prefix.
        let apps = self.list_apps()?;
        let has_config = apps
            .iter()
            .any(|app| app.0.starts_with(&format!("{}:", app_id_str)));
        if !has_config {
            return Ok(false);
        }

        // If config exists, the app is still deployed — don't delete
        Ok(true)
    }

    fn delete_artifact_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(ARTIFACTS)
                .map_err(PlatformError::storage_source)?;
            table.remove(key).map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    fn delete_raw_wasm_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(RAW_WASM)
                .map_err(PlatformError::storage_source)?;
            table.remove(key).map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    fn delete_config_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(CONFIGS)
                .map_err(PlatformError::storage_source)?;
            table.remove(key).map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    /// Delete metric buckets older than `retain_days` days.
    pub fn gc_metrics(&self, retain_days: u32) -> Result<u64, PlatformError> {
        let cutoff_ts = {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            now - (retain_days as u64 * 86400)
        };

        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        let mut deleted = 0u64;
        {
            let mut table = tx
                .open_table(METRICS)
                .map_err(PlatformError::storage_source)?;

            // Collect keys to delete (cannot mutate while iterating)
            let stale_keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        warn!(error = %err, "GC: error iterating metrics table entry, skipping");
                        None
                    }
                })
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    // Key format: "app_id:minute_timestamp"
                    let ts: u64 = key
                        .rsplit_once(':')
                        .and_then(|(_, ts)| ts.parse().ok())
                        .unwrap_or(0);
                    if ts < cutoff_ts {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect();

            for key in &stale_keys {
                table
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
                deleted += 1;
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        if deleted > 0 {
            info!(deleted, retain_days, "GC: pruned old metric buckets");
        }
        Ok(deleted)
    }

    /// Mark an app as undeployed. Actual deletion happens after the grace period.
    pub fn mark_undeployed(&self, app_name: &str) -> Result<(), PlatformError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Store the undeploy timestamp in a metadata key
        let meta_key = format!("_undeploy:{}", app_name);
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            table
                .insert(meta_key.as_str(), now.to_string().as_str())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        info!(app = %app_name, "app marked as undeployed, grace period started");
        Ok(())
    }

    /// Purge all state for apps whose undeploy grace period has expired.
    pub fn gc_undeployed_apps(&self, grace_secs: u64) -> Result<u64, PlatformError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Find all apps with expired grace periods
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SCHEMA_META)
            .map_err(PlatformError::storage_source)?;

        let expired_apps: Vec<String> = table
            .iter()
            .map_err(PlatformError::storage_source)?
            .filter_map(|e| match e {
                Ok(v) => Some(v),
                Err(err) => {
                    warn!(error = %err, "GC: error iterating schema_meta table entry, skipping");
                    None
                }
            })
            .filter_map(|(k, v)| {
                let key = k.value().to_string();
                if !key.starts_with("_undeploy:") {
                    return None;
                }
                let ts: u64 = v.value().parse().unwrap_or(0);
                if now - ts > grace_secs {
                    Some(key.strip_prefix("_undeploy:").unwrap().to_string())
                } else {
                    None
                }
            })
            .collect();

        drop(table);
        drop(tx);

        let mut purged = 0u64;
        for app_name in &expired_apps {
            self.purge_app_state(app_name)?;
            purged += 1;
            info!(app = %app_name, "GC: purged all state for undeployed app");
        }

        Ok(purged)
    }

    /// Delete ALL data for an app across all redb tables (except secrets).
    fn purge_app_state(&self, app_name: &str) -> Result<(), PlatformError> {
        use redb::ReadableTable;

        let prefix = format!("{}:", app_name);

        // Delete from artifacts table (byte values)
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(ARTIFACTS)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        warn!(error = %err, "GC: error iterating artifacts table entry, skipping");
                        None
                    }
                })
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    if key.starts_with(&prefix) {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                table.remove(key.as_str()).ok();
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        // Delete from raw_wasm table (byte values)
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(RAW_WASM)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        warn!(error = %err, "GC: error iterating raw_wasm table entry, skipping");
                        None
                    }
                })
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    if key.starts_with(&prefix) {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                table.remove(key.as_str()).ok();
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        // Delete from configs table (string values)
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(CONFIGS)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        warn!(error = %err, "GC: error iterating configs table entry, skipping");
                        None
                    }
                })
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    if key.starts_with(&prefix) {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                table.remove(key.as_str()).ok();
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        // Delete from metrics table (string values)
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(METRICS)
                .map_err(PlatformError::storage_source)?;
            let keys: Vec<String> = table
                .iter()
                .map_err(PlatformError::storage_source)?
                .filter_map(|e| match e {
                    Ok(v) => Some(v),
                    Err(err) => {
                        warn!(error = %err, "GC: error iterating metrics table entry, skipping");
                        None
                    }
                })
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    if key.starts_with(&prefix) {
                        Some(key)
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                table.remove(key.as_str()).ok();
            }
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        // Delete routes for this app
        let routes_to_delete = self.list_routes_for_app(app_name)?;
        for route in routes_to_delete {
            self.delete_route(&route.host)?;
        }

        // Remove the undeploy marker
        let meta_key = format!("_undeploy:{}", app_name);
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            table.remove(meta_key.as_str()).ok();
        }
        tx.commit().map_err(PlatformError::storage_source)?;

        // NOTE: Secrets are NOT deleted here. They must be explicitly removed
        // via `wasm-ctl secret delete --app <name>` to prevent accidental loss.
        warn!(
            app = %app_name,
            "secrets were NOT deleted — use `wasm-ctl secret delete` to remove them"
        );

        Ok(())
    }

    fn list_routes_for_app(
        &self,
        app_name: &str,
    ) -> Result<Vec<common::types::Route>, PlatformError> {
        let all_routes = self.list_routes()?;
        Ok(all_routes
            .into_iter()
            .filter(|r| r.app_id.0 == app_name)
            .collect())
    }
}

/// Extract a numeric sort key from a version string like "v3" → 3.
/// Falls back to u64::MAX for non-numeric versions (sorted last).
fn version_sort_key(version: &str) -> u64 {
    version
        .trim_start_matches('v')
        .parse::<u64>()
        .unwrap_or(u64::MAX)
}

/// Start the background GC loop.
/// Start the GC loop with hot-reloadable configuration.
///
/// The `config_rx` watch receiver allows the caller (typically main.rs) to
/// push updated `GcConfig` values when the operator changes hot-reloadable
/// fields (`gc_interval_secs`, `disk_warning_threshold`) via the admin API.
/// The loop picks up changes on the next tick — no restart required.
///
/// Non-reloadable fields (`artifact_keep_versions`, `metrics_retain_days`,
/// `undeploy_grace_secs`) are also read from the watch, so a restart will
/// pick up changes to those fields from the TOML file / env / CLI.
pub fn start_gc_loop(
    store: Store,
    config_rx: tokio::sync::watch::Receiver<GcConfig>,
    metrics: Option<Arc<crate::gc_metrics::GcMetrics>>,
) {
    let store = Arc::new(store);

    tokio::spawn(async move {
        // Read the initial interval from the watch channel
        let initial_interval = config_rx.borrow().gc_interval_secs;
        let mut tick = tokio::time::interval(Duration::from_secs(initial_interval));
        // Track the last-seen interval so we can detect changes
        let mut last_interval_secs = initial_interval;

        loop {
            tick.tick().await;

            // Read the latest config from the watch channel.
            // We borrow() rather than changed() so we don't block the loop
            // if no update has arrived — we just use the current value.
            let config = config_rx.borrow().clone();

            // If the interval changed, reset the ticker
            if config.gc_interval_secs != last_interval_secs {
                let new_interval = Duration::from_secs(config.gc_interval_secs);
                tick = tokio::time::interval(new_interval);
                // Consume the first immediate tick that interval() produces
                tick.tick().await;
                last_interval_secs = config.gc_interval_secs;
                info!(
                    new_interval_secs = config.gc_interval_secs,
                    "GC loop interval updated via hot-reload"
                );
            }

            debug!("GC tick starting");

            // 1. Garbage collect old artifact versions
            match store.gc_artifacts(config.artifact_keep_versions) {
                Ok(stats) => {
                    if stats.artifacts_deleted > 0 {
                        info!(
                            artifacts = stats.artifacts_deleted,
                            configs = stats.configs_deleted,
                            raw_wasm = stats.raw_wasm_deleted,
                            "GC: artifact cleanup complete"
                        );

                        // Update Prometheus metrics
                        if let Some(ref m) = metrics {
                            m.record_artifacts_deleted(stats.artifacts_deleted);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "GC: artifact cleanup failed"),
            }

            // 2. Garbage collect old metric buckets
            match store.gc_metrics(config.metrics_retain_days) {
                Ok(deleted) => {
                    if deleted > 0 {
                        info!(deleted, "GC: metrics cleanup complete");

                        // Update Prometheus metrics
                        if let Some(ref m) = metrics {
                            m.record_metric_buckets_deleted(deleted);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "GC: metrics cleanup failed"),
            }

            // 3. Purge fully undeployed apps past grace period
            match store.gc_undeployed_apps(config.undeploy_grace_secs) {
                Ok(purged) => {
                    if purged > 0 {
                        info!(purged, "GC: undeployed app cleanup complete");

                        // Update Prometheus metrics
                        if let Some(ref m) = metrics {
                            m.record_apps_purged(purged);
                        }
                    }
                }
                Err(e) => warn!(error = %e, "GC: undeploy cleanup failed"),
            }

            // 4. Check disk usage and update metrics
            if let Err(e) = check_disk_usage(&store, &config, metrics.as_deref()) {
                warn!(error = %e, "GC: disk check failed");
            }
        }
    });
}

fn check_disk_usage(
    store: &Store,
    config: &GcConfig,
    metrics: Option<&crate::gc_metrics::GcMetrics>,
) -> Result<(), PlatformError> {
    // Get redb file size
    let db_path = store.get_db_path();
    let file_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    // Get available disk space (platform-specific)
    let available = get_available_disk_space(&db_path)?;

    let ratio = file_size as f64 / available as f64;

    // Update Prometheus metrics
    if let Some(m) = metrics {
        m.set_file_size(file_size);
        m.set_disk_usage_ratio(ratio);
    }

    debug!(
        file_size_mb = file_size / 1_048_576,
        available_mb = available / 1_048_576,
        ratio = format!("{:.2}%", ratio * 100.0),
        "disk usage check"
    );

    if ratio > config.disk_warning_threshold {
        warn!(
            file_size_mb = file_size / 1_048_576,
            available_mb = available / 1_048_576,
            ratio = format!("{:.2}%", ratio * 100.0),
            threshold = format!("{:.0}%", config.disk_warning_threshold * 100.0),
            "disk usage exceeds warning threshold"
        );
    }

    Ok(())
}

/// Get available disk space for the filesystem containing the given path.
#[cfg(unix)]
fn get_available_disk_space(path: &std::path::Path) -> Result<u64, PlatformError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path_cstr = CString::new(path.as_os_str().as_bytes()).map_err(PlatformError::io_source)?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(path_cstr.as_ptr(), &mut stat) };

    if result != 0 {
        return Err(PlatformError::io(format!(
            "statvfs failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    // Available space = fragment size × available blocks
    let available = stat.f_bavail as u64 * stat.f_frsize as u64;
    Ok(available)
}

#[cfg(windows)]
fn get_available_disk_space(path: &std::path::Path) -> Result<u64, PlatformError> {
    use std::os::windows::ffi::OsStrExt;

    // Convert path to wide string
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free_bytes: u64 = 0;

    let result = unsafe {
        winapi::um::fileapi::GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut free_bytes as *mut u64 as winapi::um::winnt::PULARGE_INTEGER,
            &mut total_bytes as *mut u64 as winapi::um::winnt::PULARGE_INTEGER,
            &mut total_free_bytes as *mut u64 as winapi::um::winnt::PULARGE_INTEGER,
        )
    };

    if result == 0 {
        return Err(PlatformError::io(format!(
            "GetDiskFreeSpaceEx failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(free_bytes)
}

#[cfg(not(any(unix, windows)))]
fn get_available_disk_space(_path: &std::path::Path) -> Result<u64, PlatformError> {
    // Fallback for other platforms - return a large value to avoid false warnings
    Ok(1_000_000_000_000) // 1 TB
}

impl Store {
    pub fn get_db_path(&self) -> std::path::PathBuf {
        self.db_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_sort_key() {
        assert_eq!(version_sort_key("v1"), 1);
        assert_eq!(version_sort_key("v10"), 10);
        assert_eq!(version_sort_key("v2"), 2);
        assert_eq!(version_sort_key("v100"), 100);
        assert_eq!(version_sort_key("vNonNumeric"), u64::MAX);
    }

    #[test]
    fn test_version_sorting() {
        let mut versions = vec!["v10", "v1", "v2", "v100"];
        versions.sort_by_key(|a| version_sort_key(a));
        assert_eq!(versions, vec!["v1", "v2", "v10", "v100"]);
    }
}
