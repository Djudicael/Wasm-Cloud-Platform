use crate::validation::validate_hot_config;
use common::config::{
    EbpfSection, GcSection, HealthSection, LoggingSection, NodeConfig, RateLimitSection,
};
use common::error::PlatformError;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock as StdRwLock};
use storage::Store;

const HOT_CONFIG_KEY: &str = "hot_config_overrides";

/// Configuration fields that can be changed at runtime without restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotConfig {
    pub rate_limit: RateLimitSection,
    pub ebpf: EbpfSection,
    pub gc: GcSection,
    pub health: HealthSection,
    pub logging: LoggingSection,
}

impl HotConfig {
    /// Create a hot-only snapshot from the cold boot config.
    pub fn from_cold_config(cold: &NodeConfig) -> Self {
        HotConfig {
            rate_limit: cold.rate_limit.clone(),
            ebpf: cold.ebpf.clone(),
            gc: cold.gc.clone(),
            health: cold.health.clone(),
            logging: cold.logging.clone(),
        }
    }
}

/// Handle for accessing and updating hot-reloadable configuration.
pub struct HotConfigHandle {
    inner: Arc<StdRwLock<HotConfig>>,
    cold_config: HotConfig,
    store: Store,
    #[allow(dead_code)]
    node_id: String,
}

impl Clone for HotConfigHandle {
    fn clone(&self) -> Self {
        HotConfigHandle {
            inner: self.inner.clone(),
            cold_config: self.cold_config.clone(),
            store: self.store.clone(),
            node_id: self.node_id.clone(),
        }
    }
}

impl HotConfigHandle {
    /// Create a new handle, loading any persisted overrides from storage.
    pub fn new(
        cold_config: &NodeConfig,
        store: Store,
        node_id: String,
    ) -> Result<Self, PlatformError> {
        let cold = HotConfig::from_cold_config(cold_config);
        let mut hot_config = cold.clone();

        if let Ok(Some(json)) = store.load_meta(HOT_CONFIG_KEY) {
            let overrides: HotConfig =
                serde_json::from_str(&json).map_err(|e| PlatformError::Storage {
                    message: e.to_string(),
                    source: None,
                })?;
            hot_config = merge_hot_config(hot_config, overrides);
            tracing::info!("loaded hot config overrides from storage");
        }

        Ok(HotConfigHandle {
            inner: Arc::new(StdRwLock::new(hot_config)),
            cold_config: cold,
            store,
            node_id,
        })
    }

    pub async fn read(&self) -> HotConfig {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.read().unwrap().clone())
            .await
            .unwrap()
    }

    /// Apply and persist a partial hot-config update.
    pub async fn apply_update(&self, update: HotConfigUpdate) -> Result<(), PlatformError> {
        let inner = self.inner.clone();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut hot = inner.write().unwrap();
            let original = hot.clone();
            let updated = merge_hot_config_update(&original, update);
            validate_hot_config(&updated)?;
            let json = serde_json::to_string(&updated).map_err(|e| PlatformError::Storage {
                message: e.to_string(),
                source: None,
            })?;
            store
                .save_meta(HOT_CONFIG_KEY, &json)
                .map_err(|e| PlatformError::Storage {
                    message: e.to_string(),
                    source: None,
                })?;
            *hot = updated;
            Ok::<(), PlatformError>(())
        })
        .await
        .unwrap()?;
        tracing::info!("hot configuration updated");
        Ok(())
    }

    /// Reset hot configuration to the cold baseline and clear persisted overrides.
    pub async fn reset(&self) -> Result<(), PlatformError> {
        let inner = self.inner.clone();
        let cold = self.cold_config.clone();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut hot = inner.write().unwrap();
            store
                .delete_meta(HOT_CONFIG_KEY)
                .map_err(|e| PlatformError::Storage {
                    message: e.to_string(),
                    source: None,
                })?;
            *hot = cold;
            tracing::info!("hot config reset to cold defaults");
            Ok::<(), PlatformError>(())
        })
        .await
        .unwrap()?;
        Ok(())
    }
}

/// Partial update for hot-reloadable configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HotConfigUpdate {
    pub rate_limit_default_rps: Option<u32>,
    pub rate_limit_default_burst: Option<u32>,
    pub rate_limit_default_per_ip: Option<u32>,
    pub ebpf_fd_soft_limit: Option<u32>,
    pub ebpf_fd_hard_limit: Option<u32>,
    pub ebpf_mem_low_threshold_pages: Option<u64>,
    pub ebpf_mem_critical_threshold_pages: Option<u64>,
    pub ebpf_disk_slow_threshold_ns: Option<u64>,
    pub ebpf_tcp_conn_limit_per_pid: Option<u32>,
    pub ebpf_syscall_rate_limit: Option<u64>,
    pub gc_interval_secs: Option<u64>,
    pub gc_disk_warning_threshold: Option<f64>,
    pub health_check_interval_secs: Option<u64>,
    pub health_default_idle_timeout_secs: Option<u64>,
    pub logging_level: Option<String>,
}

impl HotConfigUpdate {
    pub fn count_changes(&self) -> usize {
        let mut count = 0;
        if self.rate_limit_default_rps.is_some() {
            count += 1;
        }
        if self.rate_limit_default_burst.is_some() {
            count += 1;
        }
        if self.rate_limit_default_per_ip.is_some() {
            count += 1;
        }
        if self.ebpf_fd_soft_limit.is_some() {
            count += 1;
        }
        if self.ebpf_fd_hard_limit.is_some() {
            count += 1;
        }
        if self.ebpf_mem_low_threshold_pages.is_some() {
            count += 1;
        }
        if self.ebpf_mem_critical_threshold_pages.is_some() {
            count += 1;
        }
        if self.ebpf_disk_slow_threshold_ns.is_some() {
            count += 1;
        }
        if self.ebpf_tcp_conn_limit_per_pid.is_some() {
            count += 1;
        }
        if self.ebpf_syscall_rate_limit.is_some() {
            count += 1;
        }
        if self.gc_interval_secs.is_some() {
            count += 1;
        }
        if self.gc_disk_warning_threshold.is_some() {
            count += 1;
        }
        if self.health_check_interval_secs.is_some() {
            count += 1;
        }
        if self.health_default_idle_timeout_secs.is_some() {
            count += 1;
        }
        if self.logging_level.is_some() {
            count += 1;
        }
        count
    }
}

pub(crate) fn merge_hot_config_update(base: &HotConfig, update: HotConfigUpdate) -> HotConfig {
    let mut new = base.clone();
    if let Some(rps) = update.rate_limit_default_rps {
        new.rate_limit.default_requests_per_second = rps;
    }
    if let Some(burst) = update.rate_limit_default_burst {
        new.rate_limit.default_burst_capacity = burst;
    }
    if let Some(per_ip) = update.rate_limit_default_per_ip {
        new.rate_limit.default_per_ip_limit = per_ip;
    }
    if let Some(soft) = update.ebpf_fd_soft_limit {
        new.ebpf.fd_soft_limit = soft;
    }
    if let Some(hard) = update.ebpf_fd_hard_limit {
        new.ebpf.fd_hard_limit = hard;
    }
    if let Some(low) = update.ebpf_mem_low_threshold_pages {
        new.ebpf.mem_low_threshold_pages = low;
    }
    if let Some(critical) = update.ebpf_mem_critical_threshold_pages {
        new.ebpf.mem_critical_threshold_pages = critical;
    }
    if let Some(slow) = update.ebpf_disk_slow_threshold_ns {
        new.ebpf.disk_slow_threshold_ns = slow;
    }
    if let Some(tcp_limit) = update.ebpf_tcp_conn_limit_per_pid {
        new.ebpf.tcp_conn_limit_per_pid = tcp_limit;
    }
    if let Some(syscall_rate) = update.ebpf_syscall_rate_limit {
        new.ebpf.syscall_rate_limit = syscall_rate;
    }
    if let Some(interval) = update.gc_interval_secs {
        new.gc.gc_interval_secs = interval;
    }
    if let Some(threshold) = update.gc_disk_warning_threshold {
        new.gc.disk_warning_threshold = threshold;
    }
    if let Some(check_interval) = update.health_check_interval_secs {
        new.health.check_interval_secs = check_interval;
    }
    if let Some(idle_timeout) = update.health_default_idle_timeout_secs {
        new.health.default_idle_timeout_secs = idle_timeout;
    }
    if let Some(level) = update.logging_level {
        new.logging.level = level;
    }
    new
}

/// Persisted hot config is stored as a full snapshot, so overlay replacement is enough.
fn merge_hot_config(_base: HotConfig, overlay: HotConfig) -> HotConfig {
    overlay
}
