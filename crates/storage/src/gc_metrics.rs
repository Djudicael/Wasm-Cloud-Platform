// crates/storage/src/gc_metrics.rs
use prometheus::{Gauge, IntCounter, IntGauge, Opts, Registry};

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
    pub disk_usage_ratio: Gauge,
}

impl GcMetrics {
    pub fn new(registry: &Registry) -> Self {
        let artifacts_deleted_total = IntCounter::with_opts(Opts::new(
            "gc_artifacts_deleted_total",
            "Total artifact versions deleted by GC",
        ))
        .unwrap();
        registry
            .register(Box::new(artifacts_deleted_total.clone()))
            .unwrap();

        let metric_buckets_deleted_total = IntCounter::with_opts(Opts::new(
            "gc_metric_buckets_deleted_total",
            "Total metric buckets deleted by GC",
        ))
        .unwrap();
        registry
            .register(Box::new(metric_buckets_deleted_total.clone()))
            .unwrap();

        let apps_purged_total = IntCounter::with_opts(Opts::new(
            "gc_apps_purged_total",
            "Total undeployed apps purged by GC",
        ))
        .unwrap();
        registry
            .register(Box::new(apps_purged_total.clone()))
            .unwrap();

        let redb_file_size_bytes = IntGauge::with_opts(Opts::new(
            "redb_file_size_bytes",
            "Size of the redb database file",
        ))
        .unwrap();
        registry
            .register(Box::new(redb_file_size_bytes.clone()))
            .unwrap();

        let disk_usage_ratio = Gauge::with_opts(Opts::new(
            "node_disk_usage_ratio",
            "Ratio of redb file size to available disk space",
        ))
        .unwrap();
        registry
            .register(Box::new(disk_usage_ratio.clone()))
            .unwrap();

        GcMetrics {
            artifacts_deleted_total,
            metric_buckets_deleted_total,
            apps_purged_total,
            redb_file_size_bytes,
            disk_usage_ratio,
        }
    }

    pub fn record_artifacts_deleted(&self, count: u64) {
        self.artifacts_deleted_total.inc_by(count);
    }

    pub fn record_metric_buckets_deleted(&self, count: u64) {
        self.metric_buckets_deleted_total.inc_by(count);
    }

    pub fn record_apps_purged(&self, count: u64) {
        self.apps_purged_total.inc_by(count);
    }

    pub fn set_file_size(&self, bytes: u64) {
        self.redb_file_size_bytes.set(bytes as i64);
    }

    pub fn set_disk_usage_ratio(&self, ratio: f64) {
        self.disk_usage_ratio.set(ratio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_metrics_creation() {
        let registry = Registry::new();
        let metrics = GcMetrics::new(&registry);

        // Record some events
        metrics.record_artifacts_deleted(5);
        metrics.record_metric_buckets_deleted(100);
        metrics.record_apps_purged(1);
        metrics.set_file_size(1024 * 1024 * 100); // 100 MB
        metrics.set_disk_usage_ratio(0.45);

        // Verify metrics were recorded
        let metric_families = registry.gather();
        assert!(metric_families.len() >= 5);
    }
}
