use serde::{Deserialize, Serialize};

/// Health-check and gateway-related configuration sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSection {
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    #[serde(default = "default_check_timeout")]
    pub check_timeout_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub default_idle_timeout_secs: u64,
    #[serde(default = "default_max_instances")]
    pub default_max_instances: usize,
    #[serde(default = "default_fuel_quota")]
    pub default_fuel_quota: u64,
    #[serde(default = "default_memory_pages")]
    pub default_memory_pages: u32,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default = "default_min_disk_free_bytes")]
    pub min_disk_free_bytes: u64,
    #[serde(default = "default_max_memory_bytes")]
    pub max_memory_bytes: u64,
    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval_secs: u64,
    #[serde(default = "default_cluster_node_stale_after_secs")]
    pub cluster_node_stale_after_secs: u64,
    #[serde(default)]
    pub app_defaults: AppHealthCheckDefaults,
}

fn default_check_interval() -> u64 {
    10
}

fn default_check_timeout() -> u64 {
    5
}

fn default_idle_timeout() -> u64 {
    300
}

fn default_max_instances() -> usize {
    10
}

fn default_fuel_quota() -> u64 {
    10_000_000
}

fn default_memory_pages() -> u32 {
    65536
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_success_threshold() -> u32 {
    2
}

fn default_min_disk_free_bytes() -> u64 {
    1024 * 1024 * 1024 // 1 GB
}

fn default_max_memory_bytes() -> u64 {
    4 * 1024 * 1024 * 1024 // 4 GB
}

fn default_snapshot_interval() -> u64 {
    60
}

fn default_cluster_node_stale_after_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppHealthCheckDefaults {
    #[serde(default = "default_app_health_path")]
    pub path: String,
    #[serde(default = "default_app_health_expected_status")]
    pub expected_status: u16,
    #[serde(default = "default_app_health_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_app_health_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_app_health_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_app_health_success_threshold")]
    pub success_threshold: u32,
}

fn default_app_health_path() -> String {
    "/health".to_string()
}

fn default_app_health_expected_status() -> u16 {
    200
}

fn default_app_health_interval_secs() -> u64 {
    10
}

fn default_app_health_timeout_secs() -> u64 {
    5
}

fn default_app_health_failure_threshold() -> u32 {
    3
}

fn default_app_health_success_threshold() -> u32 {
    2
}

impl Default for AppHealthCheckDefaults {
    fn default() -> Self {
        AppHealthCheckDefaults {
            path: default_app_health_path(),
            expected_status: default_app_health_expected_status(),
            interval_secs: default_app_health_interval_secs(),
            timeout_secs: default_app_health_timeout_secs(),
            failure_threshold: default_app_health_failure_threshold(),
            success_threshold: default_app_health_success_threshold(),
        }
    }
}

impl Default for HealthSection {
    fn default() -> Self {
        HealthSection {
            check_interval_secs: default_check_interval(),
            check_timeout_secs: default_check_timeout(),
            default_idle_timeout_secs: default_idle_timeout(),
            default_max_instances: default_max_instances(),
            default_fuel_quota: default_fuel_quota(),
            default_memory_pages: default_memory_pages(),
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            min_disk_free_bytes: default_min_disk_free_bytes(),
            max_memory_bytes: default_max_memory_bytes(),
            snapshot_interval_secs: default_snapshot_interval(),
            cluster_node_stale_after_secs: default_cluster_node_stale_after_secs(),
            app_defaults: AppHealthCheckDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewaySection {
    #[serde(default)]
    pub oidc: Option<crate::types::OidcConfig>,
    #[serde(default)]
    pub rate_limit: GatewayRateLimitSection,
    #[serde(default)]
    pub circuit_breaker: GatewayCircuitBreakerSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRateLimitSection {
    #[serde(default = "default_rl_kv_bucket")]
    pub kv_bucket: String,
    #[serde(default = "default_rl_sync_interval_ms")]
    pub sync_interval_ms: u64,
}

fn default_rl_kv_bucket() -> String {
    "rate_limits".to_string()
}

fn default_rl_sync_interval_ms() -> u64 {
    100
}

impl Default for GatewayRateLimitSection {
    fn default() -> Self {
        GatewayRateLimitSection {
            kv_bucket: default_rl_kv_bucket(),
            sync_interval_ms: default_rl_sync_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayCircuitBreakerSection {
    #[serde(default = "default_cb_failure_threshold")]
    pub default_failure_threshold: u32,
    #[serde(default = "default_cb_reset_timeout_secs")]
    pub default_reset_timeout_secs: u32,
}

fn default_cb_failure_threshold() -> u32 {
    5
}

fn default_cb_reset_timeout_secs() -> u32 {
    30
}

impl Default for GatewayCircuitBreakerSection {
    fn default() -> Self {
        GatewayCircuitBreakerSection {
            default_failure_threshold: default_cb_failure_threshold(),
            default_reset_timeout_secs: default_cb_reset_timeout_secs(),
        }
    }
}
