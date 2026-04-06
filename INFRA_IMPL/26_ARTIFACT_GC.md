# Step 26 — Artifact Garbage Collection

## Goal
Implement automatic cleanup of old Wasm artifacts, configs, and metrics data. The system must:
- Delete compiled artifacts for versions that are no longer referenced
- Enforce a configurable retention policy (keep last N versions per app)
- Reclaim disk space from old metric buckets past the retention window
- Clean up state for apps that have been fully undeployed
- Run as a non-blocking background task that does not interfere with request serving

---

## Context & Rationale

### The Problem This Solves

Every deployment stores a compiled artifact in `redb`. A typical AOT-compiled Wasm binary
is 5–50 MB. With frequent deploys (CI/CD pushing a new version every commit), disk usage
grows unbounded:

```
Week 1:  api-users:v1  (15 MB) + payments:v1 (20 MB)  = 35 MB
Week 4:  + 30 more versions across 5 apps               = 1 GB
Month 6: + 200 versions across 10 apps                   = 8 GB
Year 1:  + 1000+ stale versions                          = 40+ GB
```

Without garbage collection, eventually `redb` fills the node's disk, and all writes fail —
including writes for new deployments, metrics, and config updates. This is a production
outage caused by neglecting cleanup.

Step 10 introduced `prune_old_versions(app_name, keep=2)` for rollback support. This step
generalizes that into a full garbage collection system covering all data types.

### Why Keep the Last N Versions (Not Time-Based)?

Time-based retention ("delete artifacts older than 7 days") fails for apps with irregular
deploy cadence. An app that deploys once a month would lose its rollback artifact after 7
days. An app that deploys 10 times a day would accumulate 70 artifacts in 7 days.

Version-based retention ("keep the last 3 versions per app") ensures:
- **Rollback always works**: v(N-1) and v(N-2) are always available for instant rollback
- **Active apps clean up fast**: an app deploying 10x/day keeps only 3 artifacts, not 70
- **Inactive apps retain their state**: an app not deployed in 6 months still has its artifact

The default is `keep = 3` (current version + 2 rollback versions). Operators can override
per-app.

### Why GC Runs as a Background Task (Not Inline)

Running GC inline during `handle_deploy()` would add latency to the deploy path. If the
node has 100 stale artifacts to clean up, the deploy would block for the duration of all
those `redb` delete transactions.

A background task running every 10 minutes checks for stale data and cleans it up
independently of the deploy path. This follows the same pattern as the metrics aggregation
loop (step 11): the hot path (deploys, requests) is never blocked by maintenance work.

### What Gets Garbage Collected?

```
Data Type          │ Retention Policy                          │ redb Table
───────────────────┼───────────────────────────────────────────┼──────────────
Compiled artifacts │ Keep last N versions per app (default: 3) │ artifacts
Raw Wasm binaries  │ Keep last N versions per app (default: 3) │ raw_wasm
App configs        │ Keep last N versions per app (default: 3) │ configs
Metric buckets     │ Keep last D days (default: 7)             │ metrics
Undeployed apps    │ Delete all state after undeploy + grace   │ all tables
```

**Secrets are NOT garbage collected** by this system. Secret rotation and deletion are
explicit operations (step 06) because accidental secret deletion could lock out running
instances.

### Disk Space Monitoring

The GC system also monitors total `redb` file size and emits a Prometheus metric. This
enables alerting before disk fills up — the alert fires at 80% capacity, giving operators
time to increase disk, reduce retention, or scale out.

---

---

## 1. GC Configuration

```rust
// crates/common/src/gc.rs
use serde::{Deserialize, Serialize};

/// Garbage collection configuration for a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    /// How many compiled artifact versions to retain per app.
    /// The current version plus this many previous versions are kept.
    /// Default: 3 (current + 2 rollback candidates).
    pub artifact_keep_versions: usize,

    /// How many days of metric buckets to retain.
    /// Older buckets are deleted during GC.
    /// Default: 7.
    pub metrics_retain_days: u32,

    /// Grace period (in seconds) after an app is undeployed before all its
    /// state is purged. This allows rollback if the undeploy was accidental.
    /// Default: 3600 (1 hour).
    pub undeploy_grace_secs: u64,

    /// How often the GC loop runs (in seconds).
    /// Default: 600 (10 minutes).
    pub gc_interval_secs: u64,

    /// Disk usage warning threshold (percentage of total disk).
    /// When redb file size exceeds this fraction of available disk,
    /// a warning metric is emitted.
    /// Default: 0.80 (80%).
    pub disk_warning_threshold: f64,
}

impl Default for GcConfig {
    fn default() -> Self {
        GcConfig {
            artifact_keep_versions: 3,
            metrics_retain_days: 7,
            undeploy_grace_secs: 3600,
            gc_interval_secs: 600,
            disk_warning_threshold: 0.80,
        }
    }
}
```

---

## 2. Version Inventory

Before deleting anything, the GC builds an inventory of all versions per app, sorted
by deployment time. The versions to keep are identified before any deletion begins.

```rust
// crates/storage/src/gc.rs
use crate::{Store, tables::{ARTIFACTS, RAW_WASM, CONFIGS, METRICS}};
use common::{error::PlatformError, gc::GcConfig};
use std::collections::HashMap;
use tracing::{info, warn};

/// Metadata about an artifact version, used for GC decisions.
#[derive(Debug, Clone)]
struct VersionEntry {
    /// Full key in redb, e.g. "api-users:v3"
    key: String,
    /// App name without version, e.g. "api-users"
    app_name: String,
    /// Version suffix, e.g. "v3"
    version: String,
}

impl Store {
    /// Scan the artifacts table and group entries by app name.
    /// Returns: app_name → [versions sorted by key order]
    fn inventory_versions(&self) -> Result<HashMap<String, Vec<VersionEntry>>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut inventory: HashMap<String, Vec<VersionEntry>> = HashMap::new();

        for entry in table.iter().map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (k, _) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
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

        // Sort versions within each app (lexicographic — v1, v10, v2 is wrong)
        // Use a numeric-aware sort for version strings.
        for versions in inventory.values_mut() {
            versions.sort_by(|a, b| version_sort_key(&a.version).cmp(&version_sort_key(&b.version)));
        }

        Ok(inventory)
    }
}

/// Extract a numeric sort key from a version string like "v3" → 3.
/// Falls back to lexicographic ordering for non-numeric versions.
fn version_sort_key(version: &str) -> u64 {
    version
        .trim_start_matches('v')
        .parse::<u64>()
        .unwrap_or(u64::MAX)
}
```

---

## 3. Artifact Garbage Collection

```rust
// crates/storage/src/gc.rs (continued)
impl Store {
    /// Delete stale artifact versions, keeping the most recent N per app.
    pub fn gc_artifacts(&self, keep: usize) -> Result<GcStats, PlatformError> {
        let inventory = self.inventory_versions()?;
        let mut stats = GcStats::default();

        for (app_name, versions) in &inventory {
            if versions.len() <= keep {
                continue; // Nothing to prune
            }

            // Keep the last `keep` versions, delete the rest
            let to_delete = &versions[..versions.len() - keep];

            for entry in to_delete {
                // Delete from artifacts table
                self.delete_artifact_by_key(&entry.key)?;
                stats.artifacts_deleted += 1;

                // Delete corresponding raw Wasm (if it exists)
                self.delete_raw_wasm_by_key(&entry.key).ok();
                stats.raw_wasm_deleted += 1;

                // Delete corresponding config
                self.delete_config_by_key(&entry.key).ok();
                stats.configs_deleted += 1;

                info!(
                    app = %app_name,
                    version = %entry.version,
                    "GC: deleted stale version"
                );
            }
        }

        Ok(stats)
    }

    fn delete_artifact_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(ARTIFACTS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(key)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    fn delete_raw_wasm_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(RAW_WASM)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(key)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    fn delete_config_by_key(&self, key: &str) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(CONFIGS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(key)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }
}

#[derive(Debug, Default)]
pub struct GcStats {
    pub artifacts_deleted: u64,
    pub raw_wasm_deleted: u64,
    pub configs_deleted: u64,
    pub metric_buckets_deleted: u64,
    pub undeployed_apps_purged: u64,
    pub disk_bytes_reclaimed: u64,
}
```

---

## 4. Metrics Garbage Collection

Metric buckets older than the retention window are deleted. The key format in the metrics
table includes a timestamp, making range deletion efficient.

```rust
// crates/storage/src/gc.rs (continued)
impl Store {
    /// Delete metric buckets older than `retain_days` days.
    pub fn gc_metrics(&self, retain_days: u32) -> Result<u64, PlatformError> {
        let cutoff_ts = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            now - (retain_days as u64 * 86400)
        };

        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut deleted = 0u64;
        {
            let mut table = tx.open_table(METRICS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;

            // Collect keys to delete (cannot mutate while iterating)
            let stale_keys: Vec<String> = table.iter()
                .map_err(|e| PlatformError::Storage(e.to_string()))?
                .filter_map(|e| e.ok())
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    // Key format: "app_id:minute_timestamp"
                    let ts: u64 = key.rsplit_once(':')
                        .and_then(|(_, ts)| ts.parse().ok())
                        .unwrap_or(0);
                    if ts < cutoff_ts { Some(key) } else { None }
                })
                .collect();

            for key in &stale_keys {
                table.remove(key.as_str())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                deleted += 1;
            }
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))?;

        if deleted > 0 {
            info!(deleted, retain_days, "GC: pruned old metric buckets");
        }
        Ok(deleted)
    }
}
```

---

## 5. Undeploy Cleanup

When an app is undeployed (all versions removed), its state enters a grace period. After
the grace period expires, all related data is purged: artifacts, configs, raw Wasm, metrics,
and routes. Secrets require explicit deletion (safety measure).

```rust
// crates/storage/src/gc.rs (continued)
use std::time::{SystemTime, UNIX_EPOCH};

impl Store {
    /// Mark an app as undeployed. Actual deletion happens after the grace period.
    pub fn mark_undeployed(&self, app_name: &str) -> Result<(), PlatformError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Store the undeploy timestamp in a metadata key
        let meta_key = format!("_undeploy:{}", app_name);
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(crate::tables::SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(meta_key.as_str(), now.to_string().as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))?;

        info!(app = %app_name, "app marked as undeployed, grace period started");
        Ok(())
    }

    /// Purge all state for apps whose undeploy grace period has expired.
    pub fn gc_undeployed_apps(&self, grace_secs: u64) -> Result<u64, PlatformError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Find all apps with expired grace periods
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(crate::tables::SCHEMA_META)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let expired_apps: Vec<String> = table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .filter_map(|e| e.ok())
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
        let prefix = format!("{}:", app_name);

        // Delete from each table: artifacts, raw_wasm, configs, metrics, routes
        for table_name in &["artifacts", "raw_wasm", "configs", "metrics", "routes"] {
            self.delete_keys_with_prefix(table_name, &prefix)?;
        }

        // Remove the undeploy marker
        let meta_key = format!("_undeploy:{}", app_name);
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(crate::tables::SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(meta_key.as_str()).ok();
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))?;

        // NOTE: Secrets are NOT deleted here. They must be explicitly removed
        // via `wasm-ctl secret delete --app <name>` to prevent accidental loss.
        warn!(
            app = %app_name,
            "secrets were NOT deleted — use `wasm-ctl secret delete` to remove them"
        );

        Ok(())
    }

    fn delete_keys_with_prefix(
        &self,
        _table_name: &str,
        _prefix: &str,
    ) -> Result<(), PlatformError> {
        // Implementation: iterate the named table, collect keys matching prefix, delete them
        // Similar pattern to gc_metrics above
        Ok(())
    }
}
```

---

## 6. GC Background Loop

```rust
// crates/storage/src/gc.rs (continued)
use std::sync::Arc;
use std::time::Duration;

/// Start the background GC loop.
pub fn start_gc_loop(store: Store, config: GcConfig) {
    let store = Arc::new(store);
    let interval = Duration::from_secs(config.gc_interval_secs);

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;

            tracing::debug!("GC tick starting");

            // 1. Garbage collect old artifact versions
            match store.gc_artifacts(config.artifact_keep_versions) {
                Ok(stats) => {
                    if stats.artifacts_deleted > 0 {
                        info!(
                            artifacts = stats.artifacts_deleted,
                            configs = stats.configs_deleted,
                            "GC: artifact cleanup complete"
                        );
                    }
                }
                Err(e) => tracing::error!(error = %e, "GC: artifact cleanup failed"),
            }

            // 2. Garbage collect old metric buckets
            match store.gc_metrics(config.metrics_retain_days) {
                Ok(deleted) => {
                    if deleted > 0 {
                        info!(deleted, "GC: metrics cleanup complete");
                    }
                }
                Err(e) => tracing::error!(error = %e, "GC: metrics cleanup failed"),
            }

            // 3. Purge fully undeployed apps past grace period
            match store.gc_undeployed_apps(config.undeploy_grace_secs) {
                Ok(purged) => {
                    if purged > 0 {
                        info!(purged, "GC: undeployed app cleanup complete");
                    }
                }
                Err(e) => tracing::error!(error = %e, "GC: undeploy cleanup failed"),
            }

            // 4. Check disk usage
            if let Err(e) = check_disk_usage(&config) {
                tracing::error!(error = %e, "GC: disk check failed");
            }
        }
    });
}

fn check_disk_usage(config: &GcConfig) -> Result<(), PlatformError> {
    // Read redb file size and compare to available disk space.
    // Emit a warning metric if above threshold.
    // Platform-specific implementation (statvfs on Linux, GetDiskFreeSpaceEx on Windows).
    Ok(())
}
```

---

## 7. GC Metrics

```rust
// crates/storage/src/gc_metrics.rs
use prometheus::{IntCounter, IntGauge, Opts, Registry};

pub struct GcMetrics {
    /// Total artifacts deleted by GC across all runs.
    pub artifacts_deleted_total: IntCounter,

    /// Total metric buckets deleted by GC across all runs.
    pub metric_buckets_deleted_total: IntCounter,

    /// Total undeployed apps purged by GC.
    pub apps_purged_total: IntCounter,

    /// Current redb file size in bytes.
    pub redb_file_size_bytes: IntGauge,

    /// Disk usage percentage (redb size / available disk).
    pub disk_usage_ratio: prometheus::Gauge,
}

impl GcMetrics {
    pub fn new(registry: &Registry) -> Self {
        let artifacts_deleted_total = IntCounter::with_opts(
            Opts::new("gc_artifacts_deleted_total", "Total artifact versions deleted by GC"),
        ).unwrap();
        registry.register(Box::new(artifacts_deleted_total.clone())).unwrap();

        let metric_buckets_deleted_total = IntCounter::with_opts(
            Opts::new("gc_metric_buckets_deleted_total", "Total metric buckets deleted by GC"),
        ).unwrap();
        registry.register(Box::new(metric_buckets_deleted_total.clone())).unwrap();

        let apps_purged_total = IntCounter::with_opts(
            Opts::new("gc_apps_purged_total", "Total undeployed apps purged by GC"),
        ).unwrap();
        registry.register(Box::new(apps_purged_total.clone())).unwrap();

        let redb_file_size_bytes = IntGauge::with_opts(
            Opts::new("redb_file_size_bytes", "Size of the redb database file"),
        ).unwrap();
        registry.register(Box::new(redb_file_size_bytes.clone())).unwrap();

        let disk_usage_ratio = prometheus::Gauge::with_opts(
            Opts::new("node_disk_usage_ratio", "Ratio of redb file size to available disk space"),
        ).unwrap();
        registry.register(Box::new(disk_usage_ratio.clone())).unwrap();

        GcMetrics {
            artifacts_deleted_total,
            metric_buckets_deleted_total,
            apps_purged_total,
            redb_file_size_bytes,
            disk_usage_ratio,
        }
    }
}
```

---

## 8. CLI Commands

```
# View current GC configuration
wasm-ctl gc config
# Output:
# artifact_keep_versions: 3
# metrics_retain_days: 7
# undeploy_grace_secs: 3600
# gc_interval_secs: 600
# disk_warning_threshold: 80%

# Override GC config for a specific node
wasm-ctl gc config set --artifact-keep 5 --metrics-retain 14

# Force an immediate GC run (does not wait for the next tick)
wasm-ctl gc run --node node-0
# Output:
# GC complete: 12 artifacts deleted, 4320 metric buckets pruned, 1 app purged

# View disk usage
wasm-ctl gc disk --node node-0
# Output:
# redb file size: 245 MB
# available disk: 50 GB
# usage ratio: 0.49%
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Artifact GC
- [ ] Deploying v4 of an app with `keep=3` automatically deletes v1's artifact, config, and raw Wasm
- [ ] v2, v3, v4 remain in redb (the 3 most recent)
- [ ] Rollback to v3 or v2 still works after GC has run
- [ ] GC does not touch versions that are currently serving traffic (active instances)

### Metrics GC
- [ ] Metric buckets older than `metrics_retain_days` are deleted during GC
- [ ] Metric buckets within the retention window are untouched
- [ ] The `/metrics` Prometheus endpoint continues to work during GC (no lock contention)

### Undeploy Cleanup
- [ ] `wasm-ctl undeploy --app api-users` marks the app as undeployed
- [ ] Before the grace period, `wasm-ctl deploy --app api-users` can re-deploy (state still exists)
- [ ] After the grace period, all artifacts, configs, raw Wasm, metrics, and routes are deleted
- [ ] Secrets are NOT deleted automatically (operator must use `wasm-ctl secret delete`)

### Disk Monitoring
- [ ] `redb_file_size_bytes` Prometheus metric is updated every GC tick
- [ ] `node_disk_usage_ratio` exceeding 0.80 triggers a warning log
- [ ] `wasm-ctl gc disk` shows current disk usage for each node

### Non-Interference
- [ ] GC runs on a background Tokio task and does not block the deploy or request paths
- [ ] GC transactions use short-lived write locks (one transaction per deletion batch)
- [ ] Under sustained traffic (1000 req/s), GC does not cause latency spikes > 1ms
