use clap::Parser;

/// CLI surface for node startup and one-shot maintenance commands.
#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
pub(crate) struct Args {
    /// Path to a TOML configuration file. Values in the file are used as
    /// defaults; CLI flags and environment variables take precedence.
    #[arg(long)]
    pub(crate) config: Option<String>,

    #[arg(long, default_value = "/tmp/wasm-node/state.redb")]
    pub(crate) db_path: String,

    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    pub(crate) nats_url: String,

    #[arg(long)]
    pub(crate) nats_creds: Option<String>,

    #[arg(long, default_value = "8080")]
    pub(crate) proxy_port: u16,

    #[arg(long, default_value = "8443")]
    pub(crate) proxy_https_port: u16,

    #[arg(long)]
    pub(crate) tls_cert: Option<String>,

    #[arg(long)]
    pub(crate) tls_key: Option<String>,

    #[arg(long, default_value = "9090")]
    pub(crate) admin_port: u16,

    #[arg(long, env = "WASM_NODE_ADMIN_BIND_ADDRESS")]
    pub(crate) admin_bind_address: Option<String>,

    #[arg(long, default_value = "9091")]
    pub(crate) artifact_port: u16,

    #[arg(long, env = "WASM_NODE_ADMIN_ARTIFACT_BIND_ADDRESS")]
    pub(crate) artifact_bind_address: Option<String>,

    #[arg(long, default_value = "9092", env = "WASM_NODE_DEPLOY_INGRESS_PORT")]
    pub(crate) deploy_ingress_port: u16,

    #[arg(long, env = "WASM_NODE_DEPLOY_INGRESS_BIND_ADDRESS")]
    pub(crate) deploy_ingress_bind_address: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_TLS_CERT")]
    pub(crate) admin_tls_cert: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_TLS_KEY")]
    pub(crate) admin_tls_key: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_ADVERTISED_HOST")]
    pub(crate) admin_advertised_host: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_ADVERTISED_ARTIFACT_URL")]
    pub(crate) admin_advertised_artifact_url: Option<String>,

    #[arg(long)]
    pub(crate) otlp_endpoint: Option<String>,

    #[arg(long, env = "NODE_ID", default_value = "node-0")]
    pub(crate) node_id: String,

    #[arg(long, default_value = "10000")]
    pub(crate) port_start: u16,

    #[arg(long, default_value = "19999")]
    pub(crate) port_end: u16,

    #[arg(long, default_value = "generate")]
    pub(crate) key_source: String,

    #[arg(long)]
    pub(crate) key_file: Option<String>,

    #[arg(long)]
    pub(crate) key_command: Vec<String>,

    #[arg(long)]
    pub(crate) key_vault_url: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_token_env: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_mount: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_path: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_field: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_transit_mount: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_transit_key: Option<String>,

    #[arg(long)]
    pub(crate) key_vault_transit_context: Option<String>,

    #[arg(long)]
    pub(crate) key_aws_kms_region: Option<String>,

    #[arg(long)]
    pub(crate) key_aws_kms_endpoint: Option<String>,

    #[arg(long)]
    pub(crate) key_aws_kms_key_id: Option<String>,

    #[arg(long)]
    pub(crate) key_aws_kms_context: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_CACHE_DIRECTORY")]
    pub(crate) runtime_cache_directory: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_UPGRADE_SIGNING_PUBLIC_KEY")]
    pub(crate) runtime_upgrade_signing_public_key: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_ALLOCATOR")]
    pub(crate) runtime_pooling_allocator: Option<bool>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_TOTAL_COMPONENT_INSTANCES")]
    pub(crate) runtime_pooling_total_component_instances: Option<u32>,

    #[arg(
        long,
        env = "WASM_NODE_RUNTIME_POOLING_MAX_CORE_INSTANCES_PER_COMPONENT"
    )]
    pub(crate) runtime_pooling_max_core_instances_per_component: Option<u32>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_MAX_MEMORIES_PER_COMPONENT")]
    pub(crate) runtime_pooling_max_memories_per_component: Option<u32>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_MAX_TABLES_PER_COMPONENT")]
    pub(crate) runtime_pooling_max_tables_per_component: Option<u32>,

    #[arg(long, env = "ADMIN_TOKEN")]
    pub(crate) admin_token: Option<String>,

    /// Enable admin API authentication (requires tokens).
    #[arg(long)]
    pub(crate) auth_enabled: Option<bool>,

    /// Read-only bearer token for admin API (for Prometheus, monitoring).
    #[arg(long, env = "WASM_NODE_AUTH_READ_TOKEN")]
    pub(crate) auth_read_token: Option<String>,

    /// Read-write bearer token for admin API (for operators, CI/CD).
    #[arg(long, env = "WASM_NODE_AUTH_WRITE_TOKEN")]
    pub(crate) auth_write_token: Option<String>,

    /// Require TLS for admin API when auth is enabled (default: true).
    #[arg(long)]
    pub(crate) auth_require_tls: Option<bool>,

    /// Admin API rate limit per second per IP (default: 10).
    #[arg(long)]
    pub(crate) auth_rate_limit_per_second: Option<u32>,

    /// Admin API rate limit burst capacity (default: 20).
    #[arg(long)]
    pub(crate) auth_rate_limit_burst: Option<u32>,

    /// Generate random auth tokens and print to stdout, then exit.
    #[arg(long)]
    pub(crate) generate_tokens: bool,

    #[arg(long, default_value = "postgres://127.0.0.1:5432")]
    pub(crate) database_url: String,

    #[arg(long, default_value = "127.0.0.1:5432")]
    pub(crate) pgbouncer_addr: String,

    #[arg(long)]
    pub(crate) enable_db_proxy: bool,

    #[arg(long, default_value = "127.0.0.1:5433")]
    pub(crate) db_proxy_addr: String,

    #[arg(long, default_value = "db.internal:5432")]
    pub(crate) db_backend_addr: String,

    #[arg(long, default_value = "20")]
    pub(crate) db_proxy_max_connections: usize,

    #[arg(
        long,
        help = "Directory for billing record exports (if set, enables periodic export)"
    )]
    pub(crate) billing_export_dir: Option<String>,

    #[arg(
        long,
        default_value = "3600",
        help = "Billing export interval in seconds (requires --billing-export-dir)"
    )]
    pub(crate) billing_export_interval_secs: u64,

    #[arg(long, help = "Platform domain for subdomains (e.g. myplatform.com)")]
    pub(crate) platform_domain: Option<String>,

    #[arg(long, help = "Webhook URL for DNS automation")]
    pub(crate) dns_webhook_url: Option<String>,

    #[arg(long, help = "Auth token for DNS webhook")]
    pub(crate) dns_webhook_token: Option<String>,

    /// Generate a default config file and print to stdout, then exit.
    #[arg(long)]
    pub(crate) generate_config: bool,

    /// Validate a config file without starting the node, then exit.
    #[arg(long)]
    pub(crate) validate_config: Option<String>,

    /// Log output format: "json" or "text"
    #[arg(long, default_value = "json", env = "WASM_NODE_LOG_FORMAT")]
    pub(crate) log_format: String,

    /// Log output destination: "stdout", "stderr", or a file path
    #[arg(long, env = "WASM_NODE_LOG_OUTPUT")]
    pub(crate) log_output: Option<String>,

    /// Default log level (overridden by RUST_LOG)
    #[arg(long, default_value = "info", env = "WASM_NODE_LOG_LEVEL")]
    pub(crate) log_level: String,

    /// Enable log sampling for high-throughput scenarios
    #[arg(long, default_value = "false")]
    pub(crate) log_sampling: bool,

    /// INFO log sampling rate (1 = 100%, 10 = 10%)
    #[arg(long, default_value = "1")]
    pub(crate) log_info_sample_rate: u64,

    /// DEBUG log sampling rate
    #[arg(long, default_value = "10")]
    pub(crate) log_debug_sample_rate: u64,

    /// TRACE log sampling rate
    #[arg(long, default_value = "100")]
    pub(crate) log_trace_sample_rate: u64,
}
