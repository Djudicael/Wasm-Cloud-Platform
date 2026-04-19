//! Configuration management for the Wasm Cloud Platform.
//!
//! This crate provides:
//! - Configuration loading with merge priority (defaults < TOML < env < CLI)
//! - Hot-reloadable configuration with persistence in redb
//! - Validation and environment variable support

use common::config::{
    AdminSection, BillingSection, DatabaseSection, DnsSection, EbpfSection, GcSection,
    HealthSection, LoggingSection, NatsSection, NodeConfig, NodeSection, ProxySection,
    RateLimitSection, RuntimeSection, StorageSection,
};
use common::error::PlatformError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};
use storage::Store;

// -----------------------------------------------------------------------------
// Configuration Loader
// -----------------------------------------------------------------------------

/// CLI overrides extracted from clap args.
/// Only non-default values are set — None means "use the lower-priority value".
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub node_id: Option<String>,
    pub db_path: Option<String>,
    pub nats_url: Option<String>,
    pub nats_creds: Option<String>,
    pub http_port: Option<u16>,
    pub https_port: Option<u16>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub admin_port: Option<u16>,
    pub artifact_port: Option<u16>,
    pub port_start: Option<u16>,
    pub port_end: Option<u16>,
    pub key_source: Option<String>,
    pub key_file: Option<String>,
    pub database_url: Option<String>,
    pub pgbouncer_addr: Option<String>,
    pub enable_db_proxy: Option<bool>,
    pub db_proxy_addr: Option<String>,
    pub db_proxy_backend: Option<String>,
    pub db_proxy_max_connections: Option<usize>,
    pub log_level: Option<String>,
    pub otlp_endpoint: Option<String>,
    pub billing_export_dir: Option<String>,
    pub billing_export_interval_secs: Option<u64>,
    pub platform_domain: Option<String>,
    pub dns_webhook_url: Option<String>,
    pub dns_webhook_token: Option<String>,
    pub auth_token: Option<String>,
}

/// Load configuration with the merge priority:
///   1. Built-in defaults (struct Default impl)
///   2. TOML file (if provided)
///   3. Environment variables (WASM_NODE_ prefix)
///   4. CLI flags (highest priority)
pub fn load_config(
    config_path: Option<&Path>,
    cli_overrides: &CliOverrides,
) -> Result<NodeConfig, PlatformError> {
    // 1. Start with defaults
    let mut config = NodeConfig::default();

    // 2. Layer TOML file on top
    if let Some(path) = config_path {
        if path.exists() {
            let toml_str = std::fs::read_to_string(path).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to read config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            let file_config: NodeConfig = toml::from_str(&toml_str).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to parse config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            config = merge_config(config, file_config);
            tracing::info!(path = %path.display(), "configuration file loaded");
        } else {
            tracing::warn!(path = %path.display(), "config file not found, using defaults");
        }
    }

    // 3. Layer environment variables on top
    config = apply_env_overrides(config);

    // 4. Layer CLI overrides on top (highest priority)
    config = apply_cli_overrides(config, cli_overrides);

    // 5. Validate the final configuration
    validate_config(&config)?;

    Ok(config)
}

/// Merge two configs: `overlay` values take precedence over `base` values.
/// For Option fields: overlay.Some overrides base.Some; overlay.None keeps base.
/// For primitive fields: overlay's value always wins (even if it's the default).
/// This is why we only apply CLI overrides for explicitly-set flags.
fn merge_config(base: NodeConfig, overlay: NodeConfig) -> NodeConfig {
    NodeConfig {
        node: NodeSection {
            node_id: overlay.node.node_id,
        },
        storage: StorageSection {
            db_path: overlay.storage.db_path,
        },
        nats: NatsSection {
            url: overlay.nats.url,
            creds_file: overlay.nats.creds_file.or(base.nats.creds_file),
        },
        proxy: ProxySection {
            http_port: overlay.proxy.http_port,
            https_port: overlay.proxy.https_port,
            tls_cert: overlay.proxy.tls_cert.or(base.proxy.tls_cert),
            tls_key: overlay.proxy.tls_key.or(base.proxy.tls_key),
        },
        admin: AdminSection {
            port: overlay.admin.port,
            artifact_port: overlay.admin.artifact_port,
            auth_token: overlay.admin.auth_token.or(base.admin.auth_token),
        },
        runtime: RuntimeSection {
            port_start: overlay.runtime.port_start,
            port_end: overlay.runtime.port_end,
            key_source: overlay.runtime.key_source,
            key_file: overlay.runtime.key_file.or(base.runtime.key_file),
        },
        database: DatabaseSection {
            default_url: overlay.database.default_url,
            pgbouncer_addr: overlay.database.pgbouncer_addr,
            enable_db_proxy: overlay.database.enable_db_proxy,
            db_proxy_addr: overlay.database.db_proxy_addr,
            db_proxy_backend: overlay.database.db_proxy_backend,
            db_proxy_max_connections: overlay.database.db_proxy_max_connections,
        },
        logging: LoggingSection {
            level: overlay.logging.level,
            otlp_endpoint: overlay.logging.otlp_endpoint.or(base.logging.otlp_endpoint),
        },
        billing: BillingSection {
            export_dir: overlay.billing.export_dir.or(base.billing.export_dir),
            export_interval_secs: overlay.billing.export_interval_secs,
        },
        gc: GcSection {
            artifact_keep_versions: overlay.gc.artifact_keep_versions,
            metrics_retain_days: overlay.gc.metrics_retain_days,
            undeploy_grace_secs: overlay.gc.undeploy_grace_secs,
            gc_interval_secs: overlay.gc.gc_interval_secs,
            disk_warning_threshold: overlay.gc.disk_warning_threshold,
        },
        rate_limit: RateLimitSection {
            default_requests_per_second: overlay.rate_limit.default_requests_per_second,
            default_burst_capacity: overlay.rate_limit.default_burst_capacity,
            default_per_ip_limit: overlay.rate_limit.default_per_ip_limit,
        },
        ebpf: EbpfSection {
            enabled: overlay.ebpf.enabled,
            fd_soft_limit: overlay.ebpf.fd_soft_limit,
            fd_hard_limit: overlay.ebpf.fd_hard_limit,
            mem_low_threshold_pages: overlay.ebpf.mem_low_threshold_pages,
            mem_critical_threshold_pages: overlay.ebpf.mem_critical_threshold_pages,
            disk_slow_threshold_ns: overlay.ebpf.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: overlay.ebpf.tcp_conn_limit_per_pid,
            syscall_rate_limit: overlay.ebpf.syscall_rate_limit,
            sampling_period_secs: overlay.ebpf.sampling_period_secs,
        },
        dns: DnsSection {
            platform_domain: overlay.dns.platform_domain.or(base.dns.platform_domain),
            webhook_url: overlay.dns.webhook_url.or(base.dns.webhook_url),
            webhook_token: overlay.dns.webhook_token.or(base.dns.webhook_token),
        },
        health: HealthSection {
            check_interval_secs: overlay.health.check_interval_secs,
            default_idle_timeout_secs: overlay.health.default_idle_timeout_secs,
            default_max_instances: overlay.health.default_max_instances,
            default_fuel_quota: overlay.health.default_fuel_quota,
            default_memory_pages: overlay.health.default_memory_pages,
        },
    }
}

/// Apply environment variable overrides.
/// Convention: WASM_NODE_<SECTION>_<KEY> (uppercase, underscores for dots)
/// Examples:
///   WASM_NODE_NODE_ID=node-1
///   WASM_NODE_NATS_URL=nats://nats.prod:4222
///   WASM_NODE_LOGGING_LEVEL=debug
///   WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND=5000
fn apply_env_overrides(mut config: NodeConfig) -> NodeConfig {
    // Node
    if let Ok(v) = std::env::var("WASM_NODE_NODE_ID") {
        config.node.node_id = v;
    }
    // Storage
    if let Ok(v) = std::env::var("WASM_NODE_STORAGE_DB_PATH") {
        config.storage.db_path = PathBuf::from(v);
    }
    // NATS
    if let Ok(v) = std::env::var("WASM_NODE_NATS_URL") {
        config.nats.url = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CREDS_FILE") {
        config.nats.creds_file = Some(v);
    }
    // Proxy
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTP_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.http_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTPS_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.https_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_CERT") {
        config.proxy.tls_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_KEY") {
        config.proxy.tls_key = Some(v);
    }
    // Admin
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_PORT") {
        if let Ok(port) = v.parse() {
            config.admin.port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_AUTH_TOKEN") {
        config.admin.auth_token = Some(v);
    }
    // Logging
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_OTLP_ENDPOINT") {
        config.logging.otlp_endpoint = Some(v);
    }
    // Rate limit
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND") {
        if let Ok(rps) = v.parse() {
            config.rate_limit.default_requests_per_second = rps;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_BURST_CAPACITY") {
        if let Ok(burst) = v.parse() {
            config.rate_limit.default_burst_capacity = burst;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_PER_IP_LIMIT") {
        if let Ok(limit) = v.parse() {
            config.rate_limit.default_per_ip_limit = limit;
        }
    }
    // eBPF
    if let Ok(v) = std::env::var("WASM_NODE_EBPF_ENABLED") {
        if let Ok(enabled) = v.parse() {
            config.ebpf.enabled = enabled;
        }
    }
    config
}

/// Apply CLI flag overrides. Only non-None values override.
fn apply_cli_overrides(mut config: NodeConfig, cli: &CliOverrides) -> NodeConfig {
    if let Some(v) = &cli.node_id {
        config.node.node_id = v.clone();
    }
    if let Some(v) = &cli.db_path {
        config.storage.db_path = PathBuf::from(v);
    }
    if let Some(v) = &cli.nats_url {
        config.nats.url = v.clone();
    }
    if let Some(v) = &cli.nats_creds {
        config.nats.creds_file = Some(v.clone());
    }
    if let Some(v) = cli.http_port {
        config.proxy.http_port = v;
    }
    if let Some(v) = cli.https_port {
        config.proxy.https_port = v;
    }
    if let Some(v) = &cli.tls_cert {
        config.proxy.tls_cert = Some(v.clone());
    }
    if let Some(v) = &cli.tls_key {
        config.proxy.tls_key = Some(v.clone());
    }
    if let Some(v) = cli.admin_port {
        config.admin.port = v;
    }
    if let Some(v) = cli.artifact_port {
        config.admin.artifact_port = v;
    }
    if let Some(v) = cli.port_start {
        config.runtime.port_start = v;
    }
    if let Some(v) = cli.port_end {
        config.runtime.port_end = v;
    }
    if let Some(v) = &cli.key_source {
        config.runtime.key_source = v.clone();
    }
    if let Some(v) = &cli.key_file {
        config.runtime.key_file = Some(v.clone());
    }
    if let Some(v) = &cli.database_url {
        config.database.default_url = v.clone();
    }
    if let Some(v) = &cli.pgbouncer_addr {
        config.database.pgbouncer_addr = v.clone();
    }
    if let Some(v) = cli.enable_db_proxy {
        config.database.enable_db_proxy = v;
    }
    if let Some(v) = &cli.db_proxy_addr {
        config.database.db_proxy_addr = v.clone();
    }
    if let Some(v) = &cli.db_proxy_backend {
        config.database.db_proxy_backend = v.clone();
    }
    if let Some(v) = cli.db_proxy_max_connections {
        config.database.db_proxy_max_connections = v;
    }
    if let Some(v) = &cli.log_level {
        config.logging.level = v.clone();
    }
    if let Some(v) = &cli.otlp_endpoint {
        config.logging.otlp_endpoint = Some(v.clone());
    }
    if let Some(v) = &cli.billing_export_dir {
        config.billing.export_dir = Some(v.clone());
    }
    if let Some(v) = cli.billing_export_interval_secs {
        config.billing.export_interval_secs = v;
    }
    if let Some(v) = &cli.platform_domain {
        config.dns.platform_domain = Some(v.clone());
    }
    if let Some(v) = &cli.dns_webhook_url {
        config.dns.webhook_url = Some(v.clone());
    }
    if let Some(v) = &cli.dns_webhook_token {
        config.dns.webhook_token = Some(v.clone());
    }
    if let Some(v) = &cli.auth_token {
        config.admin.auth_token = Some(v.clone());
    }
    config
}

/// Validate the final merged configuration.
fn validate_config(config: &NodeConfig) -> Result<(), PlatformError> {
    let mut errors = Vec::new();

    // Port range
    if config.runtime.port_start >= config.runtime.port_end {
        errors.push("port_start must be less than port_end".to_string());
    }
    if config.runtime.port_end - config.runtime.port_start < 100 {
        errors.push("port range must span at least 100 ports".to_string());
    }

    // Log level
    let valid_levels = ["trace", "debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        errors.push(format!(
            "invalid log level '{}', must be one of: {}",
            config.logging.level,
            valid_levels.join(", ")
        ));
    }

    // GC thresholds
    if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
        errors.push("disk_warning_threshold must be between 0.0 and 1.0".to_string());
    }
    if config.gc.artifact_keep_versions == 0 {
        errors.push("artifact_keep_versions must be > 0".to_string());
    }

    // eBPF
    if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
        errors.push("fd_soft_limit must be less than fd_hard_limit".to_string());
    }
    if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
        errors.push(
            "mem_low_threshold_pages must be greater than mem_critical_threshold_pages".to_string(),
        );
    }

    // Health
    if config.health.check_interval_secs == 0 {
        errors.push("check_interval_secs must be > 0".to_string());
    }
    if config.health.default_fuel_quota == 0 {
        errors.push("default_fuel_quota must be > 0".to_string());
    }
    if config.health.default_memory_pages == 0 {
        errors.push("default_memory_pages must be > 0".to_string());
    }

    // Rate limits
    if config.rate_limit.default_requests_per_second == 0 {
        errors.push("default_requests_per_second must be > 0".to_string());
    }

    // TLS consistency
    if config.proxy.tls_cert.is_some() != config.proxy.tls_key.is_some() {
        errors.push("tls_cert and tls_key must both be set or both be unset".to_string());
    }

    // HTTPS port with no TLS
    if config.proxy.https_port > 0 && config.proxy.tls_cert.is_none() {
        errors.push("https_port requires tls_cert and tls_key".to_string());
    }

    if !errors.is_empty() {
        return Err(PlatformError::ConfigValidation(format!(
            "configuration validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Hot-Reloadable Configuration
// -----------------------------------------------------------------------------

/// Configuration fields that can be changed at runtime without restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotConfig {
    /// Rate limiting defaults.
    pub rate_limit: RateLimitSection,
    /// eBPF monitoring thresholds.
    pub ebpf: EbpfSection,
    /// Garbage collection intervals and thresholds.
    pub gc: GcSection,
    /// Health check intervals and instance defaults.
    pub health: HealthSection,
    /// Logging level (can be changed at runtime).
    pub logging: LoggingSection,
}

impl HotConfig {
    /// Create a HotConfig from the cold (full) NodeConfig.
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
    /// Cold (file/env/cli) config — used as the baseline for reset.
    cold_config: HotConfig,
    store: Store,
    #[allow(dead_code)]
    node_id: String,
}

impl HotConfigHandle {
    /// Create a new handle, loading any persisted overrides from redb.
    pub fn new(
        cold_config: &NodeConfig,
        store: Store,
        node_id: String,
    ) -> Result<Self, PlatformError> {
        let cold = HotConfig::from_cold_config(cold_config);
        let mut hot_config = cold.clone();

        // Load persisted overrides
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

    /// Read the current hot configuration.
    pub async fn read(&self) -> HotConfig {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.read().unwrap().clone())
            .await
            .unwrap()
    }

    /// Apply a partial update to the hot configuration.
    /// Validates the update, persists it, and notifies components.
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

    /// Reset hot configuration to cold defaults (clear persisted overrides).
    ///
    /// This restores all hot-reloadable fields to their cold-config values
    /// (from the TOML file, environment variables, and CLI flags). Persisted
    /// overrides in redb are deleted so a subsequent restart also starts clean.
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
            // Reset in-memory config to the cold baseline
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
/// Only fields that are Some will be applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Count how many fields are being updated.
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

/// Merge a partial update into a HotConfig.
fn merge_hot_config_update(base: &HotConfig, update: HotConfigUpdate) -> HotConfig {
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

/// Merge two HotConfigs (for loading persisted overrides).
/// Merge two `HotConfig`s: `overlay` values take precedence over `base`.
///
/// For `Option` fields inside section structs we cannot tell whether a value
/// was explicitly set or is just the default, so we replace entire sections
/// from the overlay. This is correct because the persisted `HotConfig` is
/// always a complete snapshot (written by `apply_update`), not a partial
/// overlay.
fn merge_hot_config(_base: HotConfig, overlay: HotConfig) -> HotConfig {
    overlay
}

/// Validate a HotConfig (subset of full validation).
fn validate_hot_config(config: &HotConfig) -> Result<(), PlatformError> {
    let mut errors = Vec::new();

    // Log level
    let valid_levels = ["trace", "debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        errors.push(format!(
            "invalid log level '{}', must be one of: {}",
            config.logging.level,
            valid_levels.join(", ")
        ));
    }

    // GC thresholds
    if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
        errors.push("disk_warning_threshold must be between 0.0 and 1.0".to_string());
    }
    if config.gc.artifact_keep_versions == 0 {
        errors.push("artifact_keep_versions must be > 0".to_string());
    }

    // eBPF
    if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
        errors.push("fd_soft_limit must be less than fd_hard_limit".to_string());
    }
    if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
        errors.push(
            "mem_low_threshold_pages must be greater than mem_critical_threshold_pages".to_string(),
        );
    }

    // Health
    if config.health.check_interval_secs == 0 {
        errors.push("check_interval_secs must be > 0".to_string());
    }
    if config.health.default_fuel_quota == 0 {
        errors.push("default_fuel_quota must be > 0".to_string());
    }
    if config.health.default_memory_pages == 0 {
        errors.push("default_memory_pages must be > 0".to_string());
    }

    // Rate limits
    if config.rate_limit.default_requests_per_second == 0 {
        errors.push("default_requests_per_second must be > 0".to_string());
    }

    if !errors.is_empty() {
        return Err(PlatformError::ConfigValidation(format!(
            "hot configuration validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Persistence in redb
// -----------------------------------------------------------------------------

const HOT_CONFIG_KEY: &str = "hot_config_overrides";
