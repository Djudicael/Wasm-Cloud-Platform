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

    /// Clear a previous undeploy marker when an application is deployed again.
    pub fn mark_deployed(&self, app_name: &str) -> Result<(), PlatformError> {
        let meta_key = format!("_undeploy:{app_name}");
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
        info!(app = %app_name, "application marked as deployed");
        Ok(())
    }

    /// Return whether an application is inside its undeploy grace period.
    ///
    /// Its artifacts remain recoverable until GC runs, but it must not be
    /// restored into a supervisor pool or exposed as a deployed application.
    pub fn is_undeployed(&self, app_name: &str) -> Result<bool, PlatformError> {
        let meta_key = format!("_undeploy:{app_name}");
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(SCHEMA_META)
            .map_err(PlatformError::storage_source)?;
        let marker = table
            .get(meta_key.as_str())
            .map_err(PlatformError::storage_source)?;
        Ok(marker.is_some())
    }

    /// List configurations that represent currently deployed applications.
    /// Undeploy grace-period artifacts intentionally remain in `list_apps()`
    /// for recovery/GC, but must not enter runtime or bootstrap state.
    pub fn list_deployed_apps(&self) -> Result<Vec<common::types::AppId>, PlatformError> {
        self.list_apps()?
            .into_iter()
            .filter_map(|app_id| match self.is_undeployed(&app_id.0) {
                Ok(false) => Some(Ok(app_id)),
                Ok(true) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Purge complete application IDs after their undeploy grace period.
    pub fn gc_undeployed_apps(&self, grace_secs: u64) -> Result<u64, PlatformError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(PlatformError::storage_source)?
            .as_secs();
        let candidates = {
            let tx = self
                .db
                .begin_read()
                .map_err(PlatformError::storage_source)?;
            let table = tx
                .open_table(SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            let mut candidates = Vec::new();
            for entry in table.iter().map_err(PlatformError::storage_source)? {
                let (key, value) = entry.map_err(PlatformError::storage_source)?;
                if let Some(app) = key.value().strip_prefix("_undeploy:") {
                    let timestamp = value
                        .value()
                        .parse::<u64>()
                        .map_err(PlatformError::storage_source)?;
                    if now
                        .checked_sub(timestamp)
                        .is_some_and(|age| age > grace_secs)
                    {
                        candidates.push((app.to_owned(), value.value().to_owned()));
                    }
                }
            }
            candidates
        };
        let mut purged = 0;
        for (app, marker) in candidates {
            if self.purge_undeployed_candidate(&app, &marker, now, grace_secs)? {
                purged += 1;
                info!(app=%app,"GC: purged artifact/config state; credentials, gateway policies and shared blobs retained");
            }
        }
        Ok(purged)
    }

    /// Recheck the selection fence and remove one incarnation in one transaction.
    /// A deployment commit either runs before this transaction (clearing its
    /// marker) or afterwards (writing all replacement state together).
    fn purge_undeployed_candidate(
        &self,
        app: &str,
        expected_marker: &str,
        now: u64,
        grace_secs: u64,
    ) -> Result<bool, PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        let marker_key = format!("_undeploy:{app}");
        let mut meta = tx
            .open_table(SCHEMA_META)
            .map_err(PlatformError::storage_source)?;
        let marker = meta
            .get(marker_key.as_str())
            .map_err(PlatformError::storage_source)?
            .map(|v| v.value().to_owned());
        if marker.as_deref() != Some(expected_marker) {
            return Ok(false);
        }
        let timestamp = expected_marker
            .parse::<u64>()
            .map_err(PlatformError::storage_source)?;
        if !now
            .checked_sub(timestamp)
            .is_some_and(|age| age > grace_secs)
        {
            return Ok(false);
        }

        tx.open_table(ARTIFACTS)
            .map_err(PlatformError::storage_source)?
            .remove(app)
            .map_err(PlatformError::storage_source)?;
        tx.open_table(CONFIGS)
            .map_err(PlatformError::storage_source)?
            .remove(app)
            .map_err(PlatformError::storage_source)?;
        tx.open_table(ARTIFACT_HASHES)
            .map_err(PlatformError::storage_source)?
            .remove(app)
            .map_err(PlatformError::storage_source)?;
        {
            let mut metrics = tx
                .open_table(METRICS)
                .map_err(PlatformError::storage_source)?;
            let prefix = format!("{app}:");
            let mut keys = Vec::new();
            for entry in metrics.iter().map_err(PlatformError::storage_source)? {
                let (key, _) = entry.map_err(PlatformError::storage_source)?;
                if key.value().starts_with(&prefix) {
                    keys.push(key.value().to_owned());
                }
            }
            for key in keys {
                metrics
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        {
            let mut routes = tx
                .open_table(ROUTES)
                .map_err(PlatformError::storage_source)?;
            let mut keys = Vec::new();
            for entry in routes.iter().map_err(PlatformError::storage_source)? {
                let (key, value) = entry.map_err(PlatformError::storage_source)?;
                let route: common::types::Route =
                    serde_json::from_str(value.value()).map_err(PlatformError::storage_source)?;
                if route.app_id.0 == app {
                    keys.push(key.value().to_owned());
                }
            }
            for key in keys {
                routes
                    .remove(key.as_str())
                    .map_err(PlatformError::storage_source)?;
            }
        }
        let mut cursors = Vec::new();
        for entry in meta.iter().map_err(PlatformError::storage_source)? {
            let (key, _) = entry.map_err(PlatformError::storage_source)?;
            if let Some(value) = key.value().strip_prefix("schedule_cursor:") {
                let (_, id, _): (String, String, String) =
                    serde_json::from_str(value).map_err(PlatformError::storage_source)?;
                if id == app {
                    cursors.push(key.value().to_owned());
                }
            }
        }
        for key in cursors {
            meta.remove(key.as_str())
                .map_err(PlatformError::storage_source)?;
        }
        // RAW_WASM/verification records are keyed by content hash, potentially
        // shared by other applications. Secrets, API keys, gateway policies and lifecycle ordering fences
        // are retained; neither is an application artifact GC target.
        meta.remove(marker_key.as_str())
            .map_err(PlatformError::storage_source)?;
        drop(meta);
        tx.commit().map_err(PlatformError::storage_source)?;
        Ok(true)
    }
    pub fn list_routes_for_app(
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
            let mut successful = true;

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
                Err(e) => {
                    successful = false;
                    warn!(error = %e, "GC: artifact cleanup failed");
                }
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
                Err(e) => {
                    successful = false;
                    warn!(error = %e, "GC: metrics cleanup failed");
                }
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
                Err(e) => {
                    successful = false;
                    warn!(error = %e, "GC: undeploy cleanup failed");
                }
            }

            // 4. Check disk usage and update metrics
            if let Err(e) = check_disk_usage(&store, &config, metrics.as_deref()) {
                successful = false;
                warn!(error = %e, "GC: disk check failed");
            }
            let completed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if let Err(e)=store.save_meta("gc-last-observation",&serde_json::json!({"completedAtMs":completed,"successful":successful,"intervalSecs":config.gc_interval_secs,"keepVersions":config.artifact_keep_versions,"undeployGraceSecs":config.undeploy_grace_secs}).to_string()){warn!(error=%e,"GC: observation persistence failed");}
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
pub fn get_available_disk_space(path: &std::path::Path) -> Result<u64, PlatformError> {
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
pub fn get_available_disk_space(path: &std::path::Path) -> Result<u64, PlatformError> {
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
pub fn get_available_disk_space(_path: &std::path::Path) -> Result<u64, PlatformError> {
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

#[cfg(test)]
mod undeploy_regressions {
    use super::*;
    use common::types::{AppConfig, AppId, Route};
    fn fixture() -> (Store, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        (Store::open(file.path()).unwrap(), file)
    }
    fn marker(store: &Store, app: &AppId, value: &str) {
        let tx = store.db.begin_write().unwrap();
        tx.open_table(SCHEMA_META)
            .unwrap()
            .insert(format!("_undeploy:{}", app.0).as_str(), value)
            .unwrap();
        tx.commit().unwrap();
    }
    fn seeded(store: &Store, id: &AppId, bytes: &[u8]) {
        store
            .persist_deployment(id, bytes, &AppConfig::default_for(id.clone()), "shared-sha")
            .unwrap();
    }
    #[test]
    fn exact_undeploy_gc_survives_reopen_preserves_neighbours_secrets_and_shared_blobs() {
        let (store, file) = fixture();
        let ids: [AppId; 5] = [
            "agents/tool:v1",
            "agents/tool:v2",
            "other/tool:v1",
            "agents/toolbox:v1",
            "agents/tool:v10",
        ]
        .map(|s| AppId(s.into()));
        for id in &ids {
            seeded(&store, id, b"compiled");
            store.save_secrets(id, b"encrypted-secret").unwrap();
        }
        store
            .save_raw_wasm("shared-sha", b"shared-raw-wasm")
            .unwrap();
        let tx = store.db.begin_write().unwrap();
        for id in &ids {
            tx.open_table(GATEWAY_CONFIGS)
                .unwrap()
                .insert(id.0.as_str(), "{}")
                .unwrap();
            tx.open_table(API_KEYS)
                .unwrap()
                .insert(id.0.as_str(), "[]")
                .unwrap();
            tx.open_table(METRICS)
                .unwrap()
                .insert(format!("{}:100", id.0).as_str(), "{}")
                .unwrap();
            let route = Route {
                host: format!("{}.local", id.0.replace(['/', ':'], "-")),
                app_id: id.clone(),
                path_prefix: "/".into(),
                strip_prefix: false,
                created_at: 1,
                updated_at: 1,
            };
            let json = serde_json::to_string(&route).unwrap();
            tx.open_table(ROUTES)
                .unwrap()
                .insert(route.host.as_str(), json.as_str())
                .unwrap();
            tx.open_table(SCHEMA_META)
                .unwrap()
                .insert(
                    crate::triggers::cursor_key("node", id, "tick").as_str(),
                    "{}",
                )
                .unwrap();
        }
        tx.commit().unwrap();
        let target = &ids[0];
        store
            .begin_lifecycle_event("stream", "epoch", 99, target, true)
            .unwrap();
        store.mark_undeployed(&target.0).unwrap();
        marker(&store, target, "1");
        assert_eq!(store.gc_undeployed_apps(0).unwrap(), 1);
        drop(store);
        let store = Store::open(file.path()).unwrap();
        assert!(store.load_config(target).unwrap().is_none());
        assert!(store.load_artifact(target).unwrap().is_none());
        assert!(store.get_artifact_sha256(target).unwrap().is_none());
        assert!(!store.list_deployed_apps().unwrap().contains(target));
        assert!(!store.is_undeployed(&target.0).unwrap());
        assert_eq!(
            store.load_secrets(target).unwrap().unwrap(),
            b"encrypted-secret"
        );
        let tx = store.db.begin_read().unwrap();
        assert_eq!(
            tx.open_table(RAW_WASM)
                .unwrap()
                .get("shared-sha")
                .unwrap()
                .unwrap()
                .value(),
            b"shared-raw-wasm"
        );
        assert!(tx
            .open_table(GATEWAY_CONFIGS)
            .unwrap()
            .get(target.0.as_str())
            .unwrap()
            .is_some());
        assert!(tx
            .open_table(API_KEYS)
            .unwrap()
            .get(target.0.as_str())
            .unwrap()
            .is_some());
        assert!(tx
            .open_table(METRICS)
            .unwrap()
            .get(format!("{}:100", target.0).as_str())
            .unwrap()
            .is_none());
        assert!(tx
            .open_table(SCHEMA_META)
            .unwrap()
            .get(crate::triggers::cursor_key("node", target, "tick").as_str())
            .unwrap()
            .is_none());
        for id in &ids[1..] {
            assert_eq!(store.load_artifact(id).unwrap().unwrap(), b"compiled");
            assert!(store.load_config(id).unwrap().is_some());
            assert_eq!(
                store.get_artifact_sha256(id).unwrap().as_deref(),
                Some("shared-sha")
            );
            assert!(tx
                .open_table(GATEWAY_CONFIGS)
                .unwrap()
                .get(id.0.as_str())
                .unwrap()
                .is_some());
            assert!(tx
                .open_table(API_KEYS)
                .unwrap()
                .get(id.0.as_str())
                .unwrap()
                .is_some());
            assert!(tx
                .open_table(METRICS)
                .unwrap()
                .get(format!("{}:100", id.0).as_str())
                .unwrap()
                .is_some());
            assert!(tx
                .open_table(SCHEMA_META)
                .unwrap()
                .get(crate::triggers::cursor_key("node", id, "tick").as_str())
                .unwrap()
                .is_some());
            assert_eq!(store.list_routes_for_app(&id.0).unwrap().len(), 1);
        }
        assert!(store.list_routes_for_app(&target.0).unwrap().is_empty());
        // Artifact GC must not erase the broker ordering fence.
        assert!(!store
            .begin_lifecycle_event("stream", "epoch", 98, target, false)
            .unwrap());
    }
    #[test]
    fn stale_candidate_cannot_purge_redeployment_or_a_new_undeploy_marker() {
        let (store, _file) = fixture();
        let id = AppId("agents/redeploy:v1".into());
        seeded(&store, &id, b"old");
        marker(&store, &id, "1");
        seeded(&store, &id, b"new");
        assert!(!store
            .purge_undeployed_candidate(&id.0, "1", 100, 0)
            .unwrap());
        marker(&store, &id, "2");
        assert!(!store
            .purge_undeployed_candidate(&id.0, "1", 100, 0)
            .unwrap());
        assert!(!store.purge_undeployed_candidate(&id.0, "2", 2, 0).unwrap());
        assert_eq!(store.load_artifact(&id).unwrap().unwrap(), b"new");
        assert!(store.is_undeployed(&id.0).unwrap());
    }
    #[test]
    fn purge_failure_rolls_back_every_deletion_and_retains_marker() {
        let (store, _file) = fixture();
        let id = AppId("agents/rollback:v1".into());
        seeded(&store, &id, b"old");
        marker(&store, &id, "1");
        let tx = store.db.begin_write().unwrap();
        tx.open_table(ROUTES)
            .unwrap()
            .insert("invalid.local", "invalid-json")
            .unwrap();
        tx.commit().unwrap();
        assert!(store
            .purge_undeployed_candidate(&id.0, "1", 100, 0)
            .is_err());
        assert_eq!(store.load_artifact(&id).unwrap().unwrap(), b"old");
        assert!(store.load_config(&id).unwrap().is_some());
        assert_eq!(
            store.get_artifact_sha256(&id).unwrap().as_deref(),
            Some("shared-sha")
        );
        assert!(store.is_undeployed(&id.0).unwrap());
    }
    #[test]
    fn concurrent_gc_and_atomic_redeploy_leave_complete_new_deployment() {
        let (store, _file) = fixture();
        let id = AppId("agents/race:v1".into());
        seeded(&store, &id, b"old");
        marker(&store, &id, "1");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (gc, app, gate) = (store.clone(), id.clone(), barrier.clone());
        let task = std::thread::spawn(move || {
            gate.wait();
            gc.purge_undeployed_candidate(&app.0, "1", 100, 0).unwrap()
        });
        barrier.wait();
        store
            .persist_deployment(&id, b"new", &AppConfig::default_for(id.clone()), "new-sha")
            .unwrap();
        task.join().unwrap();
        assert_eq!(store.load_artifact(&id).unwrap().unwrap(), b"new");
        assert!(store.load_config(&id).unwrap().is_some());
        assert_eq!(
            store.get_artifact_sha256(&id).unwrap().as_deref(),
            Some("new-sha")
        );
        assert!(!store.is_undeployed(&id.0).unwrap());
    }
    #[test]
    fn atomic_deployment_rejects_mismatched_identity_without_changing_state() {
        let (store, _file) = fixture();
        let id = AppId("agents/identity:v1".into());
        seeded(&store, &id, b"old");
        marker(&store, &id, "1");
        assert!(store
            .persist_deployment(
                &id,
                b"new",
                &AppConfig::default_for(AppId("other/identity:v1".into())),
                "new-sha"
            )
            .is_err());
        assert_eq!(store.load_artifact(&id).unwrap().unwrap(), b"old");
        assert!(store.is_undeployed(&id.0).unwrap());
    }
}
