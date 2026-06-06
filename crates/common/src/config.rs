//! Configuration structures for the Wasm Cloud Platform.
//! All fields have defaults; a completely empty TOML file is valid.

use serde::{Deserialize, Serialize};

#[path = "config_health_gateway.rs"]
mod config_health_gateway;
pub use config_health_gateway::{
    AppHealthCheckDefaults, GatewayCircuitBreakerSection, GatewayRateLimitSection, GatewaySection,
    HealthSection,
};

use std::path::PathBuf;

/// Top-level configuration for a wasm-node.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

fn default_true() -> bool {
    true
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StorageOpenFailureMode {
    /// Preserve the unreadable DB by moving it aside, then fail startup.
    #[default]
    QuarantineAndFail,
    /// Preserve the unreadable DB by moving it aside, then create a fresh DB.
    QuarantineAndRecreate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StorageIntegrityFailureMode {
    /// Preserve the corrupted DB by moving it aside, then exit and require an operator restart.
    #[default]
    QuarantineAndExit,
    /// Delete the corrupted DB on exit. This is destructive and should only be used
    /// when the operator explicitly opts into disposable local state.
    DeleteAndExit,
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
    #[serde(default = "default_deploy_ingress_port")]
    pub deploy_ingress_port: u16,
    /// Local bind host/address for the admin API listener.
    #[serde(default = "default_admin_bind_address")]
    pub bind_address: String,
    /// Local bind host/address for the artifact server listener.
    #[serde(default = "default_admin_bind_address")]
    pub artifact_bind_address: String,
    /// Local bind host/address for the deploy ingress listener.
    #[serde(default = "default_admin_bind_address")]
    pub deploy_ingress_bind_address: String,
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

fn default_deploy_ingress_port() -> u16 {
    9092
}

fn default_admin_bind_address() -> String {
    "127.0.0.1".to_string()
}

impl Default for AdminSection {
    fn default() -> Self {
        AdminSection {
            port: default_admin_port(),
            artifact_port: default_artifact_port(),
            deploy_ingress_port: default_deploy_ingress_port(),
            bind_address: default_admin_bind_address(),
            artifact_bind_address: default_admin_bind_address(),
            deploy_ingress_bind_address: default_admin_bind_address(),
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
/// trusted_proxies = ["10.0.0.0/8", "192.168.1.10"]
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

    /// Trusted proxy IPs/CIDR ranges allowed to supply forwarded client IP
    /// headers for the admin API.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
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
            trusted_proxies: Vec::new(),
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
            trusted_proxies: section.trusted_proxies,
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
            trusted_proxies: config.trusted_proxies,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default = "default_port_start")]
    pub port_start: u16,
    #[serde(default = "default_port_end")]
    pub port_end: u16,
    #[serde(default = "default_instance_bind_address")]
    pub instance_bind_address: String,
    /// KEK/transport seal-key source:
    /// - `generate` for ephemeral in-memory keys only
    /// - `file` for a raw 32-byte key from `key_file`
    /// - `command` for a raw 32-byte key or 64-hex-char key from `key_command`
    /// - `vault-kv` for a seal key loaded from a Vault KV v2 secret
    /// - `vault-transit` for a 32-byte seal key derived from a Vault transit HMAC
    /// - `aws-kms-hmac` for a 32-byte seal key derived from AWS KMS GenerateMac
    /// - `env:VAR_NAME` for a raw 32-byte or 64-hex-char key from env
    /// - `passphrase-env:VAR_NAME` for an Argon2id-derived seal key from an env passphrase
    #[serde(default = "default_key_source")]
    pub key_source: String,
    #[serde(default)]
    pub key_file: Option<String>,
    /// Optional command argv used when `key_source = "command"`.
    /// The first element is the executable, remaining elements are arguments.
    #[serde(default)]
    pub key_command: Vec<String>,
    /// Base URL for Vault when `key_source = "vault-kv"`, e.g. `https://vault.service:8200`.
    #[serde(default)]
    pub key_vault_url: Option<String>,
    /// Environment variable containing the Vault token when `key_source = "vault-kv"`.
    #[serde(default)]
    pub key_vault_token_env: Option<String>,
    /// KV v2 mount name when `key_source = "vault-kv"`.
    #[serde(default = "default_key_vault_mount")]
    pub key_vault_mount: String,
    /// Secret path under the KV v2 mount when `key_source = "vault-kv"`.
    #[serde(default)]
    pub key_vault_path: Option<String>,
    /// Secret field containing the seal key when `key_source = "vault-kv"`.
    #[serde(default = "default_key_vault_field")]
    pub key_vault_field: String,
    /// Transit mount name when `key_source = "vault-transit"`.
    #[serde(default = "default_key_vault_transit_mount")]
    pub key_vault_transit_mount: String,
    /// Transit key name when `key_source = "vault-transit"`.
    #[serde(default)]
    pub key_vault_transit_key: Option<String>,
    /// Transit derivation context when `key_source = "vault-transit"`.
    #[serde(default)]
    pub key_vault_transit_context: Option<String>,
    /// AWS region when `key_source = "aws-kms-hmac"`.
    #[serde(default)]
    pub key_aws_kms_region: Option<String>,
    /// Optional endpoint override when `key_source = "aws-kms-hmac"`.
    #[serde(default)]
    pub key_aws_kms_endpoint: Option<String>,
    /// AWS KMS HMAC key id/arn when `key_source = "aws-kms-hmac"`.
    #[serde(default)]
    pub key_aws_kms_key_id: Option<String>,
    /// Stable derivation context when `key_source = "aws-kms-hmac"`.
    #[serde(default)]
    pub key_aws_kms_context: Option<String>,
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

fn default_instance_bind_address() -> String {
    "127.0.0.1".to_string()
}

fn default_key_source() -> String {
    "generate".to_string()
}

fn default_key_vault_mount() -> String {
    "secret".to_string()
}

fn default_key_vault_field() -> String {
    "key".to_string()
}

fn default_key_vault_transit_mount() -> String {
    "transit".to_string()
}

fn default_pooling_total_component_instances() -> u32 {
    1000
}

impl Default for RuntimeSection {
    fn default() -> Self {
        RuntimeSection {
            port_start: default_port_start(),
            port_end: default_port_end(),
            instance_bind_address: default_instance_bind_address(),
            key_source: default_key_source(),
            key_file: None,
            key_command: Vec::new(),
            key_vault_url: None,
            key_vault_token_env: None,
            key_vault_mount: default_key_vault_mount(),
            key_vault_path: None,
            key_vault_field: default_key_vault_field(),
            key_vault_transit_mount: default_key_vault_transit_mount(),
            key_vault_transit_key: None,
            key_vault_transit_context: None,
            key_aws_kms_region: None,
            key_aws_kms_endpoint: None,
            key_aws_kms_key_id: None,
            key_aws_kms_context: None,
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
