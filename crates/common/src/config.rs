//! Configuration structures for the Wasm Cloud Platform.
//! All fields have defaults — a completely empty TOML file is valid.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::types::default_true;

/// Top-level configuration for a wasm-node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    #[serde(default)]
    pub node: NodeSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub nats: NatsSection,
    #[serde(default)]
    pub proxy: ProxySection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub runtime: RuntimeSection,
    #[serde(default)]
    pub database: DatabaseSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub billing: BillingSection,
    #[serde(default)]
    pub gc: GcSection,
    #[serde(default)]
    pub rate_limit: RateLimitSection,
    #[serde(default)]
    pub ebpf: EbpfSection,
    #[serde(default)]
    pub dns: DnsSection,
    #[serde(default)]
    pub health: HealthSection,
    #[serde(default)]
    pub gateway: GatewaySection,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            node: NodeSection::default(),
            storage: StorageSection::default(),
            nats: NatsSection::default(),
            proxy: ProxySection::default(),
            admin: AdminSection::default(),
            auth: AuthSection::default(),
            runtime: RuntimeSection::default(),
            database: DatabaseSection::default(),
            logging: LoggingSection::default(),
            billing: BillingSection::default(),
            gc: GcSection::default(),
            rate_limit: RateLimitSection::default(),
            ebpf: EbpfSection::default(),
            dns: DnsSection::default(),
            health: HealthSection::default(),
            gateway: GatewaySection::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSection {
    #[serde(default = "default_node_id")]
    pub node_id: String,
}

fn default_node_id() -> String {
    std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
}

impl Default for NodeSection {
    fn default() -> Self {
        NodeSection {
            node_id: default_node_id(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageOpenFailureMode {
    /// Preserve the unreadable DB by moving it aside, then fail startup.
    QuarantineAndFail,
    /// Preserve the unreadable DB by moving it aside, then create a fresh DB.
    QuarantineAndRecreate,
}

impl Default for StorageOpenFailureMode {
    fn default() -> Self {
        StorageOpenFailureMode::QuarantineAndFail
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StorageIntegrityFailureMode {
    /// Preserve the corrupted DB by moving it aside, then exit and require an operator restart.
    QuarantineAndExit,
    /// Delete the corrupted DB on exit. This is destructive and should only be used
    /// when the operator explicitly opts into disposable local state.
    DeleteAndExit,
}

impl Default for StorageIntegrityFailureMode {
    fn default() -> Self {
        StorageIntegrityFailureMode::QuarantineAndExit
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    #[serde(default)]
    pub open_failure_mode: StorageOpenFailureMode,
    #[serde(default)]
    pub integrity_failure_mode: StorageIntegrityFailureMode,
}

fn default_db_path() -> PathBuf {
    PathBuf::from("/tmp/wasm-node/state.redb")
}

impl Default for StorageSection {
    fn default() -> Self {
        StorageSection {
            db_path: default_db_path(),
            open_failure_mode: StorageOpenFailureMode::default(),
            integrity_failure_mode: StorageIntegrityFailureMode::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsSection {
    #[serde(default = "default_nats_url")]
    pub url: String,
    #[serde(default)]
    pub creds_file: Option<String>,
}

fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".to_string()
}

impl Default for NatsSection {
    fn default() -> Self {
        NatsSection {
            url: default_nats_url(),
            creds_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxySection {
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_key: Option<String>,
}

fn default_http_port() -> u16 {
    8080
}

fn default_https_port() -> u16 {
    0
}

impl Default for ProxySection {
    fn default() -> Self {
        ProxySection {
            http_port: default_http_port(),
            https_port: default_https_port(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSection {
    #[serde(default = "default_admin_port")]
    pub port: u16,
    #[serde(default = "default_artifact_port")]
    pub artifact_port: u16,
    /// Local bind host/address for the admin API listener.
    #[serde(default = "default_admin_bind_address")]
    pub bind_address: String,
    /// Local bind host/address for the artifact server listener.
    #[serde(default = "default_admin_bind_address")]
    pub artifact_bind_address: String,
    /// Optional dedicated TLS certificate path for the admin HTTPS listener.
    /// If unset, the node may fall back to the shared proxy TLS material.
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// Optional dedicated TLS private key path for the admin HTTPS listener.
    /// If unset, the node may fall back to the shared proxy TLS material.
    #[serde(default)]
    pub tls_key: Option<String>,
    /// Optional routable host name or IP used when advertising the artifact endpoint
    /// to peer nodes. The local listener may still bind to loopback.
    #[serde(default)]
    pub advertised_host: Option<String>,
    /// Optional fully-qualified artifact base URL advertised to peer nodes.
    /// Example: "https://node-1.internal:9443".
    /// When set, this takes precedence over `advertised_host`.
    #[serde(default)]
    pub advertised_artifact_url: Option<String>,
    /// Legacy single admin token (deprecated in favor of [auth] section).
    /// When set and [auth] is not configured, this token is used as the write token.
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_admin_port() -> u16 {
    9090
}

fn default_artifact_port() -> u16 {
    9091
}

fn default_admin_bind_address() -> String {
    "127.0.0.1".to_string()
}

impl Default for AdminSection {
    fn default() -> Self {
        AdminSection {
            port: default_admin_port(),
            artifact_port: default_artifact_port(),
            bind_address: default_admin_bind_address(),
            artifact_bind_address: default_admin_bind_address(),
            tls_cert: None,
            tls_key: None,
            advertised_host: None,
            advertised_artifact_url: None,
            auth_token: None,
        }
    }
}

/// Authentication configuration for the admin API.
///
/// This section provides bearer-token authentication with separate read/write
/// permission levels, rate limiting, and TLS enforcement.
///
/// When `enabled = false` (the default), all admin API endpoints are accessible
/// without authentication (backward compatible).
///
/// When `enabled = true`, requests must include an `Authorization: Bearer <token>`
/// header. The read token grants GET access only; the write token grants full access.
///
/// # TOML Example
///
/// ```toml
/// [auth]
/// enabled = true
/// read_token = "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456"
/// write_token = "f6e5d4c3b2a1098765432109876543210987fedcba0987654321fedcba098765"
/// require_tls = true
/// rate_limit_per_second = 10
/// rate_limit_burst = 20
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSection {
    /// Enable authentication on admin API endpoints.
    #[serde(default)]
    pub enabled: bool,

    /// Read-only bearer token. Grants access to GET endpoints.
    #[serde(default)]
    pub read_token: Option<String>,

    /// Read-write bearer token. Grants access to all endpoints.
    #[serde(default)]
    pub write_token: Option<String>,

    /// Require TLS for admin API when authentication is enabled.
    #[serde(default = "default_auth_require_tls")]
    pub require_tls: bool,

    /// Rate limit for admin API requests (requests per second per IP).
    /// Set to 0 to disable rate limiting.
    #[serde(default = "default_auth_rate_limit")]
    pub rate_limit_per_second: u32,

    /// Maximum burst for admin API rate limiting.
    #[serde(default = "default_auth_burst")]
    pub rate_limit_burst: u32,
}

fn default_auth_require_tls() -> bool {
    true
}
fn default_auth_rate_limit() -> u32 {
    10
}
fn default_auth_burst() -> u32 {
    20
}

impl Default for AuthSection {
    fn default() -> Self {
        AuthSection {
            enabled: false,
            read_token: None,
            write_token: None,
            require_tls: default_auth_require_tls(),
            rate_limit_per_second: default_auth_rate_limit(),
            rate_limit_burst: default_auth_burst(),
        }
    }
}

impl From<AuthSection> for crate::auth::AuthConfig {
    fn from(section: AuthSection) -> Self {
        crate::auth::AuthConfig {
            enabled: section.enabled,
            read_token: section.read_token,
            write_token: section.write_token,
            require_tls: section.require_tls,
            rate_limit_per_second: section.rate_limit_per_second,
            rate_limit_burst: section.rate_limit_burst,
        }
    }
}

impl From<crate::auth::AuthConfig> for AuthSection {
    fn from(config: crate::auth::AuthConfig) -> Self {
        AuthSection {
            enabled: config.enabled,
            read_token: config.read_token,
            write_token: config.write_token,
            require_tls: config.require_tls,
            rate_limit_per_second: config.rate_limit_per_second,
            rate_limit_burst: config.rate_limit_burst,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default = "default_port_start")]
    pub port_start: u16,
    #[serde(default = "default_port_end")]
    pub port_end: u16,
    #[serde(default = "default_key_source")]
    pub key_source: String,
    #[serde(default)]
    pub key_file: Option<String>,
    /// Optional directory for Wasmtime code cache artifacts.
    /// When set, the runtime enables Wasmtime's compilation cache.
    #[serde(default)]
    pub cache_directory: Option<String>,
    /// Optional Ed25519 public key (hex, 32 bytes) used to verify signed
    /// platform upgrade metadata before installing a new node binary.
    #[serde(default)]
    pub upgrade_signing_public_key: Option<String>,
    /// Enable Wasmtime pooling allocator for component instances.
    #[serde(default)]
    pub pooling_allocator: bool,
    /// Maximum number of concurrent component instances when pooling is enabled.
    #[serde(default = "default_pooling_total_component_instances")]
    pub pooling_total_component_instances: u32,
    /// Optional cap on core instances per component when pooling is enabled.
    #[serde(default)]
    pub pooling_max_core_instances_per_component: Option<u32>,
    /// Optional cap on memories per component when pooling is enabled.
    #[serde(default)]
    pub pooling_max_memories_per_component: Option<u32>,
    /// Optional cap on tables per component when pooling is enabled.
    #[serde(default)]
    pub pooling_max_tables_per_component: Option<u32>,
}

fn default_port_start() -> u16 {
    10000
}

fn default_port_end() -> u16 {
    19999
}

fn default_key_source() -> String {
    "generate".to_string()
}

fn default_pooling_total_component_instances() -> u32 {
    1000
}

impl Default for RuntimeSection {
    fn default() -> Self {
        RuntimeSection {
            port_start: default_port_start(),
            port_end: default_port_end(),
            key_source: default_key_source(),
            key_file: None,
            cache_directory: None,
            upgrade_signing_public_key: None,
            pooling_allocator: false,
            pooling_total_component_instances: default_pooling_total_component_instances(),
            pooling_max_core_instances_per_component: None,
            pooling_max_memories_per_component: None,
            pooling_max_tables_per_component: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSection {
    #[serde(default = "default_db_url")]
    pub default_url: String,
    #[serde(default = "default_pgbouncer_addr")]
    pub pgbouncer_addr: String,
    #[serde(default)]
    pub enable_db_proxy: bool,
    #[serde(default = "default_db_proxy_addr")]
    pub db_proxy_addr: String,
    #[serde(default = "default_db_proxy_backend")]
    pub db_proxy_backend: String,
    #[serde(default = "default_db_proxy_max_conn")]
    pub db_proxy_max_connections: usize,
}

fn default_db_url() -> String {
    "postgres://127.0.0.1:5432".to_string()
}

fn default_pgbouncer_addr() -> String {
    "127.0.0.1:5432".to_string()
}

fn default_db_proxy_addr() -> String {
    "127.0.0.1:5433".to_string()
}

fn default_db_proxy_backend() -> String {
    "db.internal:5432".to_string()
}

fn default_db_proxy_max_conn() -> usize {
    20
}

impl Default for DatabaseSection {
    fn default() -> Self {
        DatabaseSection {
            default_url: default_db_url(),
            pgbouncer_addr: default_pgbouncer_addr(),
            enable_db_proxy: false,
            db_proxy_addr: default_db_proxy_addr(),
            db_proxy_backend: default_db_proxy_backend(),
            db_proxy_max_connections: default_db_proxy_max_conn(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSection {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Per-module log level overrides.
    #[serde(default)]
    pub modules: std::collections::HashMap<String, String>,
    /// Log sampling configuration.
    #[serde(default)]
    pub sampling: LogSamplingSection,
    /// Log file rotation (only when output is a file path).
    #[serde(default)]
    pub rotation: LogRotationSection,
    /// Log forwarding to external aggregators.
    #[serde(default)]
    pub forward: LogForwardSection,
    /// Audit logging configuration.
    #[serde(default)]
    pub audit: LogAuditSection,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "json".to_string()
}

impl Default for LoggingSection {
    fn default() -> Self {
        LoggingSection {
            level: default_log_level(),
            format: default_log_format(),
            output: None,
            otlp_endpoint: None,
            modules: std::collections::HashMap::new(),
            sampling: LogSamplingSection::default(),
            rotation: LogRotationSection::default(),
            forward: LogForwardSection::default(),
            audit: LogAuditSection::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSamplingSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sample_rate_info")]
    pub info_rate: u64,
    #[serde(default = "default_sample_rate_debug")]
    pub debug_rate: u64,
    #[serde(default = "default_sample_rate_trace")]
    pub trace_rate: u64,
}

fn default_sample_rate_info() -> u64 {
    1
}

fn default_sample_rate_debug() -> u64 {
    10
}

fn default_sample_rate_trace() -> u64 {
    100
}

impl Default for LogSamplingSection {
    fn default() -> Self {
        LogSamplingSection {
            enabled: false,
            info_rate: default_sample_rate_info(),
            debug_rate: default_sample_rate_debug(),
            trace_rate: default_sample_rate_trace(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRotationSection {
    #[serde(default = "default_rotation_max_file_size_mb")]
    pub max_file_size_mb: u64,
    #[serde(default = "default_rotation_max_files")]
    pub max_files: u32,
    #[serde(default = "default_rotation_max_age_hours")]
    pub max_age_hours: u64,
    #[serde(default = "default_rotation_compress")]
    pub compress: bool,
}

fn default_rotation_max_file_size_mb() -> u64 {
    100
}

fn default_rotation_max_files() -> u32 {
    10
}

fn default_rotation_max_age_hours() -> u64 {
    24
}

fn default_rotation_compress() -> bool {
    true
}

impl Default for LogRotationSection {
    fn default() -> Self {
        LogRotationSection {
            max_file_size_mb: default_rotation_max_file_size_mb(),
            max_files: default_rotation_max_files(),
            max_age_hours: default_rotation_max_age_hours(),
            compress: default_rotation_compress(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogForwardSection {
    #[serde(default = "default_forward_buffer_capacity")]
    pub buffer_capacity: usize,
    #[serde(default = "default_forward_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_forward_flush_interval_ms")]
    pub flush_interval_ms: u64,
    #[serde(default)]
    pub sinks: Vec<LogForwardSinkSection>,
}

fn default_forward_buffer_capacity() -> usize {
    8192
}

fn default_forward_batch_size() -> usize {
    200
}

fn default_forward_flush_interval_ms() -> u64 {
    1000
}

impl Default for LogForwardSection {
    fn default() -> Self {
        LogForwardSection {
            buffer_capacity: default_forward_buffer_capacity(),
            batch_size: default_forward_batch_size(),
            flush_interval_ms: default_forward_flush_interval_ms(),
            sinks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogForwardSinkSection {
    #[serde(rename = "type")]
    pub sink_type: String,
    pub endpoint: Option<String>,
    pub index_prefix: Option<String>,
    pub subject: Option<String>,
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogAuditSection {
    pub output: Option<String>,
    #[serde(default)]
    pub rotation: LogRotationSection,
}

impl Default for LogAuditSection {
    fn default() -> Self {
        LogAuditSection {
            output: None,
            rotation: LogRotationSection {
                max_file_size_mb: 50,
                max_files: 30,
                max_age_hours: 168,
                compress: true,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingSection {
    #[serde(default)]
    pub export_dir: Option<String>,
    #[serde(default = "default_billing_interval")]
    pub export_interval_secs: u64,
}

fn default_billing_interval() -> u64 {
    3600
}

impl Default for BillingSection {
    fn default() -> Self {
        BillingSection {
            export_dir: None,
            export_interval_secs: default_billing_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcSection {
    #[serde(default = "default_gc_keep_versions")]
    pub artifact_keep_versions: usize,
    #[serde(default = "default_gc_metrics_days")]
    pub metrics_retain_days: u32,
    #[serde(default = "default_gc_grace_secs")]
    pub undeploy_grace_secs: u64,
    #[serde(default = "default_gc_interval_secs")]
    pub gc_interval_secs: u64,
    #[serde(default = "default_gc_disk_threshold")]
    pub disk_warning_threshold: f64,
}

fn default_gc_keep_versions() -> usize {
    5
}

fn default_gc_metrics_days() -> u32 {
    30
}

fn default_gc_grace_secs() -> u64 {
    300
}

fn default_gc_interval_secs() -> u64 {
    3600
}

fn default_gc_disk_threshold() -> f64 {
    0.9
}

impl Default for GcSection {
    fn default() -> Self {
        GcSection {
            artifact_keep_versions: default_gc_keep_versions(),
            metrics_retain_days: default_gc_metrics_days(),
            undeploy_grace_secs: default_gc_grace_secs(),
            gc_interval_secs: default_gc_interval_secs(),
            disk_warning_threshold: default_gc_disk_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSection {
    #[serde(default = "default_rps")]
    pub default_requests_per_second: u32,
    #[serde(default = "default_burst")]
    pub default_burst_capacity: u32,
    #[serde(default = "default_per_ip")]
    pub default_per_ip_limit: u32,
}

fn default_rps() -> u32 {
    100
}

fn default_burst() -> u32 {
    200
}

fn default_per_ip() -> u32 {
    10
}

impl Default for RateLimitSection {
    fn default() -> Self {
        RateLimitSection {
            default_requests_per_second: default_rps(),
            default_burst_capacity: default_burst(),
            default_per_ip_limit: default_per_ip(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EbpfSection {
    #[serde(default = "default_ebpf_enabled")]
    pub enabled: bool,
    #[serde(default = "default_fd_soft")]
    pub fd_soft_limit: u32,
    #[serde(default = "default_fd_hard")]
    pub fd_hard_limit: u32,
    #[serde(default = "default_mem_low")]
    pub mem_low_threshold_pages: u64,
    #[serde(default = "default_mem_critical")]
    pub mem_critical_threshold_pages: u64,
    #[serde(default = "default_disk_slow")]
    pub disk_slow_threshold_ns: u64,
    #[serde(default = "default_tcp_limit")]
    pub tcp_conn_limit_per_pid: u32,
    #[serde(default = "default_syscall_rate")]
    pub syscall_rate_limit: u64,
    #[serde(default = "default_sampling")]
    pub sampling_period_secs: u64,
    #[serde(default = "default_enable_namespace_enforcer")]
    pub enable_namespace_enforcer: bool,
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,
    #[serde(default = "default_enable_forged_header_detect")]
    pub enable_forged_header_detect: bool,
}

fn default_ebpf_enabled() -> bool {
    true
}

fn default_fd_soft() -> u32 {
    8192
}

fn default_fd_hard() -> u32 {
    9728
}

fn default_mem_low() -> u64 {
    65536
}

fn default_mem_critical() -> u64 {
    16384
}

fn default_disk_slow() -> u64 {
    50_000_000
}

fn default_tcp_limit() -> u32 {
    10000
}

fn default_syscall_rate() -> u64 {
    100_000
}

fn default_sampling() -> u64 {
    10
}

fn default_enable_namespace_enforcer() -> bool {
    true
}

fn default_gateway_port() -> u16 {
    crate::INTERNAL_GATEWAY_PORT
}

fn default_enable_forged_header_detect() -> bool {
    true
}

impl Default for EbpfSection {
    fn default() -> Self {
        EbpfSection {
            enabled: default_ebpf_enabled(),
            fd_soft_limit: default_fd_soft(),
            fd_hard_limit: default_fd_hard(),
            mem_low_threshold_pages: default_mem_low(),
            mem_critical_threshold_pages: default_mem_critical(),
            disk_slow_threshold_ns: default_disk_slow(),
            tcp_conn_limit_per_pid: default_tcp_limit(),
            syscall_rate_limit: default_syscall_rate(),
            sampling_period_secs: default_sampling(),
            enable_namespace_enforcer: default_enable_namespace_enforcer(),
            gateway_port: default_gateway_port(),
            enable_forged_header_detect: default_enable_forged_header_detect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSection {
    #[serde(default)]
    pub platform_domain: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_token: Option<String>,
    /// Enable the embedded DNS stub for *.internal resolution.
    #[serde(default = "default_true")]
    pub stub_enabled: bool,
    /// UDP port for the embedded DNS stub (0 = auto-assign).
    #[serde(default = "default_dns_stub_port")]
    pub stub_port: u16,
}

fn default_dns_stub_port() -> u16 {
    15353
}

impl Default for DnsSection {
    fn default() -> Self {
        DnsSection {
            platform_domain: None,
            webhook_url: None,
            webhook_token: None,
            stub_enabled: true,
            stub_port: 15353,
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for GatewaySection {
    fn default() -> Self {
        GatewaySection {
            oidc: None,
            rate_limit: GatewayRateLimitSection::default(),
            circuit_breaker: GatewayCircuitBreakerSection::default(),
        }
    }
}
