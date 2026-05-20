use clap::Parser;
use messaging::reconnect::{NatsHealth, NatsHealthWatcher};
use reqwest::Url;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

fn symm_key_from_exact_32(
    bytes: &[u8],
    source: &str,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    if bytes.len() != 32 {
        anyhow::bail!(
            "{source} must contain exactly 32 bytes, found {} bytes",
            bytes.len()
        );
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Ok(secrets::crypto::SymmetricKey::from_bytes(key))
}

fn load_kek_from_env_spec(spec: &str) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let var_name = spec
        .strip_prefix("env:")
        .ok_or_else(|| anyhow::anyhow!("invalid env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    let trimmed = raw.trim();

    // Accept either raw 32-byte strings or 64-char hex for operator convenience.
    if trimmed.len() == 64 {
        let decoded = hex::decode(trimmed)
            .map_err(|e| anyhow::anyhow!("failed to decode hex KEK from {var_name}: {e}"))?;
        return symm_key_from_exact_32(&decoded, &format!("environment variable {var_name}"));
    }

    symm_key_from_exact_32(raw.as_bytes(), &format!("environment variable {var_name}"))
}

fn seal_kek_blob(
    seal_key: &secrets::crypto::SymmetricKey,
    kek_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    Ok(secrets::crypto::encrypt(seal_key, kek_bytes)?.0)
}

fn load_or_create_persisted_kek(
    store: &storage::Store,
    seal_key: &secrets::crypto::SymmetricKey,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    match store.load_kek()? {
        Some(bytes) if bytes.len() == 32 => {
            let legacy = symm_key_from_exact_32(&bytes, "legacy plaintext KEK")?;
            let sealed = seal_kek_blob(seal_key, legacy.as_bytes())?;
            store.save_kek(&sealed)?;
            tracing::warn!(
                "migrated legacy plaintext KEK in redb into a sealed-at-rest blob using the configured key source"
            );
            Ok(legacy)
        }
        Some(sealed_blob) => {
            let plaintext =
                secrets::crypto::decrypt(seal_key, &secrets::crypto::EncryptedBlob(sealed_blob))?;
            tracing::info!("loaded sealed KEK from redb using configured key source");
            symm_key_from_exact_32(&plaintext, "persisted sealed KEK")
        }
        None => {
            let initial_kek = symm_key_from_exact_32(
                seal_key.as_bytes(),
                "configured file/env key source initial KEK",
            )?;
            let sealed = seal_kek_blob(seal_key, initial_kek.as_bytes())?;
            store.save_kek(&sealed)?;
            tracing::info!("initialized sealed KEK in redb from configured key source");
            Ok(initial_kek)
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed.eq_ignore_ascii_case("localhost")
        || trimmed
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn normalize_artifact_base_url(raw: &str) -> anyhow::Result<String> {
    let url = Url::parse(raw.trim())
        .map_err(|e| anyhow::anyhow!("invalid advertised artifact URL '{}': {e}", raw.trim()))?;
    let mut normalized = url.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

fn host_for_socket_address(host: &str) -> String {
    let trimmed = host.trim();
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{trimmed}]"),
        _ => trimmed.to_string(),
    }
}

fn bind_socket_address(host: &str, port: u16) -> anyhow::Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        anyhow::bail!("bind host must not be empty");
    }
    Ok(format!("{}:{}", host_for_socket_address(trimmed), port))
}

fn advertised_host_base_url(host: &str, port: u16) -> anyhow::Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        anyhow::bail!("admin.advertised_host must not be empty");
    }

    normalize_artifact_base_url(&format!(
        "http://{}:{}",
        host_for_socket_address(trimmed),
        port
    ))
}

fn build_artifact_server_url(admin: &common::config::AdminSection) -> anyhow::Result<String> {
    if let Some(url) = admin.advertised_artifact_url.as_deref() {
        return normalize_artifact_base_url(url);
    }
    if let Some(host) = admin.advertised_host.as_deref() {
        return advertised_host_base_url(host, admin.artifact_port);
    }
    Ok(format!("http://127.0.0.1:{}", admin.artifact_port))
}

fn build_proxy_advertised_address(config: &common::config::NodeConfig) -> anyhow::Result<String> {
    if let Some(host) = config.admin.advertised_host.as_deref() {
        return bind_socket_address(host, config.proxy.http_port);
    }
    Ok(format!("127.0.0.1:{}", config.proxy.http_port))
}

fn artifact_server_url_is_loopback(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToOwned::to_owned))
        .map(|host| is_loopback_host(&host))
        .unwrap_or(false)
}

const BOOTSTRAP_ARTIFACT_TOKEN_TTL_SECS: u64 = 600;

fn generate_artifact_peer_token() -> String {
    common::auth::AuthConfig::generate_token()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn admin_tls_material(config: &common::config::NodeConfig) -> Option<(String, String)> {
    if let (Some(cert), Some(key)) = (config.admin.tls_cert.clone(), config.admin.tls_key.clone()) {
        return Some((cert, key));
    }
    if let (Some(cert), Some(key)) = (config.proxy.tls_cert.clone(), config.proxy.tls_key.clone()) {
        return Some((cert, key));
    }
    None
}

fn admin_tls_is_configured(config: &common::config::NodeConfig) -> bool {
    admin_tls_material(config).is_some()
}

async fn serve_admin_app(
    admin_addr: String,
    admin_app: axum::Router,
    tls_cert: Option<String>,
    tls_key: Option<String>,
) -> anyhow::Result<()> {
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .map_err(|e| anyhow::anyhow!("admin TLS config error: {e}"))?;
        info!(addr = %admin_addr, "admin API listening with TLS");
        axum_server::bind_rustls(
            admin_addr
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid admin bind address: {e}"))?,
            rustls_config,
        )
        .serve(admin_app.into_make_service())
        .await
        .map_err(|e| anyhow::anyhow!("admin HTTPS server error: {e}"))?;
    } else {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .map_err(|e| anyhow::anyhow!("admin API bind failed: {e}"))?;
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app)
            .await
            .map_err(|e| anyhow::anyhow!("admin HTTP server error: {e}"))?;
    }
    Ok(())
}

fn load_kek_from_config(
    store: &storage::Store,
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    match runtime.key_source.as_str() {
        "file" => {
            let key_file = runtime
                .key_file
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime.key_source=file requires runtime.key_file"))?;

            let bytes = std::fs::read(key_file)
                .map_err(|e| anyhow::anyhow!("failed to read key file {}: {}", key_file, e))?;
            tracing::info!(path = %key_file, "loaded KEK seal key from key file");
            let seal_key = symm_key_from_exact_32(&bytes, &format!("key file {key_file}"))?;
            load_or_create_persisted_kek(store, &seal_key)
        }
        spec if spec.starts_with("env:") => {
            let seal_key = load_kek_from_env_spec(spec)?;
            load_or_create_persisted_kek(store, &seal_key)
        }
        "generate" => {
            if let Ok(Some(_persisted_kek)) = store.load_kek() {
                anyhow::bail!(
                    "persisted KEK detected in redb; key_source=generate cannot unlock or replace persisted secret state safely. Configure runtime.key_source=file or env:VAR_NAME to keep existing secrets"
                );
            }
            tracing::warn!(
                "key_source=generate: using ephemeral KEK; secrets created on this node will not survive restart"
            );
            Ok(secrets::crypto::SymmetricKey::generate())
        }
        other => anyhow::bail!(
            "unsupported runtime.key_source '{}'; supported values are 'generate', 'file', or 'env:VAR_NAME'",
            other
        ),
    }
}

use ebpf_monitor::{ActionDispatcher, EbpfMetrics, EventCallbacks, MonitorConfig};
use supervisor::SupervisorCommand;

mod dns_stub;

/// Platform callbacks for the eBPF monitor's recovery actions.
///
/// This struct implements the `EventCallbacks` trait defined in the
/// `ebpf_monitor` crate, bridging kernel-level events to the platform's
/// actual components (backpressure signal, NATS health, event bus).
///
/// The eBPF monitor calls these methods when it detects anomalies that
/// require automated recovery actions. The decoupled design (trait object)
/// avoids circular dependencies between the `ebpf_monitor` and
/// `proxy`/`messaging` crates.
struct NodeEbpfCallbacks {
    backpressure: proxy::backpressure::BackpressureSignal,
    nats_health: Arc<NatsHealth>,
    bus: messaging::NatsBus,
    #[allow(dead_code)]
    node_id: String,
    /// Channel to send immediate action commands to the supervisor
    /// (kill largest instance, prune idle, remove from upstream).
    supervisor_tx: mpsc::Sender<SupervisorCommand>,
}

impl EventCallbacks for NodeEbpfCallbacks {
    fn activate_backpressure(&self, reason: &str) {
        warn!(reason, "eBPF: activating backpressure");
        self.backpressure.set_rejecting();
    }

    fn deactivate_backpressure(&self) {
        info!("eBPF: deactivating backpressure");
        self.backpressure.set_accepting();
    }

    fn mark_nats_disconnected(&self) {
        warn!("eBPF: pre-emptive NATS disconnect (TCP retransmits detected)");
        self.nats_health.mark_disconnected();
    }

    fn publish_node_under_pressure(&self, node_id: &str, pressure_level: u32) {
        let event = messaging::events::Event::NodeUnderPressure {
            node_id: node_id.to_string(),
            pressure_level,
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish pressure event: {}", e);
            }
        });
    }

    fn publish_node_pressure_recovered(&self, node_id: &str) {
        let event = messaging::events::Event::NodePressureRecovered {
            node_id: node_id.to_string(),
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish pressure recovered event: {}", e);
            }
        });
    }

    fn publish_security_incident(&self, node_id: &str, pid: u32, syscall_nr: u64, category: &str) {
        let event = messaging::events::Event::SecurityIncident {
            node_id: node_id.to_string(),
            app_id: String::new(), // Unknown at eBPF level
            pid,
            syscall_nr,
            category: category.to_string(),
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish security incident event: {}", e);
            }
        });
    }

    fn kill_instance(&self, pid: u32, reason: &str) {
        // Wasm instances run as in-process Tokio tasks, not separate OS
        // processes. The PID from eBPF refers to the node process itself
        // or a child process. We request the supervisor kill the largest
        // instance (most memory) as the best recovery action.
        warn!(
            pid,
            reason, "eBPF: kill instance requested — sending KillLargestInstance to supervisor"
        );
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::KillLargestInstance {
                reason: reason.to_string(),
            })
        {
            warn!(error = %e, "Failed to send KillLargestInstance command to supervisor");
        }
    }

    fn prune_idle_instances(&self) {
        // Kill all instances idle for more than 60 seconds to free FDs.
        warn!("eBPF: prune idle instances requested — sending PruneIdleInstances to supervisor");
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::PruneIdleInstances {
                idle_threshold_secs: 60,
            })
        {
            warn!(error = %e, "Failed to send PruneIdleInstances command to supervisor");
        }
    }

    fn remove_from_upstream(&self, pid: u32) {
        // The eBPF monitor detected a process exit. Since Wasm instances
        // are in-process, the health loop will handle cleanup. Log for
        // visibility — the PID may refer to a child process spawned by
        // a Wasm instance that has already exited.
        debug!(
            pid,
            "eBPF: remove from upstream requested — process exit detected, health loop will handle"
        );
    }

    fn kill_instance_by_tid(&self, tid: u32, reason: &str) {
        // eBPF namespace enforcement detected a forged header or other
        // security incident from a specific TID. Request the supervisor
        // to kill the largest instance as the best recovery action.
        // (The supervisor doesn't have a per-TID kill command yet, so
        // we fall back to KillLargestInstance as the most aggressive
        // recovery action available.)
        warn!(
            tid,
            reason, "eBPF: namespace security incident — kill instance by TID requested"
        );
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::KillLargestInstance {
                reason: format!("{} (tid={})", reason, tid),
            })
        {
            warn!(
                error = %e,
                "Failed to send KillLargestInstance command for TID security incident"
            );
        }
    }
}

pub mod db_config;
pub mod handlers;
pub mod log_reload;
pub mod recovery;
pub mod upgrade;

// Configuration is now handled by the `config` crate.
use config::{load_config, CliOverrides};

#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
struct Args {
    /// Path to a TOML configuration file. Values in the file are used as
    /// defaults; CLI flags and environment variables take precedence.
    #[arg(long)]
    config: Option<String>,

    #[arg(long, default_value = "/tmp/wasm-node/state.redb")]
    db_path: String,

    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    #[arg(long)]
    nats_creds: Option<String>,

    #[arg(long, default_value = "8080")]
    proxy_port: u16,

    #[arg(long, default_value = "8443")]
    proxy_https_port: u16,

    #[arg(long)]
    tls_cert: Option<String>,

    #[arg(long)]
    tls_key: Option<String>,

    #[arg(long, default_value = "9090")]
    admin_port: u16,

    #[arg(long, env = "WASM_NODE_ADMIN_BIND_ADDRESS")]
    admin_bind_address: Option<String>,

    #[arg(long, default_value = "9091")]
    artifact_port: u16,

    #[arg(long, env = "WASM_NODE_ADMIN_ARTIFACT_BIND_ADDRESS")]
    artifact_bind_address: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_TLS_CERT")]
    admin_tls_cert: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_TLS_KEY")]
    admin_tls_key: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_ADVERTISED_HOST")]
    admin_advertised_host: Option<String>,

    #[arg(long, env = "WASM_NODE_ADMIN_ADVERTISED_ARTIFACT_URL")]
    admin_advertised_artifact_url: Option<String>,

    #[arg(long)]
    otlp_endpoint: Option<String>,

    #[arg(long, env = "NODE_ID", default_value = "node-0")]
    node_id: String,

    #[arg(long, default_value = "10000")]
    port_start: u16,

    #[arg(long, default_value = "19999")]
    port_end: u16,

    #[arg(long, default_value = "generate")]
    key_source: String,

    #[arg(long)]
    key_file: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_CACHE_DIRECTORY")]
    runtime_cache_directory: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_UPGRADE_SIGNING_PUBLIC_KEY")]
    runtime_upgrade_signing_public_key: Option<String>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_ALLOCATOR")]
    runtime_pooling_allocator: Option<bool>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_TOTAL_COMPONENT_INSTANCES")]
    runtime_pooling_total_component_instances: Option<u32>,

    #[arg(
        long,
        env = "WASM_NODE_RUNTIME_POOLING_MAX_CORE_INSTANCES_PER_COMPONENT"
    )]
    runtime_pooling_max_core_instances_per_component: Option<u32>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_MAX_MEMORIES_PER_COMPONENT")]
    runtime_pooling_max_memories_per_component: Option<u32>,

    #[arg(long, env = "WASM_NODE_RUNTIME_POOLING_MAX_TABLES_PER_COMPONENT")]
    runtime_pooling_max_tables_per_component: Option<u32>,

    #[arg(long, env = "ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Enable admin API authentication (requires tokens).
    #[arg(long)]
    auth_enabled: Option<bool>,

    /// Read-only bearer token for admin API (for Prometheus, monitoring).
    #[arg(long, env = "WASM_NODE_AUTH_READ_TOKEN")]
    auth_read_token: Option<String>,

    /// Read-write bearer token for admin API (for operators, CI/CD).
    #[arg(long, env = "WASM_NODE_AUTH_WRITE_TOKEN")]
    auth_write_token: Option<String>,

    /// Require TLS for admin API when auth is enabled (default: true).
    #[arg(long)]
    auth_require_tls: Option<bool>,

    /// Admin API rate limit per second per IP (default: 10).
    #[arg(long)]
    auth_rate_limit_per_second: Option<u32>,

    /// Admin API rate limit burst capacity (default: 20).
    #[arg(long)]
    auth_rate_limit_burst: Option<u32>,

    /// Generate random auth tokens and print to stdout, then exit.
    #[arg(long)]
    generate_tokens: bool,

    #[arg(long, default_value = "postgres://127.0.0.1:5432")]
    database_url: String,

    #[arg(long, default_value = "127.0.0.1:5432")]
    pgbouncer_addr: String,

    #[arg(long)]
    enable_db_proxy: bool,

    #[arg(long, default_value = "127.0.0.1:5433")]
    db_proxy_addr: String,

    #[arg(long, default_value = "db.internal:5432")]
    db_backend_addr: String,

    #[arg(long, default_value = "20")]
    db_proxy_max_connections: usize,

    #[arg(
        long,
        help = "Directory for billing record exports (if set, enables periodic export)"
    )]
    billing_export_dir: Option<String>,

    #[arg(
        long,
        default_value = "3600",
        help = "Billing export interval in seconds (requires --billing-export-dir)"
    )]
    billing_export_interval_secs: u64,

    #[arg(long, help = "Platform domain for subdomains (e.g. myplatform.com)")]
    platform_domain: Option<String>,

    #[arg(long, help = "Webhook URL for DNS automation")]
    dns_webhook_url: Option<String>,

    #[arg(long, help = "Auth token for DNS webhook")]
    dns_webhook_token: Option<String>,

    /// Generate a default config file and print to stdout, then exit.
    #[arg(long)]
    generate_config: bool,

    /// Validate a config file without starting the node, then exit.
    #[arg(long)]
    validate_config: Option<String>,

    /// Log output format: "json" or "text"
    #[arg(long, default_value = "json", env = "WASM_NODE_LOG_FORMAT")]
    log_format: String,

    /// Log output destination: "stdout", "stderr", or a file path
    #[arg(long, env = "WASM_NODE_LOG_OUTPUT")]
    log_output: Option<String>,

    /// Default log level (overridden by RUST_LOG)
    #[arg(long, default_value = "info", env = "WASM_NODE_LOG_LEVEL")]
    log_level: String,

    /// Enable log sampling for high-throughput scenarios
    #[arg(long, default_value = "false")]
    log_sampling: bool,

    /// INFO log sampling rate (1 = 100%, 10 = 10%)
    #[arg(long, default_value = "1")]
    log_info_sample_rate: u64,

    /// DEBUG log sampling rate
    #[arg(long, default_value = "10")]
    log_debug_sample_rate: u64,

    /// TRACE log sampling rate
    #[arg(long, default_value = "100")]
    log_trace_sample_rate: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // --generate-config: print a default TOML config to stdout and exit
    if args.generate_config {
        let default_config = common::config::NodeConfig::default();
        let toml_str =
            toml::to_string_pretty(&default_config).expect("failed to serialize default config");
        println!("{}", toml_str);
        return Ok(());
    }

    // --generate-tokens: generate random auth tokens and print to stdout, then exit
    if args.generate_tokens {
        let auth_config = common::auth::AuthConfig::generate_default();
        println!("# Add these to your config.toml under [auth] section:");
        println!("[auth]");
        println!("enabled = true");
        println!(
            "read_token = \"{}\"",
            auth_config.read_token.as_deref().unwrap_or("")
        );
        println!(
            "write_token = \"{}\"",
            auth_config.write_token.as_deref().unwrap_or("")
        );
        println!("require_tls = true");
        println!(
            "rate_limit_per_second = {}",
            auth_config.rate_limit_per_second
        );
        println!("rate_limit_burst = {}", auth_config.rate_limit_burst);
        return Ok(());
    }

    // --validate-config: load and validate a config file, then exit
    if let Some(ref path) = args.validate_config {
        match config::load_config(Some(std::path::Path::new(path)), &CliOverrides::default()) {
            Ok(_) => {
                println!("✅ Configuration file '{}' is valid.", path);
                return Ok(());
            }
            Err(e) => {
                eprintln!("❌ Configuration validation failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Convert CLI args to overrides for the config system
    let cli_overrides = CliOverrides {
        node_id: Some(args.node_id.clone()),
        db_path: Some(args.db_path.clone()),
        nats_url: Some(args.nats_url.clone()),
        nats_creds: args.nats_creds.clone(),
        http_port: Some(args.proxy_port),
        https_port: Some(args.proxy_https_port),
        tls_cert: args.tls_cert.clone(),
        tls_key: args.tls_key.clone(),
        admin_port: Some(args.admin_port),
        artifact_port: Some(args.artifact_port),
        admin_bind_address: args.admin_bind_address.clone(),
        artifact_bind_address: args.artifact_bind_address.clone(),
        admin_tls_cert: args.admin_tls_cert.clone(),
        admin_tls_key: args.admin_tls_key.clone(),
        admin_advertised_host: args.admin_advertised_host.clone(),
        admin_advertised_artifact_url: args.admin_advertised_artifact_url.clone(),
        port_start: Some(args.port_start),
        port_end: Some(args.port_end),
        key_source: Some(args.key_source.clone()),
        key_file: args.key_file.clone(),
        runtime_cache_directory: args.runtime_cache_directory.clone(),
        runtime_upgrade_signing_public_key: args.runtime_upgrade_signing_public_key.clone(),
        runtime_pooling_allocator: args.runtime_pooling_allocator,
        runtime_pooling_total_component_instances: args.runtime_pooling_total_component_instances,
        runtime_pooling_max_core_instances_per_component: args
            .runtime_pooling_max_core_instances_per_component,
        runtime_pooling_max_memories_per_component: args.runtime_pooling_max_memories_per_component,
        runtime_pooling_max_tables_per_component: args.runtime_pooling_max_tables_per_component,
        database_url: Some(args.database_url.clone()),
        pgbouncer_addr: Some(args.pgbouncer_addr.clone()),
        enable_db_proxy: Some(args.enable_db_proxy),
        db_proxy_addr: Some(args.db_proxy_addr.clone()),
        db_proxy_backend: Some(args.db_backend_addr.clone()),
        db_proxy_max_connections: Some(args.db_proxy_max_connections),
        log_level: Some(args.log_level.clone()),
        otlp_endpoint: args.otlp_endpoint.clone(),
        billing_export_dir: args.billing_export_dir.clone(),
        billing_export_interval_secs: Some(args.billing_export_interval_secs),
        platform_domain: args.platform_domain.clone(),
        dns_webhook_url: args.dns_webhook_url.clone(),
        dns_webhook_token: args.dns_webhook_token.clone(),
        auth_token: args.admin_token.clone(),
        auth_enabled: args.auth_enabled,
        auth_read_token: args.auth_read_token.clone(),
        auth_write_token: args.auth_write_token.clone(),
        auth_require_tls: args.auth_require_tls,
        auth_rate_limit_per_second: args.auth_rate_limit_per_second,
        auth_rate_limit_burst: args.auth_rate_limit_burst,
    };

    // Load configuration with merge priority: defaults < TOML < env < CLI
    let config_path = args.config.as_deref().map(std::path::Path::new);
    let config = load_config(config_path, &cli_overrides)?;

    // Set up structured logging with reload handle (allows runtime log-level changes)
    let format = match config.logging.format.as_str() {
        "text" => common::logging::LogFormat::Text,
        _ => common::logging::LogFormat::Json,
    };

    let output = if let Some(ref path) = args.log_output {
        common::logging::LogOutput::File {
            path: std::path::PathBuf::from(path),
        }
    } else if let Some(ref path) = config.logging.output {
        common::logging::LogOutput::File {
            path: std::path::PathBuf::from(path),
        }
    } else {
        common::logging::LogOutput::Stdout
    };

    let logging_config = common::logging::LoggingConfig {
        format,
        output,
        default_level: config.logging.level.clone(),
        module_levels: config.logging.modules.clone(),
        sampling_enabled: args.log_sampling || config.logging.sampling.enabled,
        info_sample_rate: if args.log_info_sample_rate != 1 {
            args.log_info_sample_rate
        } else {
            config.logging.sampling.info_rate
        },
        debug_sample_rate: if args.log_debug_sample_rate != 10 {
            args.log_debug_sample_rate
        } else {
            config.logging.sampling.debug_rate
        },
        trace_sample_rate: if args.log_trace_sample_rate != 100 {
            args.log_trace_sample_rate
        } else {
            config.logging.sampling.trace_rate
        },
        node_id: config.node.node_id.clone(),
        include_source: cfg!(debug_assertions),
    };

    let log_reload_handle = common::logging::init_logging(&logging_config);

    info!(node_id = %config.node.node_id, "wasm-node starting");
    info!(
        config_merge = "defaults + TOML + env + CLI",
        "configuration loaded"
    );

    if let Some(parent) = config.storage.db_path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }

    let store = match storage::Store::open(&config.storage.db_path) {
        Ok(s) => {
            info!(path = %config.storage.db_path.display(), "storage opened");
            s
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %config.storage.db_path.display(),
                mode = ?config.storage.open_failure_mode,
                "failed to open redb"
            );

            if !config.storage.db_path.exists() {
                return Err(anyhow::anyhow!(
                    "failed to open redb at {}: {}",
                    config.storage.db_path.display(),
                    e
                ));
            }

            let quarantined_path = recovery::quarantine_db_file(&config.storage.db_path, "open_failure")
                .map_err(|quarantine_err| {
                    anyhow::anyhow!(
                        "failed to open redb at {}: {}. also failed to quarantine the unreadable DB: {}",
                        config.storage.db_path.display(),
                        e,
                        quarantine_err
                    )
                })?;

            match config.storage.open_failure_mode {
                common::config::StorageOpenFailureMode::QuarantineAndFail => {
                    return Err(anyhow::anyhow!(
                        "failed to open redb at {}: {}. unreadable database quarantined to {}. refusing automatic local state recreation by default; set storage.open_failure_mode = \"quarantine_and_recreate\" only if you intentionally want a fresh local DB bootstrap",
                        config.storage.db_path.display(),
                        e,
                        quarantined_path.display()
                    ));
                }
                common::config::StorageOpenFailureMode::QuarantineAndRecreate => {
                    tracing::warn!(
                        original_path = %config.storage.db_path.display(),
                        quarantined_path = %quarantined_path.display(),
                        "quarantined unreadable redb and recreating a fresh local database due to explicit recovery mode"
                    );
                    let s = storage::Store::open(&config.storage.db_path).map_err(|e2| {
                        anyhow::anyhow!(
                            "failed to open a fresh redb at {} after quarantining {}: {}",
                            config.storage.db_path.display(),
                            quarantined_path.display(),
                            e2
                        )
                    })?;
                    info!(
                        path = %config.storage.db_path.display(),
                        quarantined_path = %quarantined_path.display(),
                        "storage recreated after quarantining unreadable DB"
                    );
                    s
                }
            }
        }
    };

    // Initialize hot-reloadable configuration handle.
    // Loads any persisted overrides from redb so they survive restarts.
    let hot_config_handle =
        config::HotConfigHandle::new(&config, store.clone(), config.node.node_id.clone())?;
    info!("hot config handle initialized (persisted overrides applied if any)");

    // ── Watch channels for hot-reloadable component config ────────────
    // These allow the config sync loop to push updated values to
    // long-running background tasks without restarting them.

    // GC config watch (interval, disk threshold, keep versions, etc.)
    let initial_gc_config = common::gc::GcConfig {
        artifact_keep_versions: config.gc.artifact_keep_versions,
        metrics_retain_days: config.gc.metrics_retain_days,
        undeploy_grace_secs: config.gc.undeploy_grace_secs,
        gc_interval_secs: config.gc.gc_interval_secs,
        disk_warning_threshold: config.gc.disk_warning_threshold,
    };
    let (gc_config_tx, gc_config_rx) = tokio::sync::watch::channel(initial_gc_config);

    // Health-check interval watch
    let (health_interval_tx, health_interval_rx) =
        tokio::sync::watch::channel(config.health.check_interval_secs);

    // Start the GC loop with the watch receiver (hot-reloadable interval)
    storage::gc::start_gc_loop(
        store.clone(),
        gc_config_rx,
        None, // GC metrics not yet wired
    );
    info!("GC loop started (interval hot-reloadable via config sync)");

    // Initialize recovery metrics early (needed for recovery mode detection)
    let recovery_metrics = Arc::new(metrics::recovery::RecoveryMetrics::new());

    // Detect recovery mode (L4: total loss detection)
    let recovery_mode = recovery::detect_recovery_mode(&store, &config.node.node_id);
    match recovery_mode {
        recovery::RecoveryMode::Normal => {
            info!("normal startup — existing state found");
        }
        recovery::RecoveryMode::FullRebuild => {
            info!("recovery mode: full rebuild required — will request state from cluster");
            recovery_metrics.set_recovery_mode(1);
        }
        recovery::RecoveryMode::CorruptionDetected => {
            tracing::warn!("recovery mode: corruption detected — will attempt partial rebuild");
            recovery_metrics.set_recovery_mode(2);
        }
    }

    let runtime = runtime::WasmRuntime::new_with_runtime_config(Some(&config.runtime))
        .expect("Failed to create WasmRuntime");
    info!("Wasm runtime initialized (Cranelift AOT)");

    let bind_addr: IpAddr = "0.0.0.0".parse().unwrap();
    let port_alloc = Arc::new(supervisor::port_alloc::PortAllocator::new(
        bind_addr,
        config.runtime.port_start,
        config.runtime.port_end,
    ));

    let upstream_registry = Arc::new(proxy::upstream::UpstreamRegistry::default());
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());
    let host_router = Arc::new(proxy::router::HostRouter::default());

    let mut bus = match &config.nats.creds_file {
        Some(creds) => messaging::NatsBus::connect_secure(&config.nats.url, creds).await?,
        None => messaging::NatsBus::connect(&config.nats.url).await?,
    };
    bus.set_node_id(config.node.node_id.clone());
    info!(url = %config.nats.url, "NATS connected");

    bus.setup_jetstream().await?;

    // Initialize NATS health tracking for L5 (partition) recovery
    let nats_health = Arc::new(NatsHealth::new());

    // Start NATS health watcher (updates last message timestamp periodically)
    let _nats_watcher_handle =
        NatsHealthWatcher::new((*nats_health).clone(), Duration::from_secs(5)).start();

    recovery::startup_integrity_check(&store, bus.client(), &config.storage).await;

    let (event_tx, event_rx) = mpsc::channel::<messaging::events::Event>(1000);
    // Wire the event receiver to a publisher task that forwards events to NATS
    {
        let bus_for_publisher = bus.clone();
        tokio::spawn(async move {
            messaging::publisher::run_publisher(bus_for_publisher, event_rx).await;
        });
    }

    // Initialize database manager
    let db_config = db_config::DatabaseConfig {
        default_database_url: config.database.default_url.clone(),
        health_check_addr: config.database.pgbouncer_addr.clone(),
        health_check_interval_secs: 30,
        enable_builtin_proxy: config.database.enable_db_proxy,
        builtin_proxy_addr: config.database.db_proxy_addr.clone(),
        builtin_proxy_backend: config.database.db_proxy_backend.clone(),
        builtin_proxy_max_connections: config.database.db_proxy_max_connections,
    };

    let db_manager = db_config::DatabaseManager::new(db_config.clone());
    db_manager.initialize().await?;

    let env_resolver = Arc::new(move |config: &common::types::AppConfig, _host_port: u16| {
        let mut vars = Vec::new();
        for (k, v) in &config.env_vars {
            vars.push((k.clone(), v.clone()));
        }
        // Inject DATABASE_URL if not already provided in env_vars
        if !config.env_vars.contains_key("DATABASE_URL") {
            vars.push((
                "DATABASE_URL".to_string(),
                db_config.default_database_url.clone(),
            ));
        }
        vars
    });

    // Initialize billing collector
    let billing_collector =
        billing::BillingCollector::start(store.clone(), config.node.node_id.clone());
    info!("billing collector started");

    // Optionally start billing export loop
    if let Some(ref export_dir) = config.billing.export_dir {
        let exporter = Arc::new(billing::FileExporter::new(std::path::PathBuf::from(
            export_dir,
        )));
        let interval = Duration::from_secs(config.billing.export_interval_secs);
        billing::start_export_loop(store.clone(), exporter, interval);
        info!(
            dir = export_dir,
            interval = interval.as_secs(),
            "billing export loop started"
        );
    }

    // eBPF monitor initialization is moved to after the backpressure signal
    // and supervisor are created, so the ActionDispatcher can reference them.
    // See below (after line ~535).

    let mut supervisor = supervisor::Supervisor::new(
        store.clone(),
        runtime.clone(),
        port_alloc.clone(),
        upstream_registry.clone(),
        host_router.clone(),
        service_registry.clone(),
        env_resolver,
        event_tx.clone(),
        Some(billing_collector.tx()),
    );

    supervisor.restore_from_storage().await?;
    info!("supervisor state restored from storage");

    // Set the health interval watch before starting the loop.
    // set_health_interval_rx requires &mut Self, but Supervisor is behind Arc.
    // Arc::get_mut only succeeds if there's a single owner. Since supervisor
    // was just created by Supervisor::new (which returns Arc<Self>), we are
    // the sole owner at this point.
    if let Some(sup) = Arc::get_mut(&mut supervisor) {
        sup.set_health_interval_rx(health_interval_rx);
    } else {
        tracing::warn!("could not set health interval watch — supervisor already shared");
    }

    supervisor.clone().start_health_loop();
    supervisor.clone().start_command_loop();

    host_router.load_routes_from_store(&store).await;
    info!("routes loaded from local storage");

    // Initialize secret provider with KEK.
    //
    // Hardened key-source behavior:
    //   - `file`: load the raw 32-byte KEK from `runtime.key_file`
    //   - `env:VAR_NAME`: load the KEK from an environment variable
    //   - `generate`: create an ephemeral KEK for this process only
    //
    // Plaintext KEK persistence in redb is no longer used for normal operation.
    // A legacy persisted KEK can be migrated into `runtime.key_file` when
    // `key_source=file` is configured and the file does not yet exist.
    let kek = load_kek_from_config(&store, &config.runtime)?;
    let artifact_transfer_authority = common::artifact_transfer::ArtifactTransferAuthority::derive(
        &config.node.node_id,
        kek.as_bytes(),
    );
    let secret_provider = Arc::new(secrets::LocalSecretProvider::new(store.clone(), kek));

    // Determine whether this node still needs bootstrap. An empty node that has
    // already completed a valid bootstrap session (including an empty snapshot)
    // should not re-bootstrap forever on restart.
    let bootstrap_completed = store
        .load_meta(handlers::BOOTSTRAP_APPLIED_META_KEY)
        .ok()
        .flatten()
        .is_some();
    let needs_bootstrap = store.list_apps()?.is_empty() && !bootstrap_completed;
    let bootstrap_session = if needs_bootstrap {
        let session_id = common::auth::AuthConfig::generate_token();
        let nonce = common::auth::AuthConfig::generate_token();
        let pending = serde_json::json!({
            "session_id": session_id,
            "nonce": nonce,
            "requested_at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        store
            .save_meta(handlers::BOOTSTRAP_PENDING_META_KEY, &pending.to_string())
            .map_err(anyhow::Error::from)?;
        Some(Arc::new(tokio::sync::Mutex::new(
            handlers::BootstrapSessionState {
                session_id,
                nonce,
                keypair: secrets::BootstrapKeyPair::generate(),
                applied: false,
            },
        )))
    } else {
        None
    };

    let artifact_server_url = build_artifact_server_url(&config.admin)?;
    let proxy_address = build_proxy_advertised_address(&config)?;
    let node_load_table = Arc::new(proxy::node_table::NodeLoadTable::default());
    let artifact_peer_token = if artifact_server_url_is_loopback(&artifact_server_url) {
        None
    } else {
        Some(generate_artifact_peer_token())
    };
    let artifact_peer_token_expires_at_ms = artifact_peer_token
        .as_ref()
        .map(|_| now_unix_ms() + BOOTSTRAP_ARTIFACT_TOKEN_TTL_SECS * 1000);
    if config.admin.advertised_artifact_url.is_some() || config.admin.advertised_host.is_some() {
        info!(artifact_server_url = %artifact_server_url, remote_auth = artifact_peer_token.is_some(), "using configured advertised artifact endpoint");
    } else {
        info!(artifact_server_url = %artifact_server_url, "using local-only default advertised artifact endpoint");
    }

    // ── Gateway Setup (early, so EventDispatcher can reference it) ────
    let oidc_provider = config.gateway.oidc.as_ref().map(|oidc_cfg| {
        let provider = Arc::new(proxy::gateway::oidc::OidcProvider::new(oidc_cfg.clone()));
        provider.clone().start_refresh_loop();
        tracing::info!(issuer = %oidc_cfg.issuer_url, "OIDC provider initialized");
        provider
    });
    let gateway = Arc::new(proxy::gateway::Gateway::new(oidc_provider));

    // ── Embedded DNS Stub (resolves *.internal without external DNS) ──
    let _dns_stub_addr = if config.dns.stub_enabled {
        match dns_stub::start_dns_stub(
            format!("127.0.0.1:{}", config.dns.stub_port)
                .parse()
                .unwrap(),
        )
        .await
        {
            Ok(addr) => {
                info!(%addr, "embedded DNS stub started for *.internal resolution");
                Some(addr)
            }
            Err(e) => {
                warn!(error = %e, "failed to start embedded DNS stub");
                None
            }
        }
    } else {
        None
    };

    let dispatcher = Arc::new(handlers::EventDispatcher {
        supervisor: supervisor.clone(),
        upstream: upstream_registry.clone(),
        host_router: host_router.clone(),
        store: store.clone(),
        runtime: runtime.clone(),
        node_id: config.node.node_id.clone(),
        artifact_server_url: artifact_server_url.clone(),
        upgrade_signing_public_key: config.runtime.upgrade_signing_public_key.clone(),
        secret_provider: secret_provider.clone(),
        bootstrap_session: bootstrap_session.clone(),
        bus: bus.clone(),
        dns_webhook: proxy::dns_webhook::DnsWebhookManager::new(
            config.dns.webhook_url.clone(),
            config.dns.webhook_token.clone(),
        ),
        node_table: node_load_table.clone(),
        gateway: Some(gateway.clone()),
    });

    // Subscribe to control-plane streams with subject-filtered durable consumers.
    // This avoids duplicate delivery when a stream carries multiple event classes.
    let subscription_specs = vec![
        ("DEPLOY", "deploy.>"),
        ("DEPLOY", "routes.>"),
        ("CONTROL", "instance.ready.>"),
        ("CONTROL", "instance.dead.>"),
        ("CONTROL", "secrets.update.>"),
        ("CONTROL", "config.update.>"),
        ("CONTROL", "gateway.config.>"),
        ("NODE", "node.load.>"),
        ("NODE", "cluster.node_joined.>"),
        ("NODE", "cluster.snapshot.>"),
        ("HEALTH", "cluster.health.changed.>"),
        ("HEALTH", "cluster.health.snapshot.>"),
        ("PLATFORM", "platform.upgrade.>"),
        ("PLATFORM", "platform.upgrade_complete.>"),
        ("PLATFORM", "platform.draining.>"),
        ("PLATFORM", "config.hot_reload.>"),
        ("EBPF", "ebpf.pressure.*"),
        ("EBPF", "ebpf.pressure.recovered.*"),
        ("EBPF", "ebpf.security.incident.*"),
    ];

    let sanitize_subject = |subject: &str| {
        subject
            .replace('.', "-")
            .replace('>', "all")
            .replace('*', "one")
    };

    for (stream, subject) in subscription_specs {
        let d = dispatcher.clone();
        let consumer = format!("node-{}-{}", config.node.node_id, sanitize_subject(subject));
        tracing::info!(stream, subject, consumer = %consumer, "subscribing durable consumer");
        bus.subscribe_durable(stream, &consumer, Some(subject), move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    // If this is a fresh node, request state snapshot from cluster
    if needs_bootstrap {
        info!("fresh node detected — requesting state snapshot from cluster");
        if artifact_server_url_is_loopback(&artifact_server_url) {
            warn!(
                artifact_server_url = %artifact_server_url,
                "fresh node is advertising a loopback artifact endpoint; this only works for same-host/local-only setups. Configure admin.advertised_host or admin.advertised_artifact_url for routable multi-node exchange"
            );
        }

        let (bootstrap_session_id, bootstrap_nonce, public_key_bytes) = {
            let state = bootstrap_session
                .as_ref()
                .expect("bootstrap session should exist for fresh node")
                .lock()
                .await;
            (
                state.session_id.clone(),
                state.nonce.clone(),
                state.keypair.public_bytes(),
            )
        };

        let join_event = messaging::events::Event::NodeJoined {
            node_id: config.node.node_id.clone(),
            bootstrap_session_id,
            bootstrap_nonce,
            artifact_server_url: artifact_server_url.clone(),
            artifact_auth_token: artifact_peer_token.clone(),
            public_key_bytes,
            protocol_version: common::protocol::PROTOCOL_VERSION,
            binary_version: common::protocol::BINARY_VERSION.to_string(),
        };

        bus.publish(&join_event).await?;
        info!("NodeJoined event published, waiting for snapshot");

        // Wait for StateSnapshot with a timeout instead of fixed sleep
        let snapshot_subject = format!("cluster.snapshot.{}", config.node.node_id);
        let timeout = tokio::time::Duration::from_secs(30);
        match tokio::time::timeout(timeout, bus.wait_for_event(&snapshot_subject)).await {
            Ok(Ok(_)) => info!("State snapshot received"),
            Ok(Err(e)) => warn!(error = %e, "failed to receive state snapshot"),
            Err(_) => warn!("timed out waiting for state snapshot after 30s"),
        }
    }

    supervisor::scaling::start_load_reporter(
        supervisor.clone(),
        bus.clone(),
        config.node.node_id.clone(),
        proxy_address.clone(),
        5_000_000_000,
    );

    let cold_start_supervisor = supervisor.clone();
    let cold_start = Arc::new(move |app_id: common::types::AppId| {
        let sup = cold_start_supervisor.clone();
        Box::pin(async move { sup.ensure_instance(&app_id).await.ok() })
            as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    // Initialize Prometheus metrics
    let prom_metrics = Arc::new(metrics::exporter::Metrics::new());
    prom_metrics.set_platform_info(
        &config.node.node_id,
        common::protocol::BINARY_VERSION,
        common::protocol::PROTOCOL_VERSION,
    );
    info!(
        node_id = %config.node.node_id,
        binary_version = common::protocol::BINARY_VERSION,
        protocol_version = common::protocol::PROTOCOL_VERSION,
        "platform version metrics initialized"
    );

    // Initialize health check metrics
    let health_metrics = Arc::new(metrics::health_metrics::HealthMetrics::new(
        &prom_metrics.registry,
    ));
    info!("health check metrics registered with Prometheus");

    let backpressure = proxy::backpressure::BackpressureSignal::new();

    // ── Initialize eBPF monitor (kernel-level observability) ────────────
    // The eBPF monitor provides kernel-level monitoring for memory pressure,
    // FD exhaustion, TCP retransmits, disk I/O latency, and syscall anomalies.
    // It uses eBPF programs on Linux >= 5.8 with BTF, or falls back to
    // userspace polling on other platforms.
    let ebpf_config = MonitorConfig::from_ebpf_section(&config.ebpf);
    let ebpf_metrics = Arc::new(EbpfMetrics::new(&prom_metrics.registry));
    let ebpf_callbacks: Arc<dyn EventCallbacks> = Arc::new(NodeEbpfCallbacks {
        backpressure: backpressure.clone(),
        nats_health: nats_health.clone(),
        bus: bus.clone(),
        node_id: config.node.node_id.clone(),
        supervisor_tx: supervisor.command_tx(),
    });
    let ebpf_dispatcher = Arc::new(ActionDispatcher::with_config(
        ebpf_metrics.clone(),
        ebpf_callbacks,
        config.node.node_id.clone(),
        ebpf_config.clone(),
    ));
    let node_pid = std::process::id();
    // Clone metrics and dispatcher for admin API before moving into init()
    let ebpf_metrics_admin = ebpf_metrics.clone();
    let ebpf_dispatcher_admin = ebpf_dispatcher.clone();
    let ebpf_dispatcher_sync = ebpf_dispatcher.clone();
    let _ebpf_handle =
        ebpf_monitor::init(ebpf_config, ebpf_metrics, ebpf_dispatcher, node_pid).await;
    if _ebpf_handle.is_ebpf_active() {
        info!("eBPF monitor initialized with kernel-level monitoring");
    } else {
        info!("eBPF monitor running in userspace fallback mode (5s polling interval)");
    }

    // Wire namespace_map from eBPF monitor to supervisor for TID registration
    if let Some(sup) = Arc::get_mut(&mut supervisor) {
        sup.set_namespace_map(_ebpf_handle.namespace_map.clone());
    }

    // Read initial rate-limit defaults from hot config (may have persisted overrides)
    let initial_hot = hot_config_handle.read().await;
    let default_rate_config = proxy::rate_limiter::RateLimitConfig {
        requests_per_second: initial_hot.rate_limit.default_requests_per_second,
        burst_capacity: initial_hot.rate_limit.default_burst_capacity,
        per_ip_limit: initial_hot.rate_limit.default_per_ip_limit,
    };
    drop(initial_hot);

    // Initialize rate limit metrics (register with the same registry)
    let rate_limit_metrics = Arc::new(proxy::metrics::RateLimitMetrics::new(
        &prom_metrics.registry,
    ));

    let rate_limiter = Arc::new(proxy::rate_limiter::RateLimiter::new(default_rate_config));
    let rate_limiter_sync = rate_limiter.clone();

    // ── Gateway Config Load ───────────────────────────────────────────
    // Gateway was created early so EventDispatcher can reference it.
    // Load persisted configs and API keys into the in-memory cache.
    match store.list_gateway_configs() {
        Ok(configs) => {
            let count = configs.len();
            for (app_id, cfg) in configs {
                gateway.set_route_config(&app_id, cfg).await;
            }
            tracing::info!(count = count, "gateway configs loaded from storage");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load gateway configs from storage");
        }
    }

    // 3b. Load API keys from storage into gateway validators
    match store.list_apps() {
        Ok(app_ids) => {
            for app_id in app_ids {
                match store.load_api_keys(&app_id.0) {
                    Ok(keys) if !keys.is_empty() => {
                        let validator = proxy::gateway::api_key::ApiKeyValidator::new(keys);
                        gateway.set_api_key_validator(&app_id.0, validator).await;
                    }
                    _ => {}
                }
            }
            tracing::info!("api key validators loaded from storage");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load app list for api keys");
        }
    }

    // 4. Setup NATS KV bucket for distributed rate limiting
    let rate_limit_kv = if config.gateway.rate_limit.kv_bucket.is_empty() {
        None
    } else {
        let js = async_nats::jetstream::new(bus.client().clone());
        match js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: config.gateway.rate_limit.kv_bucket.clone(),
                max_age: std::time::Duration::from_secs(10),
                history: 1,
                ..Default::default()
            })
            .await
        {
            Ok(kv) => {
                tracing::info!(bucket = %config.gateway.rate_limit.kv_bucket, "NATS KV bucket created for rate limiting");
                Some(kv)
            }
            Err(e) => {
                tracing::warn!(error = %e, bucket = %config.gateway.rate_limit.kv_bucket, "failed to create NATS KV bucket");
                None
            }
        }
    };

    // 5. Create distributed rate limiters for apps with gateway rate limit configs
    if let Some(ref kv) = rate_limit_kv {
        let gateway_configs = match store.list_gateway_configs() {
            Ok(configs) => configs,
            Err(_) => Vec::new(),
        };
        for (app_id, cfg) in gateway_configs {
            if let Some(ref route_rl) = cfg.rate_limit {
                if route_rl.distributed {
                    let limiter = Arc::new(
                        proxy::gateway::distributed_limiter::DistributedRateLimiter::new(
                            app_id.clone(),
                            config.node.node_id.clone(),
                            proxy::gateway::distributed_limiter::DistributedRateLimitConfig {
                                global_rps: route_rl.requests_per_second,
                                per_node_burst: route_rl.burst_capacity,
                                sync_interval_ms: config.gateway.rate_limit.sync_interval_ms,
                                kv_bucket: config.gateway.rate_limit.kv_bucket.clone(),
                            },
                        ),
                    );
                    limiter.set_kv_store(kv.clone()).await;
                    let limiter_clone = limiter.clone();
                    limiter_clone.start_sync_loop();
                    gateway
                        .distributed_limiters
                        .write()
                        .await
                        .insert(app_id, limiter);
                }
            }
        }
    }

    // ── Internal Mesh Gateway ─────────────────────────────────────────
    // Starts a local Axum proxy for East-West traffic between apps.
    // Listens on a single port (9080). Namespace isolation relies on
    // service discovery: the Supervisor only injects service URLs for
    // same-namespace apps. The gateway port is open to all namespaces.
    let internal_gw = internal_gateway::InternalGateway::new(
        service_registry.clone(),
        rate_limiter.clone(),
        gateway.circuit_breaker.clone(),
        gateway.clone(),
    )
    .with_namespace_map(_ebpf_handle.namespace_map.clone())
    .with_ebpf_active(_ebpf_handle.is_ebpf_active())
    .with_cold_start(cold_start.clone());
    tokio::spawn(async move {
        if let Err(e) = internal_gw.run().await {
            tracing::error!(error = %e, "internal gateway exited");
        }
    });
    info!("internal gateway started for East-West traffic on port 9080");

    let wasm_proxy = proxy::service::WasmProxy {
        router: host_router.clone(),
        upstream: upstream_registry.clone(),
        rate_limiter,
        node_table: node_load_table.clone(),
        cold_start,
        backpressure: backpressure.clone(),
        metrics: Some(rate_limit_metrics),
        gateway: gateway.clone(),
        max_body_size_bytes: 10 * 1024 * 1024, // 10 MB
    };

    let tls = match (&config.proxy.tls_cert, &config.proxy.tls_key) {
        (Some(cert), Some(key)) => Some(proxy::tls::tls_settings(
            std::path::Path::new(cert),
            std::path::Path::new(key),
        )),
        _ => None,
    };

    let proxy_timeouts = proxy::config::ProxyTimeouts::default();
    let proxy_server = proxy::ProxyServer::build(
        wasm_proxy,
        config.proxy.http_port,
        Some(config.proxy.https_port).filter(|&p| p > 0),
        tls,
        proxy_timeouts,
    );

    // Admin API with pgBouncer status endpoint and Prometheus metrics
    let pgbouncer_check_addr = config.database.pgbouncer_addr.clone();
    let db_path_clone = config.storage.db_path.clone();
    let store_gc = store.clone();
    let supervisor_gc = supervisor.clone();
    let supervisor_instances = supervisor.clone();
    let supervisor_kill = supervisor.clone();
    let store_billing = store.clone();
    let host_router_admin = host_router.clone();
    let ebpf_cmd_tx = supervisor.command_tx();

    // ── Health Check System ───────────────────────────────────────────
    let startup_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let app_health_registry = Arc::new(tokio::sync::RwLock::new(
        proxy::health::AppHealthRegistry::new(),
    ));

    let health_state = proxy::health::HealthState {
        node_id: config.node.node_id.clone(),
        nats_health: nats_health.clone(),
        backpressure: Arc::new(backpressure.clone()),
        started_at: std::time::Instant::now(),
        startup_complete: startup_complete.clone(),
        instance_count_provider: supervisor.clone()
            as Arc<dyn proxy::health::InstanceCountProvider + Send + Sync>,
        dependency_checkers: Arc::new(vec![
            Box::new(proxy::health::NatsDependencyChecker::new(
                nats_health.clone(),
            )),
            Box::new(proxy::health::RedbDependencyChecker::new(store.clone())),
            Box::new(proxy::health::DiskDependencyChecker::new(
                config.storage.db_path.clone(),
                config.health.min_disk_free_bytes,
            )),
            Box::new(proxy::health::MemoryDependencyChecker::new(
                config.health.max_memory_bytes,
            )),
        ]),
        app_health_registry: app_health_registry.clone(),
        config: proxy::health::HealthCheckConfig {
            min_disk_free_bytes: config.health.min_disk_free_bytes,
            max_memory_bytes: config.health.max_memory_bytes,
            failure_threshold: config.health.failure_threshold,
            success_threshold: config.health.success_threshold,
            check_interval: std::time::Duration::from_secs(config.health.check_interval_secs),
            check_timeout: std::time::Duration::from_secs(config.health.check_timeout_secs),
        },
    };

    // Wire app health registry into upstream registry
    {
        let upstream_inner = upstream_registry.app_health_registry.write().await;
        // The registry is already wired via clone; no action needed.
        // This block ensures the RwLock type matches.
        let _ = &*upstream_inner;
    }

    let health_router = proxy::health::health_router(health_state.clone());

    // Health event publisher and background loop
    let health_publisher = Arc::new(proxy::health_events::HealthEventPublisher::new(
        bus.clone(),
        config.node.node_id.clone(),
    ));
    let _health_loop_handle = proxy::health_events::start_health_loop(
        Arc::new(health_state.clone()),
        health_publisher.clone(),
    );

    // Start background per-app upstream health checker
    let _upstream_health_handle = {
        let upstream_checker =
            proxy::upstream_health::UpstreamHealthChecker::new(upstream_registry.clone());
        upstream_checker.start()
    };
    info!("upstream health checker started");

    // Spawn a task to periodically update health metrics from the health state
    {
        let hm = health_metrics.clone();
        let hs = Arc::new(health_state.clone());
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;

                // Evaluate dependencies for metrics update
                let mut dependencies = Vec::new();
                for checker in hs.dependency_checkers.iter() {
                    dependencies.push(checker.check());
                }
                dependencies.push(common::health::DependencyHealth {
                    name: "backpressure".to_string(),
                    status: if hs.backpressure.is_accepting() {
                        common::health::DependencyStatus::Healthy
                    } else {
                        common::health::DependencyStatus::Unhealthy
                    },
                    message: if hs.backpressure.is_accepting() {
                        "accepting requests".to_string()
                    } else {
                        "rejecting requests — node at capacity".to_string()
                    },
                    latency_ms: None,
                    last_check: chrono::Utc::now().to_rfc3339(),
                });

                let status = proxy::health::compute_status_for_probe(
                    &dependencies,
                    common::health::ProbeType::Readiness,
                );

                let report = common::health::NodeHealthReport {
                    status,
                    node_id: hs.node_id.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    uptime_secs: hs.started_at.elapsed().as_secs(),
                    startup_complete: hs
                        .startup_complete
                        .load(std::sync::atomic::Ordering::Relaxed),
                    accepting_requests: hs.backpressure.is_accepting(),
                    active_instances: hs.instance_count_provider.active_instance_count(),
                    deployed_apps: hs.instance_count_provider.deployed_app_count(),
                    dependencies,
                    apps: hs.instance_count_provider.app_health_summaries(),
                };

                hm.update_from_report(&report);
            }
        });
    }

    // ── Admin API Authentication Setup ──────────────────────────────────
    // Resolve the effective AuthConfig from: [auth] section > legacy admin.auth_token > defaults.
    // Persisted overrides from redb (token rotations) take the highest priority.
    let mut effective_auth_config: common::auth::AuthConfig = if config.auth.enabled {
        let ac: common::auth::AuthConfig = config.auth.clone().into();
        ac
    } else if config.admin.auth_token.is_some() {
        tracing::info!(
            "using legacy admin.auth_token as write token — \
             consider migrating to the [auth] section for separate read/write tokens"
        );
        common::auth::AuthConfig::from_legacy_token(config.admin.auth_token.as_deref().unwrap())
    } else {
        common::auth::AuthConfig::default()
    };

    // Load persisted auth config overrides from redb (survives restarts)
    match store.load_auth_config() {
        Ok(Some(persisted)) => {
            tracing::info!("loaded persisted auth config from database (overrides TOML values)");
            effective_auth_config = persisted;
        }
        Ok(None) => {
            tracing::debug!("no persisted auth config found — using TOML/CLI values");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to load persisted auth config — falling back to TOML file values"
            );
        }
    }

    // Validate the effective auth config
    if let Err(e) = effective_auth_config.validate() {
        anyhow::bail!("Invalid auth configuration: {}", e);
    }

    let admin_tls_enabled = admin_tls_is_configured(&config);
    proxy::auth_middleware::check_admin_tls_requirement(&effective_auth_config, admin_tls_enabled)
        .map_err(anyhow::Error::msg)?;

    // Check config file permissions (warn if world-readable)
    if let Some(ref config_path) = args.config {
        proxy::auth_middleware::check_config_file_permissions(std::path::Path::new(config_path));
    }

    // Create shared auth state for the middleware
    let auth_config_shared = Arc::new(tokio::sync::RwLock::new(effective_auth_config.clone()));
    let auth_metrics = Arc::new(proxy::auth_middleware::AuthMetrics::new(
        &prom_metrics.registry,
    ));
    let admin_rate_limiter = Arc::new(proxy::auth_middleware::AdminRateLimiter::new(
        effective_auth_config.rate_limit_per_second,
        effective_auth_config.rate_limit_burst,
    ));

    // Audit callback — bridges proxy auth middleware to supervisor audit trail

    let audit_fn: proxy::auth_middleware::AuditCallback = Arc::new(
        move |info: proxy::auth_middleware::AuditInfo| {
            let event_type = if info.status_code >= 400 {
                supervisor::audit::AuditEventType::AuthFailure
            } else {
                supervisor::audit::AuditEventType::AdminApiCall
            };
            let event = supervisor::audit::AuditEvent {
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                node_id: info.node_id.clone(),
                event_type,
                actor: format!("admin:{}", info.token_type),
                app_id: "_platform".to_string(),
                details: serde_json::json!({
                    "path": info.path,
                    "method": info.method,
                    "client_ip": info.client_ip.map(|ip| ip.to_string()).unwrap_or("unknown".to_string()),
                    "status_code": info.status_code,
                }),
            };
            supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &event);
        },
    );

    let auth_state = proxy::auth_middleware::AuthState {
        config: auth_config_shared.clone(),
        metrics: auth_metrics,
        rate_limiter: admin_rate_limiter.clone(),
        audit_fn: Some(audit_fn),
        node_id: config.node.node_id.clone(),
    };

    if effective_auth_config.enabled {
        info!(
            "admin API authentication enabled (rate limit: {}/s per IP, burst: {})",
            effective_auth_config.rate_limit_per_second, effective_auth_config.rate_limit_burst,
        );
    } else {
        info!("admin API authentication disabled — all endpoints accessible without token");
    }

    // Clone for token rotation endpoint
    let rotate_auth_config = auth_config_shared.clone();
    let rotate_store = store.clone();
    let rotate_node_id = config.node.node_id.clone();

    let admin_app = axum::Router::new()
        .merge(health_router)
        .route(
            "/status/pgbouncer",
            axum::routing::get(move || {
                let addr = pgbouncer_check_addr.clone();
                async move {
                    let available = supervisor::db_proxy::check_pgbouncer(&addr).await;
                    let status = if available { "healthy" } else { "unavailable" };
                    axum::Json(serde_json::json!({
                        "status": status,
                        "address": addr,
                        "available": available,
                    }))
                }
            }),
        )
        .route(
            "/admin/instances/{app_id}",
            axum::routing::get({
                let supervisor = supervisor_instances.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let supervisor = supervisor.clone();
                    async move {
                        let app_id = common::types::AppId(app_id);
                        let instances = supervisor.list_instances(&app_id).await;
                        axum::Json(serde_json::json!({
                            "app_id": app_id.0,
                            "instances": instances.iter().map(|id| serde_json::json!({
                                "id": id.0.to_string(),
                            })).collect::<Vec<_>>(),
                            "count": instances.len(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/instances/{app_id}/kill",
            axum::routing::post({
                let supervisor = supervisor_kill.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let supervisor = supervisor.clone();
                    async move {
                        let app_id = common::types::AppId(app_id);
                        match supervisor.kill_all_instances(&app_id).await {
                            Ok(()) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "status": "killed",
                                    "app_id": app_id.0,
                                    "message": "all instances killed"
                                })),
                            ),
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "status": "error",
                                    "app_id": app_id.0,
                                    "message": format!("failed to kill instances: {e}")
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/billing/count",
            axum::routing::get({
                let store = store_billing.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.get_all_billing_records() {
                            Ok(records) => axum::Json(serde_json::json!({
                                "count": records.len() as u64,
                            })),
                            Err(e) => axum::Json(serde_json::json!({
                                "count": 0,
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/billing/verify",
            axum::routing::post({
                let store = store_billing.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.get_all_billing_records() {
                            Ok(records) => match billing::verify_chain(&records) {
                                Ok(count) => (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "valid": true,
                                        "count": count,
                                    })),
                                ),
                                Err(e) => (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "valid": false,
                                        "error": format!("{:?}", e),
                                    })),
                                ),
                            },
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "valid": false,
                                    "error": format!("failed to read records: {:?}", e),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/rebuild",
            axum::routing::post(move || {
                let db_path = db_path_clone.clone();
                async move {
                    tracing::warn!("Admin rebuild requested — quarantining local state for rebuild");
                    match recovery::quarantine_db_file(&db_path, "admin_rebuild") {
                        Ok(quarantined_path) => (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "rebuild_prepared",
                                "message": "Local state quarantined. Restart the node to rebuild from cluster state.",
                                "quarantined_path": quarantined_path.display().to_string()
                            })),
                        ),
                        Err(e) => {
                            tracing::error!(error = %e, "failed to quarantine database for rebuild");
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "status": "error",
                                    "message": format!("failed to quarantine database: {e}")
                                })),
                            )
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gc/force",
            axum::routing::post(move || {
                let store = store_gc.clone();
                let supervisor = supervisor_gc.clone();
                async move {
                    tracing::info!("Forcing immediate GC run");

                    // Force purge undeployed apps with grace period = 0
                    let purged = store.gc_undeployed_apps(0).unwrap_or(0);
                    tracing::info!(apps = purged, "Forced GC: undeployed apps purged");

                    // Only force-kill instances for undeployed apps (apps with no active routes).
                    // Killing instances for still-deployed apps would cause unnecessary disruption.
                    let app_ids = store.list_apps().unwrap_or_default();
                    let routes = store.list_routes().unwrap_or_default();
                    let routed_app_ids: Vec<String> =
                        routes.iter().map(|r| r.app_id.0.clone()).collect();
                    let mut killed_count = 0;

                    for app_id in &app_ids {
                        // Skip apps that still have active routes — they are still deployed
                        if routed_app_ids.contains(&app_id.0) {
                            continue;
                        }
                        let app_id_obj = common::types::AppId(app_id.0.clone());
                        match supervisor.kill_all_instances(&app_id_obj).await {
                            Ok(()) => {
                                killed_count += 1;
                            }
                            Err(e) => {
                                tracing::debug!(app = %app_id.0, error = %e, "No instances to kill");
                            }
                        }
                    }

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "gc_complete",
                            "undeployed_apps_purged": purged,
                            "apps_killed": killed_count,
                        })),
                    )
                }
            }),
        )
        .merge(metrics::exporter::metrics_router(prom_metrics))
        // ── eBPF Monitor Admin Endpoints ──────────────────────────────
        .route(
            "/admin/ebpf/status",
            axum::routing::get(move || {
                let metrics = ebpf_metrics_admin.clone();
                let dispatcher = ebpf_dispatcher_admin.clone();
                async move {
                    let status = ebpf_monitor::MonitorStatus {
                        ebpf_active: metrics.ebpf_active.get() == 1,
                        backpressure_active: dispatcher.is_backpressure_active(),
                        degraded_mode: dispatcher.is_degraded(),
                        pressure_level: dispatcher.last_pressure_level(),
                        oom_kills: metrics.oom_kills.get(),
                        process_exits: metrics.process_exits.get(),
                        tcp_retransmits: metrics.tcp_retransmits.get(),
                        security_violations: metrics.security_violations.get(),
                        events_processed: metrics.events_processed.get(),
                        events_parse_errors: metrics.events_parse_errors.get(),
                        fd_usage_ratio: metrics.get_fd_usage_ratio(),
                        memory_pressure_level: metrics.memory_pressure_level.get(),
                        tcp_connection_count: metrics.tcp_connection_count.get(),
                        fd_count: metrics.fd_count.get(),
                    };
                    axum::Json(status)
                }
            }),
        )
        .route(
            "/admin/ebpf/config",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let cmd_tx = ebpf_cmd_tx.clone();
                async move {
                    let body = body.0;
                    let mut actions = Vec::new();

                    // Action: prune idle instances to free FDs
                    if body.get("prune_idle").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let threshold = body.get("idle_threshold_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(60);
                        if let Err(e) = cmd_tx.try_send(SupervisorCommand::PruneIdleInstances {
                            idle_threshold_secs: threshold,
                        }) {
                            tracing::warn!(error = %e, "Failed to send PruneIdleInstances command");
                        } else {
                            actions.push("prune_idle");
                        }
                    }

                    // Action: kill the largest instance (most memory)
                    if body.get("kill_largest").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let reason = body.get("kill_largest_reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("admin API request")
                            .to_string();
                        if let Err(e) = cmd_tx.try_send(SupervisorCommand::KillLargestInstance {
                            reason,
                        }) {
                            tracing::warn!(error = %e, "Failed to send KillLargestInstance command");
                        } else {
                            actions.push("kill_largest");
                        }
                    }

                    // Threshold updates (logged for future propagation to eBPF programs)
                    if let Some(thresholds) = body.get("thresholds") {
                        tracing::info!(
                            thresholds = ?thresholds,
                            "eBPF threshold update requested (propagation to eBPF programs pending)"
                        );
                        actions.push("threshold_update_logged");
                    }

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            "actions": actions,
                        })),
                    )
                }
            }),
        )
        .route(
            "/admin/routes",
            axum::routing::get({
                let router = host_router_admin.clone();
                move || {
                    let router = router.clone();
                    async move {
                        let routes = router.list_routes().await;
                        axum::Json(serde_json::json!({
                            "routes": routes,
                            "count": routes.len(),
                        }))
                    }
                }
            }),
        )
        // ── Gateway Configuration Endpoints ─────────────────────────────
        .route(
            "/admin/gateway",
            axum::routing::get({
                let store = store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.list_gateway_configs() {
                            Ok(configs) => axum::Json(serde_json::json!({
                                "configs": configs.iter().map(|(app_id, cfg)| serde_json::json!({
                                    "app_id": app_id,
                                    "config": cfg,
                                })).collect::<Vec<_>>(),
                                "count": configs.len(),
                            })),
                            Err(e) => axum::Json(serde_json::json!({
                                "configs": Vec::<serde_json::Value>::new(),
                                "count": 0,
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::get({
                let store = store.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    async move {
                        match store.load_gateway_config(&app_id) {
                            Ok(Some(config)) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "app_id": app_id,
                                    "config": config,
                                })),
                            ),
                            Ok(None) => (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({
                                    "error": "not_found",
                                    "message": format!("no gateway config for {app_id}"),
                                })),
                            ),
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::post({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>,
                      axum::Json(body): axum::Json<common::types::GatewayRouteConfig>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.save_gateway_config(&app_id.0, &body) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        let event = messaging::events::Event::GatewayConfigUpdate {
                            app_id: app_id.clone(),
                            config: body,
                        };
                        if let Err(e) = bus.publish(&event).await {
                            tracing::warn!(error = %e, "failed to publish gateway config update");
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "updated",
                                "app_id": app_id.0,
                            })),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::delete({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.delete_gateway_config(&app_id.0) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        let event = messaging::events::Event::GatewayConfigRemove {
                            app_id: app_id.clone(),
                        };
                        if let Err(e) = bus.publish(&event).await {
                            tracing::warn!(error = %e, "failed to publish gateway config remove");
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "removed",
                                "app_id": app_id.0,
                            })),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist",
            axum::routing::get({
                let gateway = gateway.clone();
                move || {
                    let gateway = gateway.clone();
                    async move {
                        let rules = gateway.list_cross_namespace_rules().await;
                        axum::Json(serde_json::json!({
                            "rules": rules.iter().map(|(s, t)| serde_json::json!({"source": s, "target": t})).collect::<Vec<_>>(),
                            "count": rules.len(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist",
            axum::routing::post({
                let gateway = gateway.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let gateway = gateway.clone();
                    async move {
                        let source = body.get("source").and_then(|v| v.as_str()).unwrap_or("");
                        let target = body.get("target").and_then(|v| v.as_str()).unwrap_or("");
                        if source.is_empty() || target.is_empty() {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"error": "source and target required"})),
                            );
                        }
                        gateway.add_cross_namespace_rule(source, target).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "added"})),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist/{source}/{target}",
            axum::routing::delete({
                let gateway = gateway.clone();
                move |
                    axum::extract::Path((source, target)): axum::extract::Path<(String, String)>| {
                    let gateway = gateway.clone();
                    async move {
                        gateway.remove_cross_namespace_rule(&source, &target).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "removed"})),
                        )
                    }
                }
            }),
        )
        // ── App Management Endpoints ────────────────────────────────────
        .route(
            "/admin/apps",
            axum::routing::get({
                let store = store.clone();
                let supervisor = supervisor_instances.clone();
                move |axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let store = store.clone();
                    let supervisor = supervisor.clone();
                    async move {
                        let namespace = params.get("namespace").cloned().unwrap_or_else(|| "default".to_string());
                        match store.list_apps() {
                            Ok(app_ids) => {
                                let mut apps = Vec::new();
                                for app_id in app_ids {
                                    if app_id.namespace() == namespace {
                                        let instances = supervisor.list_instances(&app_id).await.len() as u64;
                                        apps.push(serde_json::json!({
                                            "id": app_id.0,
                                            "namespace": app_id.namespace(),
                                            "instances": instances,
                                        }));
                                    }
                                }
                                axum::Json(serde_json::json!(apps))
                            }
                            Err(e) => axum::Json(serde_json::json!({
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/apps/{app_id}/manifest",
            axum::routing::get({
                let store = store.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        let config = match store.load_config(&app_id) {
                            Ok(Some(c)) => c,
                            Ok(None) => {
                                return (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({
                                        "error": "not_found",
                                        "message": format!("no config for {}", app_id.0),
                                    })),
                                );
                            }
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    axum::Json(serde_json::json!({
                                        "error": "storage_error",
                                        "message": format!("{e}"),
                                    })),
                                );
                            }
                        };
                        let gateway_config = store.load_gateway_config(&app_id.0).unwrap_or(None);
                        let api_keys = store.load_api_keys(&app_id.0).unwrap_or_default();
                        let manifest = serde_json::json!({
                            "app": {
                                "name": config.id.bare_app_name(),
                                "version": config.id.bare_name().split(':').nth(1).unwrap_or("v1"),
                                "namespace": config.namespace,
                                "wasm_bind_port": config.wasm_bind_port,
                            },
                            "fuel": {
                                "quota": config.fuel_quota.0,
                                "memory_pages": config.memory_limit.0,
                                "max_instances": config.max_instances,
                                "idle_timeout_secs": config.idle_timeout_secs,
                            },
                            "policy": config.policy,
                            "gateway": gateway_config,
                            "env": config.env_vars,
                            "secrets": config.secret_keys,
                            "api_keys": api_keys,
                        });
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(manifest),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/api_keys/{app_id}",
            axum::routing::post({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>,
                      axum::Json(body): axum::Json<Vec<common::types::ApiKeyRecord>>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.save_api_keys(&app_id.0, &body) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        // Publish event so all nodes update their validators
                        let event = messaging::events::Event::GatewayConfigUpdate {
                            app_id: app_id.clone(),
                            config: common::types::GatewayRouteConfig::default(),
                        };
                        let _ = bus.publish(&event).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "updated",
                                "app_id": app_id.0,
                                "key_count": body.len(),
                            })),
                        )
                    }
                }
            }),
        )
        // ── Configuration Management Endpoints ──────────────────────────
        .route(
            "/admin/config",
            axum::routing::get({
                let cold = config.clone();
                let hot = hot_config_handle.clone();
                move || {
                    let cold = cold.clone();
                    let hot = hot.clone();
                    async move {
                        let hot_cfg = hot.read().await;
                        axum::Json(serde_json::json!({
                            "cold": {
                                "node_id": cold.node.node_id,
                                "nats_url": cold.nats.url,
                                "proxy_http_port": cold.proxy.http_port,
                                "proxy_https_port": cold.proxy.https_port,
                                "admin_port": cold.admin.port,
                                "artifact_port": cold.admin.artifact_port,
                                "db_path": cold.storage.db_path.to_string_lossy(),
                                "port_range": format!("{}-{}", cold.runtime.port_start, cold.runtime.port_end),
                                "database_url": cold.database.default_url,
                                "key_source": cold.runtime.key_source,
                            },
                            "hot": {
                                "rate_limit": hot_cfg.rate_limit,
                                "ebpf": hot_cfg.ebpf,
                                "gc": hot_cfg.gc,
                                "health": hot_cfg.health,
                                "logging": hot_cfg.logging,
                            },
                            "hot_reloadable_fields": [
                                "rate_limit.default_requests_per_second",
                                "rate_limit.default_burst_capacity",
                                "rate_limit.default_per_ip_limit",
                                "ebpf.fd_soft_limit",
                                "ebpf.fd_hard_limit",
                                "ebpf.mem_low_threshold_pages",
                                "ebpf.mem_critical_threshold_pages",
                                "ebpf.disk_slow_threshold_ns",
                                "ebpf.tcp_conn_limit_per_pid",
                                "ebpf.syscall_rate_limit",
                                "gc.gc_interval_secs",
                                "gc.disk_warning_threshold",
                                "health.check_interval_secs",
                                "health.default_idle_timeout_secs",
                                "logging.level",
                            ],
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/config",
            axum::routing::patch({
                let hot = hot_config_handle.clone();
                let log_h = log_reload_handle.clone();
                let nbus = bus.clone();
                let nid = config.node.node_id.clone();
                move |body: axum::Json<serde_json::Value>| {
                    let hot = hot.clone();
                    let log_h = log_h.clone();
                    let nbus = nbus.clone();
                    let nid = nid.clone();
                    async move {
                        let raw = body.0;
                        // Build a HotConfigUpdate from the JSON body
                        let update = config::HotConfigUpdate {
                            rate_limit_default_rps: raw.get("rate_limit_default_rps")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            rate_limit_default_burst: raw.get("rate_limit_default_burst")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            rate_limit_default_per_ip: raw.get("rate_limit_default_per_ip")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_fd_soft_limit: raw.get("ebpf_fd_soft_limit")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_fd_hard_limit: raw.get("ebpf_fd_hard_limit")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_mem_low_threshold_pages: raw.get("ebpf_mem_low_threshold_pages")
                                .and_then(|v| v.as_u64()),
                            ebpf_mem_critical_threshold_pages: raw.get("ebpf_mem_critical_threshold_pages")
                                .and_then(|v| v.as_u64()),
                            ebpf_disk_slow_threshold_ns: raw.get("ebpf_disk_slow_threshold_ns")
                                .and_then(|v| v.as_u64()),
                            ebpf_tcp_conn_limit_per_pid: raw.get("ebpf_tcp_conn_limit_per_pid")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_syscall_rate_limit: raw.get("ebpf_syscall_rate_limit")
                                .and_then(|v| v.as_u64()),
                            gc_interval_secs: raw.get("gc_interval_secs")
                                .and_then(|v| v.as_u64()),
                            gc_disk_warning_threshold: raw.get("gc_disk_warning_threshold")
                                .and_then(|v| v.as_f64()),
                            health_check_interval_secs: raw.get("health_check_interval_secs")
                                .and_then(|v| v.as_u64()),
                            health_default_idle_timeout_secs: raw.get("health_default_idle_timeout_secs")
                                .and_then(|v| v.as_u64()),
                            logging_level: raw.get("logging_level")
                                .and_then(|v| v.as_str()).map(|s| s.to_string()),
                        };

                        if update.count_changes() == 0 {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "no_changes",
                                    "message": "No hot-reloadable fields were provided in the request body"
                                })),
                            );
                        }

                        match hot.apply_update(update.clone()).await {
                            Ok(()) => {
                                // If log level changed, apply it to the tracing subscriber
                                if let Some(ref level) = update.logging_level {
                                    if let Err(e) = log_h.update_levels(level) {
                                        tracing::warn!(error = %e, "failed to apply log level change via reload handle");
                                    } else {
                                        tracing::info!(new_level = %level, "log level changed at runtime");
                                    }
                                }

                                // Publish ConfigHotReload event to NATS (informational)
                                let changes_json = serde_json::to_value(&update)
                                    .unwrap_or(serde_json::json!({}));
                                let event = messaging::events::Event::ConfigHotReload {
                                    node_id: nid.clone(),
                                    changes: changes_json,
                                };
                                if let Err(e) = nbus.publish(&event).await {
                                    tracing::warn!(error = %e, "failed to publish ConfigHotReload event");
                                }

                                tracing::info!(
                                    changes = update.count_changes(),
                                    "hot config updated via admin API"
                                );

                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "updated",
                                        "changes_applied": update.count_changes(),
                                    })),
                                )
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "hot config update rejected");
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "validation_failed",
                                        "message": e.to_string(),
                                    })),
                                )
                            }
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/config",
            axum::routing::delete({
                let hot = hot_config_handle.clone();
                move || {
                    let hot = hot.clone();
                    async move {
                        match hot.reset().await {
                            Ok(()) => {
                                tracing::info!("hot config reset to cold defaults via admin API");
                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "reset",
                                        "message": "Hot config reset to startup defaults. Restart to re-read TOML file.",
                                    })),
                                )
                            }
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "reset_failed",
                                    "message": e.to_string(),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        // ── Logging Admin Endpoints ────────────────────────────────────
        .route(
            "/admin/logging/levels",
            axum::routing::get({
                let _log_h = log_reload_handle.clone();
                move || {
                    let _log_h = _log_h.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "message": "Current log levels are managed by the tracing subscriber. \
                                        Use RUST_LOG format for updates.",
                            "hint": "PATCH /admin/logging/levels with {\"directives\": \"debug,supervisor=trace\"}"
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/logging/levels",
            axum::routing::patch({
                let log_h = log_reload_handle.clone();
                let nid = config.node.node_id.clone();
                move |body: axum::Json<serde_json::Value>| {
                    let log_h = log_h.clone();
                    let nid = nid.clone();
                    async move {
                        let directives = match body.0.get("directives").and_then(|v| v.as_str()) {
                            Some(d) => d,
                            None => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "status": "error",
                                        "message": "Missing field 'directives' in request body",
                                    })),
                                );
                            }
                        };

                        match log_h.update_levels(directives) {
                            Ok(()) => {
                                tracing::info!(
                                    config_key = "logging.levels",
                                    new_value = %directives,
                                    node_id = %nid,
                                    "log levels updated via admin API"
                                );
                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "updated",
                                        "directives": directives,
                                    })),
                                )
                            }
                            Err(e) => {
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "status": "error",
                                        "message": format!("Invalid log directives: {}", e),
                                    })),
                                )
                            }
                        }
                    }
                }
            }),
        )
        // ── Token Rotation Endpoint ────────────────────────────────────
        .route(
            "/admin/auth/rotate-token",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let auth_config = rotate_auth_config.clone();
                let store = rotate_store.clone();
                let node_id = rotate_node_id.clone();
                async move {
                    // Parse the request body
                    let req: proxy::auth_middleware::RotateTokenRequest = match serde_json::from_value(body.0) {
                        Ok(r) => r,
                        Err(e) => {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "invalid_request",
                                    "message": format!("Failed to parse request: {}", e)
                                })),
                            );
                        }
                    };

                    // Validate the rotation request
                    let new_token = match proxy::auth_middleware::validate_rotation_request(&req) {
                        Ok(t) => t,
                        Err(e) => {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "validation_failed",
                                    "message": e
                                })),
                            );
                        }
                    };

                    // Apply the rotation
                    let mut config = auth_config.write().await;
                    match req.token_type.as_str() {
                        "read" => {
                            let old = config.read_token.clone();
                            config.read_token = Some(new_token.clone());
                            tracing::warn!(
                                old_prefix = old.map(|t| t[..8.min(t.len())].to_string()).unwrap_or_else(|| "none".to_string()),
                                new_prefix = &new_token[..8.min(new_token.len())],
                                "read token rotated via admin API"
                            );
                        }
                        "write" => {
                            let old = config.write_token.clone();
                            config.write_token = Some(new_token.clone());
                            tracing::warn!(
                                old_prefix = old.map(|t| t[..8.min(t.len())].to_string()).unwrap_or_else(|| "none".to_string()),
                                new_prefix = &new_token[..8.min(new_token.len())],
                                "write token rotated via admin API"
                            );
                        }
                        _ => unreachable!("validate_rotation_request should have caught this"),
                    }

                    // Persist the updated config to redb
                    if let Err(e) = store.save_auth_config(&config) {
                        tracing::error!(error = %e, "failed to persist rotated token to database");
                    }

                    // Audit log the rotation
                    let audit_event = supervisor::audit::AuditEvent {
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        node_id,
                        event_type: supervisor::audit::AuditEventType::TokenRotated,
                        actor: "admin:write_token".to_string(),
                        app_id: "_platform".to_string(),
                        details: serde_json::json!({
                            "token_type": req.token_type,
                        }),
                    };
                    supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &audit_event);

                    drop(config);

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "rotated",
                            "token_type": req.token_type,
                            "new_token": new_token,
                            "warning": "Save this token securely. It will not be shown again.",
                        })),
                    )
                }
            }),
        )
        // ── Auth Middleware Layer ───────────────────────────────────────
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            proxy::auth_middleware::auth_middleware,
        ));

    // ── Config Sync Loop ──────────────────────────────────────────────
    // Periodically reads HotConfigHandle and pushes updates to components
    // that need hot-reloadable parameters (rate limiter, eBPF, GC, health).
    {
        let sync_hot = hot_config_handle.clone();
        let sync_rate_limiter = rate_limiter_sync.clone();
        let sync_ebpf_dispatcher = ebpf_dispatcher_sync.clone();
        let sync_gc_tx = gc_config_tx;
        let sync_health_tx = health_interval_tx;
        let sync_log_handle = log_reload_handle.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                let hot = sync_hot.read().await;

                // 1. Rate limiter
                sync_rate_limiter.update_default_config(proxy::rate_limiter::RateLimitConfig {
                    requests_per_second: hot.rate_limit.default_requests_per_second,
                    burst_capacity: hot.rate_limit.default_burst_capacity,
                    per_ip_limit: hot.rate_limit.default_per_ip_limit,
                });

                // 2. eBPF monitor thresholds
                let new_ebpf_config =
                    ebpf_monitor::MonitorConfig::from_ebpf_section(&common::config::EbpfSection {
                        enabled: hot.ebpf.enabled,
                        fd_soft_limit: hot.ebpf.fd_soft_limit,
                        fd_hard_limit: hot.ebpf.fd_hard_limit,
                        mem_low_threshold_pages: hot.ebpf.mem_low_threshold_pages,
                        mem_critical_threshold_pages: hot.ebpf.mem_critical_threshold_pages,
                        disk_slow_threshold_ns: hot.ebpf.disk_slow_threshold_ns,
                        tcp_conn_limit_per_pid: hot.ebpf.tcp_conn_limit_per_pid,
                        syscall_rate_limit: hot.ebpf.syscall_rate_limit,
                        sampling_period_secs: hot.ebpf.sampling_period_secs,
                        enable_namespace_enforcer: hot.ebpf.enable_namespace_enforcer,
                        gateway_port: hot.ebpf.gateway_port,
                        enable_forged_header_detect: hot.ebpf.enable_forged_header_detect,
                    });
                sync_ebpf_dispatcher.update_thresholds(new_ebpf_config);

                // 3. GC config (interval + disk threshold)
                let new_gc_config = common::gc::GcConfig {
                    artifact_keep_versions: hot.gc.artifact_keep_versions,
                    metrics_retain_days: hot.gc.metrics_retain_days,
                    undeploy_grace_secs: hot.gc.undeploy_grace_secs,
                    gc_interval_secs: hot.gc.gc_interval_secs,
                    disk_warning_threshold: hot.gc.disk_warning_threshold,
                };
                let _ = sync_gc_tx.send(new_gc_config);

                // 4. Health check interval
                let _ = sync_health_tx.send(hot.health.check_interval_secs);

                // 5. Log level (apply via reload handle if changed)
                if let Err(e) = sync_log_handle.update_levels(&hot.logging.level) {
                    tracing::debug!(error = %e, "config sync: log level unchanged or invalid");
                }
            }
        });
        info!("config sync loop started (10s interval, pushes hot-reload updates to components)");
    }

    // ── SIGHUP Handler for Auth Config Reload ──────────────────────────
    // When the operator edits the config file with new tokens and sends
    // SIGHUP, the node reads the updated file and applies the new tokens
    // immediately. Old tokens are invalidated as soon as the new config
    // is loaded into the RwLock.
    setup_sighup_handler(auth_config_shared.clone(), args.config.clone());

    // ── Periodic Rate Limiter Pruning ──────────────────────────────────
    // Prune stale IP buckets every 60 seconds to prevent memory leaks
    // on long-running nodes with many unique client IPs.
    {
        let prune_limiter = admin_rate_limiter.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                prune_limiter.prune_stale(Duration::from_secs(300)); // 5-minute idle threshold
            }
        });
    }

    let admin_addr = bind_socket_address(&config.admin.bind_address, config.admin.port)?;
    let admin_tls = admin_tls_material(&config);
    let (admin_tls_cert, admin_tls_key) = match admin_tls {
        Some((cert, key)) => (Some(cert), Some(key)),
        None => (None, None),
    };
    tokio::spawn(async move {
        if let Err(e) = serve_admin_app(admin_addr, admin_app, admin_tls_cert, admin_tls_key).await
        {
            tracing::error!(error = %e, "admin API server failed");
        }
    });

    let mut artifact_peer_tokens = Vec::new();
    if let Some(token) = artifact_peer_token.clone() {
        artifact_peer_tokens.push(storage::artifact_server::ArtifactPeerTokenConfig::new(
            token,
            artifact_peer_token_expires_at_ms,
            false,
            true,
        ));
    }
    if let Some(token) = effective_auth_config.write_token.clone() {
        if !artifact_peer_tokens
            .iter()
            .any(|existing| existing.token == token)
        {
            artifact_peer_tokens.push(storage::artifact_server::ArtifactPeerTokenConfig::new(
                token, None, true, true,
            ));
        }
    }
    let artifact_app = storage::artifact_server::artifact_router(
        store.clone(),
        artifact_peer_tokens,
        Some(artifact_transfer_authority.clone()),
    );
    let artifact_addr = bind_socket_address(
        &config.admin.artifact_bind_address,
        config.admin.artifact_port,
    )?;
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr)
            .await
            .expect("artifact server bind failed");
        info!(addr = %artifact_addr, "artifact server listening");
        axum::serve(
            listener,
            artifact_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Signal that startup is complete — all probes are now active
    startup_complete.store(true, std::sync::atomic::Ordering::Relaxed);
    info!(node_id = %config.node.node_id, "node startup complete — all probes active");

    info!(
        http = config.proxy.http_port,
        https = config.proxy.https_port,
        admin = config.admin.port,
        artifact = config.admin.artifact_port,
        "node fully started"
    );

    // Run Pingora in background to allow graceful shutdown setup
    std::thread::spawn(move || {
        proxy_server.run();
    });

    // Wait for shutdown signal (SIGTERM / Ctrl-C)
    // On Linux/WSL, `systemctl stop` sends SIGTERM, so we must handle it.
    // We race both signals — whichever fires first triggers graceful drain.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received — gracefully shutting down all instances"),
            _ = sigint.recv() => info!("SIGINT (Ctrl-C) received — gracefully shutting down all instances"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.unwrap();
        info!("Ctrl-C received — gracefully shutting down all instances");
    }

    // Gracefully shutdown all instances with timeout
    let shutdown_timeout = std::time::Duration::from_secs(30);
    supervisor.shutdown_all(shutdown_timeout).await;

    info!("All instances stopped — exiting");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::{
        admin_tls_is_configured, admin_tls_material, artifact_server_url_is_loopback,
        bind_socket_address, build_artifact_server_url, build_proxy_advertised_address,
        load_kek_from_config, load_kek_from_env_spec, serve_admin_app,
    };
    use common::config::{AdminSection, NodeConfig, ProxySection, RuntimeSection};
    use storage::Store;
    use tempfile::{NamedTempFile, TempDir};

    const TEST_ADMIN_TLS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDJTCCAg2gAwIBAgIUAt2GkIIjTn/cu46520UjbQSS8FowDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUxOTIxNTg0MFoXDTI2MDUy
MDIxNTg0MFowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEA8lREjhEhJuV4ZePiYZZkXBpyqx+PypsyReF4J/PuXS2r
CCQtH557/sq7StoHWp0r1Qt7gzxyXd9A1UeVxjzj+GcUjiDdYcx/CcPdt+eUWm1v
IeIO6OZAiyADv89P1hK6L713gA9wbiNkmiGL+02u8/B6VAuCsZvgjfmT1R23K45V
/ofTkKEB7+HKrp8HBiKv0zENL8/+W2dRFFaPWIKNhTx1S71BE7dHhcr5t2zBYiyM
zwKEIvfz35Cby8DJLhKwLB9lAGeOn9b2VgqtUQIphp0FlwxdbK5MWJWd3Ogc0tb4
szvzNso5osiFrtCfgv7RroWMx0Mjzzd6RhGoF2SeqwIDAQABo28wbTAdBgNVHQ4E
FgQUOOyJajcQI9xXHGfGO3tYSWOtGoswHwYDVR0jBBgwFoAUOOyJajcQI9xXHGfG
O3tYSWOtGoswDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARhwR/AAABgglsb2Nh
bGhvc3QwDQYJKoZIhvcNAQELBQADggEBAKwa3TXl7GWPoAOUErZwcExLzRBQuVji
mg11BI93QXSBtaD09GMeqx6D3y4j16gZLd5wZHBD6Whff5nm38WI1jyrKFwnNWI6
Nw205MZhbXmKxiROfLEFYIR2MwUTl5Ma6xR0szhEHYYSgLYlbS8Bobs5Z1wO0+Oy
khANpI5vxX9Ih85WYicQ1wL45L3iKx+E6HRBJBHJ71/d8s942lpGzyPyrX0j1orc
kbG/g6epb8tsUaLWYET9e8JkFaOxiZYy1DT2e1H//a2li5yox30JEgDDhmrRtlo/
cuA1MNK5uKiOJs4TZH38Cx7B2vlun/ZHEqCHwqb++CbSAhz7RBj+U10=
-----END CERTIFICATE-----
"#;

    const TEST_ADMIN_TLS_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDyVESOESEm5Xhl
4+JhlmRcGnKrH4/KmzJF4Xgn8+5dLasIJC0fnnv+yrtK2gdanSvVC3uDPHJd30DV
R5XGPOP4ZxSOIN1hzH8Jw92355RabW8h4g7o5kCLIAO/z0/WErovvXeAD3BuI2Sa
IYv7Ta7z8HpUC4Kxm+CN+ZPVHbcrjlX+h9OQoQHv4cqunwcGIq/TMQ0vz/5bZ1EU
Vo9Ygo2FPHVLvUETt0eFyvm3bMFiLIzPAoQi9/PfkJvLwMkuErAsH2UAZ46f1vZW
Cq1RAimGnQWXDF1srkxYlZ3c6BzS1vizO/M2yjmiyIWu0J+C/tGuhYzHQyPPN3pG
EagXZJ6rAgMBAAECggEAIlhYKQx7duBSCprcRHmEutsSwnckMZKCcw4MMhlsAK/O
zEYYUSFssIV6OxcgsLKS+kx40nZYPT69mRzeuOx7YQL3ElfNGKXboX4tp/l9+L0G
4bYA5/huUGmWrnJK/evEkKyZScCmbi28/e1gQhtV/wPnyo6hFNwjXOvxDGT8R4NL
yanKMN+Dl0RqVPdA4tsucPBrVwOqSEjPVIn8NML8HbwbNyVrquV7Isc/4EP6zgjw
1QQBlCiHhAhkY9eGpthaQ85o7BVkGEHqUgdN8ysN56+hXorVxBpistTsTc/8mEy5
r8sMEOE4qx0OP2CPHqGyE0FSAmIGNNCJSKWi8L30uQKBgQD5YrnnV0k4v9dOV3D9
7MkHItnzMqBehlQAhMHyZ2S9HiLO24yoKlFvVEubwA/0gazVHzKaKdZIgeX662Av
suN2m681JLQqbM5ewRNyRde58r63krWuMNgAupEIb2x6piQN6PQtSY79zXAb1UyO
7scafaUjV61OZs/oM3EwuU7xOQKBgQD4waEyAP0nFIHkDkPTmaUHrKMbYfZp0/hw
iSZubQoASKqw6xEPFZn9LEqqjslR1KQS+EnFq7zDkAztPc+yrOsJN8iYoaKL9zna
7bx1HYVwrGWLfKZ+GCCwGTnQX8NrJ7AQoX9ajRrHhFLgpy5dMXVA5o+0wR+bwTaM
+5MDnWUjAwKBgEfMOKF168q+0InpespgRXAchIsT5D/ShJSxo/TZ95LK/lJ3uwMf
S9q1dh8dKHrIaq3hEXx41wyA+WlIIqUY54vaPpMaQhSExtVY2PRpTzZlwKqxPkUs
IsPy8pZvHdghxPeMPeBb8SL45nHc8vGjpQbnbYfDUk3kI69CQDA66ZNhAoGAf7tN
flOrqgmJuQTqJxlZ+FrZVhIzaZwCkiaaqVEsNYEaxMWveMNq0umPXYz8Kxy5M1Ry
7SGGSBULzjZTFDheZ9lRE67LvHsyJgy1HJ4QCw87BSj4hP72qfYKDclemwNCEQgc
UO7rtU9pDxpJYGkpAC5j1Djmdh/8VuBHWS/U4ukCgYEA6KDtUgXS2ztjuXzXEQjF
PBbvIkQLj/6muZ4ZgXThPwign1/5ih97ZBikWmmYB+zPme1gCj7otOMC9E6gxLmq
nWSPzrabXM5Z5hatTBeVxCQBoFL/hUTvOEqHXQuWtpqwHZ5PnTdsVk31sFJA0Vl3
uoKQp7o8ET+CcFRg9vEG/uA=
-----END PRIVATE KEY-----
"#;

    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_bind_socket_address_formats_ipv6() {
        let addr = bind_socket_address("::1", 9090).unwrap();
        assert_eq!(addr, "[::1]:9090");
    }

    #[test]
    fn test_admin_tls_is_configured_requires_cert_and_key() {
        let mut config = NodeConfig::default();
        assert!(!admin_tls_is_configured(&config));

        config.proxy = ProxySection {
            tls_cert: Some("/tmp/admin.crt".to_string()),
            tls_key: Some("/tmp/admin.key".to_string()),
            ..ProxySection::default()
        };
        assert!(admin_tls_is_configured(&config));
    }

    #[test]
    fn test_admin_tls_material_prefers_dedicated_admin_cert() {
        let mut config = NodeConfig::default();
        config.proxy = ProxySection {
            tls_cert: Some("/tmp/proxy.crt".to_string()),
            tls_key: Some("/tmp/proxy.key".to_string()),
            ..ProxySection::default()
        };
        config.admin.tls_cert = Some("/tmp/admin.crt".to_string());
        config.admin.tls_key = Some("/tmp/admin.key".to_string());

        let material = admin_tls_material(&config).expect("admin TLS material should exist");
        assert_eq!(material.0, "/tmp/admin.crt");
        assert_eq!(material.1, "/tmp/admin.key");
    }

    fn install_test_rustls_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    fn test_admin_app() -> axum::Router {
        axum::Router::new().route(
            "/ping",
            axum::routing::get(|| async { axum::Json(serde_json::json!({ "ok": true })) }),
        )
    }

    #[tokio::test]
    async fn test_serve_admin_app_tls_accepts_https_requests() {
        install_test_rustls_provider();

        let temp_dir = TempDir::new().unwrap();
        let cert_path = temp_dir.path().join("admin.crt");
        let key_path = temp_dir.path().join("admin.key");
        std::fs::write(&cert_path, TEST_ADMIN_TLS_CERT_PEM).unwrap();
        std::fs::write(&key_path, TEST_ADMIN_TLS_KEY_PEM).unwrap();

        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe_listener.local_addr().unwrap().port();
        drop(probe_listener);

        let handle = tokio::spawn(serve_admin_app(
            format!("127.0.0.1:{port}"),
            test_admin_app(),
            Some(cert_path.to_string_lossy().to_string()),
            Some(key_path.to_string_lossy().to_string()),
        ));

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let mut response = None;
        for _ in 0..20 {
            match client
                .get(format!("https://127.0.0.1:{port}/ping"))
                .send()
                .await
            {
                Ok(resp) => {
                    response = Some(resp);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }

        let response = response.expect("admin HTTPS listener did not respond in time");
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["ok"], true);

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_serve_admin_app_tls_rejects_missing_cert_file() {
        install_test_rustls_provider();

        let err = serve_admin_app(
            "127.0.0.1:0".to_string(),
            test_admin_app(),
            Some("/tmp/does-not-exist-admin.crt".to_string()),
            Some("/tmp/does-not-exist-admin.key".to_string()),
        )
        .await
        .expect_err("missing TLS files should fail");

        assert!(err.to_string().contains("admin TLS config error"));
    }

    #[tokio::test]
    async fn test_serve_admin_app_tls_rejects_invalid_pem_contents() {
        install_test_rustls_provider();

        let temp_dir = TempDir::new().unwrap();
        let cert_path = temp_dir.path().join("bad-admin.crt");
        let key_path = temp_dir.path().join("bad-admin.key");
        std::fs::write(&cert_path, b"not a cert").unwrap();
        std::fs::write(&key_path, b"not a key").unwrap();

        let err = serve_admin_app(
            "127.0.0.1:0".to_string(),
            test_admin_app(),
            Some(cert_path.to_string_lossy().to_string()),
            Some(key_path.to_string_lossy().to_string()),
        )
        .await
        .expect_err("invalid TLS PEM should fail");

        assert!(err.to_string().contains("admin TLS config error"));
    }

    #[test]
    fn test_build_artifact_server_url_defaults_to_loopback() {
        let admin = AdminSection::default();
        let url = build_artifact_server_url(&admin).unwrap();
        assert_eq!(url, "http://127.0.0.1:9091");
        assert!(artifact_server_url_is_loopback(&url));
    }

    #[test]
    fn test_build_artifact_server_url_from_advertised_host() {
        let admin = AdminSection {
            advertised_host: Some("node-1.internal".to_string()),
            ..AdminSection::default()
        };
        let url = build_artifact_server_url(&admin).unwrap();
        assert_eq!(url, "http://node-1.internal:9091");
        assert!(!artifact_server_url_is_loopback(&url));
    }

    #[test]
    fn test_build_artifact_server_url_from_explicit_url() {
        let admin = AdminSection {
            advertised_artifact_url: Some("https://artifacts.node-1.internal/base/".to_string()),
            ..AdminSection::default()
        };
        let url = build_artifact_server_url(&admin).unwrap();
        assert_eq!(url, "https://artifacts.node-1.internal/base");
        assert!(!artifact_server_url_is_loopback(&url));
    }

    #[test]
    fn test_build_proxy_advertised_address_defaults_to_loopback() {
        let config = NodeConfig::default();
        let addr = build_proxy_advertised_address(&config).unwrap();
        assert_eq!(addr, "127.0.0.1:8080");
    }

    #[test]
    fn test_build_proxy_advertised_address_uses_advertised_host() {
        let mut config = NodeConfig::default();
        config.admin.advertised_host = Some("node-1.internal".to_string());
        config.proxy.http_port = 18080;

        let addr = build_proxy_advertised_address(&config).unwrap();
        assert_eq!(addr, "node-1.internal:18080");
    }

    #[test]
    fn test_load_kek_from_env_spec_hex() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let var_name = "WASM_NODE_TEST_KEK_HEX";
        let value = "11".repeat(32);
        std::env::set_var(var_name, &value);

        let key = load_kek_from_env_spec(&format!("env:{var_name}")).unwrap();
        assert_eq!(key.as_bytes(), &[0x11; 32]);

        std::env::remove_var(var_name);
    }

    #[test]
    fn test_load_kek_from_env_spec_raw_32_bytes() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let var_name = "WASM_NODE_TEST_KEK_RAW";
        let value = "A".repeat(32);
        std::env::set_var(var_name, &value);

        let key = load_kek_from_env_spec(&format!("env:{var_name}")).unwrap();
        assert_eq!(key.as_bytes(), value.as_bytes());

        std::env::remove_var(var_name);
    }

    #[test]
    fn test_file_key_source_initializes_sealed_kek_from_key_file() {
        let temp_db = NamedTempFile::new().unwrap();
        let store = Store::open(temp_db.path()).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("master.key");
        let seal_key = [0x22u8; 32];
        std::fs::write(&key_path, seal_key).unwrap();

        let runtime = RuntimeSection {
            key_source: "file".to_string(),
            key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        };

        let key = load_kek_from_config(&store, &runtime).unwrap();
        assert_eq!(key.as_bytes(), &seal_key);

        let persisted = store.load_kek().unwrap().unwrap();
        assert_ne!(persisted, seal_key.to_vec());
        assert!(persisted.len() > 32);
    }

    #[test]
    fn test_file_key_source_reloads_existing_sealed_kek() {
        let temp_db = NamedTempFile::new().unwrap();
        let store = Store::open(temp_db.path()).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("master.key");
        let seal_key = [0x44u8; 32];
        std::fs::write(&key_path, seal_key).unwrap();

        let runtime = RuntimeSection {
            key_source: "file".to_string(),
            key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        };

        let first = load_kek_from_config(&store, &runtime).unwrap();
        let second = load_kek_from_config(&store, &runtime).unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());
    }

    #[test]
    fn test_file_key_source_migrates_legacy_plaintext_db_kek_into_sealed_blob() {
        let temp_db = NamedTempFile::new().unwrap();
        let store = Store::open(temp_db.path()).unwrap();
        let legacy = [0x55u8; 32];
        store.save_kek(&legacy).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("master.key");
        std::fs::write(&key_path, [0x66u8; 32]).unwrap();
        let runtime = RuntimeSection {
            key_source: "file".to_string(),
            key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        };

        let key = load_kek_from_config(&store, &runtime).unwrap();
        assert_eq!(key.as_bytes(), &legacy);
        let persisted = store.load_kek().unwrap().unwrap();
        assert_ne!(persisted, legacy.to_vec());
        assert!(persisted.len() > 32);
    }

    #[test]
    fn test_wrong_file_seal_key_rejects_sealed_kek() {
        let temp_db = NamedTempFile::new().unwrap();
        let store = Store::open(temp_db.path()).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("master.key");
        std::fs::write(&key_path, [0x77u8; 32]).unwrap();
        let runtime = RuntimeSection {
            key_source: "file".to_string(),
            key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let _ = load_kek_from_config(&store, &runtime).unwrap();

        std::fs::write(&key_path, [0x88u8; 32]).unwrap();
        assert!(load_kek_from_config(&store, &runtime).is_err());
    }

    #[test]
    fn test_generate_key_source_rejects_persisted_kek() {
        let temp_db = NamedTempFile::new().unwrap();
        let store = Store::open(temp_db.path()).unwrap();
        store.save_kek(&[0x33u8; 48]).unwrap();

        let runtime = RuntimeSection {
            key_source: "generate".to_string(),
            ..Default::default()
        };

        let err = match load_kek_from_config(&store, &runtime) {
            Ok(_) => panic!("expected persisted KEK rejection"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("persisted KEK detected"));
    }
}

/// Create the admin auth middleware closure.
///
/// - If `token` is `None`, all requests pass through (auth disabled).
/// - Health-check paths (`/health`, `/_platform/health`, `/status/`) are always allowed
///   so that load-balancers can probe the node without credentials.
/// - Otherwise the `Authorization` header must be `Bearer <token>`.
/// Set up a SIGHUP handler that reloads auth configuration from the config file.
///
/// When the operator edits the config file with new tokens and sends SIGHUP,
/// the node reads the updated file and applies the new tokens immediately.
/// Old tokens are invalidated as soon as the new config is loaded.
fn setup_sighup_handler(
    auth_config: Arc<tokio::sync::RwLock<common::auth::AuthConfig>>,
    config_path: Option<String>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        tokio::spawn(async move {
            let mut stream = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to install SIGHUP handler");
                    return;
                }
            };
            loop {
                stream.recv().await;

                if let Some(ref path) = config_path {
                    tracing::info!("SIGHUP received — reloading auth config from file");

                    match std::fs::read_to_string(path) {
                        Ok(content) => {
                            match toml::from_str::<common::config::NodeConfig>(&content) {
                                Ok(new_config) => {
                                    let new_auth: common::auth::AuthConfig =
                                        new_config.auth.clone().into();
                                    if let Err(e) = new_auth.validate() {
                                        tracing::error!(
                                            error = %e,
                                            "auth config in file is invalid — keeping current config"
                                        );
                                    } else {
                                        let mut auth = auth_config.write().await;
                                        *auth = new_auth;
                                        tracing::info!("auth config reloaded from file");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "failed to parse config file on SIGHUP reload"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                path = %path,
                                "failed to read config file on SIGHUP reload"
                            );
                        }
                    }
                } else {
                    tracing::warn!("SIGHUP received but no config file path — cannot reload auth");
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        // SIGHUP is not available on non-Unix platforms
        let _ = (auth_config, config_path);
    }
}
