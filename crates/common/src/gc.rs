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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_gc_config() {
        let config = GcConfig::default();
        assert_eq!(config.artifact_keep_versions, 3);
        assert_eq!(config.metrics_retain_days, 7);
        assert_eq!(config.undeploy_grace_secs, 3600);
        assert_eq!(config.gc_interval_secs, 600);
        assert_eq!(config.disk_warning_threshold, 0.80);
    }

    #[test]
    fn test_gc_config_serialization() {
        let config = GcConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: GcConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            config.artifact_keep_versions,
            deserialized.artifact_keep_versions
        );
    }
}
