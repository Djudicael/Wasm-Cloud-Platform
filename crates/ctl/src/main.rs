// crates/ctl/src/main.rs
use clap::{Parser, Subcommand};

mod cmds {
    pub mod billing;
    pub mod deploy;
    pub mod gc;
    pub mod list;
    pub mod logs;
    pub mod node;
    pub mod platform;
    pub mod policy;
    pub mod routes;
    pub mod secrets;
    pub mod status;
}

#[derive(Parser)]
#[command(name = "wasm-ctl", about = "Wasm Cloud Platform CLI", version)]
struct Cli {
    #[arg(
        long,
        env = "WASM_CTL_NATS_URL",
        default_value = "nats://127.0.0.1:4222"
    )]
    nats_url: String,

    #[arg(
        long,
        env = "WASM_CTL_NODE_API",
        default_value = "http://127.0.0.1:9090"
    )]
    node_api: String,

    #[arg(long, env = "WASM_CTL_NATS_CREDS")]
    nats_creds: Option<String>,

    /// Bearer token for admin API authentication.
    /// Can also be set via WASM_CTL_AUTH_TOKEN environment variable,
    /// or in ~/.wasm-ctl/config.toml under [auth] token = "...".
    #[arg(long, env = "WASM_CTL_AUTH_TOKEN")]
    auth_token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Deploy or update a Wasm application
    Deploy(cmds::deploy::DeployArgs),
    /// Remove a deployed application
    Remove { app_id: String },
    /// List all deployed applications
    List,
    /// Show running instances across the cluster
    Instances,
    /// Manage HTTP routes
    Routes(cmds::routes::RoutesArgs),
    /// Manage application secrets
    Secrets(cmds::secrets::SecretsArgs),
    /// Stream logs from a running application
    Logs { app_id: String },
    /// Show cluster health status
    Status,
    /// Platform binary management and upgrades
    Platform(cmds::platform::PlatformArgs),
    /// Garbage collection management
    Gc(cmds::gc::GcArgs),
    /// Node-level operations (health check, rebuild)
    Node {
        #[arg(long, help = "Target node ID (default: local node)")]
        target: Option<String>,
        #[command(subcommand)]
        action: NodeAction,
    },
    /// Cluster-level health and operations
    Cluster,
    /// Billing and fuel accounting
    Billing {
        #[arg(long, default_value = "/tmp/wasm-node/state.redb")]
        store_path: String,
        #[command(subcommand)]
        action: BillingAction,
    },
    /// WASI policy enforcement: view policies, violations, and profiles
    Policy {
        #[command(subcommand)]
        action: cmds::policy::PolicyCommand,
    },
}

#[derive(Subcommand)]
enum NodeAction {
    /// Check node health status
    Health,
    /// Force a full node rebuild from cluster state
    Rebuild,
    /// Show eBPF kernel-level monitor status
    EbpfStatus,
    /// Send commands to the eBPF monitor (prune idle instances, kill largest)
    EbpfConfig {
        /// Prune idle instances to free file descriptors
        #[arg(long)]
        prune_idle: bool,
        /// Idle threshold in seconds for pruning (default: 60)
        #[arg(long, default_value = "60")]
        idle_threshold_secs: u64,
        /// Kill the largest instance (most memory) for pressure recovery
        #[arg(long)]
        kill_largest: bool,
        /// Reason for killing the largest instance
        #[arg(long, default_value = "cli request")]
        kill_largest_reason: String,
    },
    /// View or update hot-reloadable configuration
    Config {
        /// Set a hot-reloadable parameter (key=value). Repeatable.
        /// e.g. --set rate_limit_default_rps=5000 --set logging_level=debug
        #[arg(long)]
        set: Vec<String>,
        /// Reset hot config to startup defaults
        #[arg(long)]
        reset: bool,
        /// Output in JSON format
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BillingAction {
    /// Generate a billing report for a tenant
    Report {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        start_ms: u64,
        #[arg(long)]
        end_ms: u64,
    },
    /// Verify billing chain integrity
    Verify,
    /// View billing records
    Records {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long)]
        last: Option<usize>,
    },
    /// Export billing records to a file
    Export {
        #[arg(long)]
        output: String,
    },
}

impl NodeAction {
    pub async fn run(&self, node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
        match self {
            NodeAction::Health => cmds::node::health(node_api, http).await,
            NodeAction::Rebuild => cmds::node::rebuild(node_api, http).await,
            NodeAction::EbpfStatus => {
                let url = format!("{}/admin/ebpf/status", node_api);
                let resp = http.get(&url).send().await?;
                if resp.status().is_success() {
                    let body: serde_json::Value = resp.json().await?;
                    println!("eBPF Monitor Status");
                    println!("===================");
                    println!(
                        "  Mode:              {}",
                        if body["ebpf_active"].as_bool().unwrap_or(false) {
                            "eBPF (kernel)"
                        } else {
                            "Userspace fallback"
                        }
                    );
                    println!(
                        "  Backpressure:      {}",
                        if body["backpressure_active"].as_bool().unwrap_or(false) {
                            "ACTIVE (rejecting)"
                        } else {
                            "normal"
                        }
                    );
                    println!(
                        "  Degraded Mode:     {}",
                        if body["degraded_mode"].as_bool().unwrap_or(false) {
                            "YES (slow I/O)"
                        } else {
                            "no"
                        }
                    );
                    println!(
                        "  Pressure Level:    {}",
                        match body["pressure_level"].as_u64().unwrap_or(0) {
                            0 => "none",
                            1 => "low",
                            2 => "medium",
                            3 => "critical",
                            _ => "unknown",
                        }
                    );
                    println!();
                    println!("  OOM Kills:         {}", body["oom_kills"]);
                    println!("  Process Exits:     {}", body["process_exits"]);
                    println!("  TCP Retransmits:   {}", body["tcp_retransmits"]);
                    println!("  Security Violations: {}", body["security_violations"]);
                    println!();
                    println!("  Events Processed:  {}", body["events_processed"]);
                    println!("  Parse Errors:      {}", body["events_parse_errors"]);
                    println!(
                        "  FD Usage Ratio:    {:.2}",
                        body["fd_usage_ratio"].as_f64().unwrap_or(0.0)
                    );
                    println!("  Memory Pressure:   {}", body["memory_pressure_level"]);
                    println!("  TCP Connections:   {}", body["tcp_connection_count"]);
                    println!("  Open FDs:          {}", body["fd_count"]);
                } else {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to get eBPF status: {} {}", status, text);
                }
                Ok(())
            }
            NodeAction::EbpfConfig {
                prune_idle,
                idle_threshold_secs,
                kill_largest,
                kill_largest_reason,
            } => {
                let url = format!("{}/admin/ebpf/config", node_api);
                let mut body = serde_json::json!({});
                if *prune_idle {
                    body["prune_idle"] = serde_json::json!(true);
                    body["idle_threshold_secs"] = serde_json::json!(idle_threshold_secs);
                }
                if *kill_largest {
                    body["kill_largest"] = serde_json::json!(true);
                    body["kill_largest_reason"] = serde_json::json!(kill_largest_reason);
                }
                if body.as_object().map_or(true, |o| o.is_empty()) {
                    println!("No actions specified. Use --prune_idle or --kill_largest.");
                    return Ok(());
                }
                let resp = http.post(&url).json(&body).send().await?;
                if resp.status().is_success() {
                    let result: serde_json::Value = resp.json().await?;
                    println!("eBPF config commands accepted: {:?}", result["actions"]);
                } else {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to send eBPF config: {} {}", status, text);
                }
                Ok(())
            }
            NodeAction::Config { set, reset, json } => {
                let config_url = format!("{}/admin/config", node_api);

                // Reset takes priority
                if *reset {
                    let resp = http.delete(&config_url).send().await?;
                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        println!("✅ Hot config reset to startup defaults.");
                        if !json {
                            println!();
                            println!("  Status:   {}", result["status"]);
                            println!("  Message:  {}", result["message"]);
                        } else {
                            println!("{}", serde_json::to_string_pretty(&result).unwrap());
                        }
                    } else {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        anyhow::bail!("Failed to reset config: {} {}", status, text);
                    }
                    return Ok(());
                }

                // Apply set overrides
                if !set.is_empty() {
                    let mut body = serde_json::json!({});
                    for pair in set {
                        let (key, value) = pair.split_once('=').ok_or_else(|| {
                            anyhow::anyhow!("Invalid --set format: '{}'. Expected key=value", pair)
                        })?;
                        // Try to parse as number first, then fall back to string
                        if let Ok(u) = value.parse::<u64>() {
                            body[key] = serde_json::json!(u);
                        } else if let Ok(f) = value.parse::<f64>() {
                            body[key] = serde_json::json!(f);
                        } else {
                            body[key] = serde_json::json!(value);
                        }
                    }
                    let resp = http.patch(&config_url).json(&body).send().await?;
                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        println!(
                            "✅ Hot config updated ({} field(s) changed).",
                            result["changes_applied"]
                        );
                    } else {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        anyhow::bail!("Failed to update config: {} {}", status, text);
                    }
                    // After setting, fall through to show the current config
                }

                // Show current config
                let resp = http.get(&config_url).send().await?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Failed to get config: {} {}", status, text);
                }
                let result: serde_json::Value = resp.json().await?;

                if *json {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap());
                } else {
                    println!("📋 Effective Configuration");
                    println!("=========================");
                    println!();
                    println!("Cold (startup) config:");
                    let cold = &result["cold"];
                    println!("  node_id:           {}", cold["node_id"]);
                    println!("  nats_url:          {}", cold["nats_url"]);
                    println!("  proxy_http_port:   {}", cold["proxy_http_port"]);
                    println!("  proxy_https_port:  {}", cold["proxy_https_port"]);
                    println!("  admin_port:        {}", cold["admin_port"]);
                    println!("  artifact_port:     {}", cold["artifact_port"]);
                    println!("  db_path:           {}", cold["db_path"]);
                    println!("  port_range:        {}", cold["port_range"]);
                    println!("  database_url:      {}", cold["database_url"]);
                    println!("  key_source:        {}", cold["key_source"]);
                    println!();
                    println!("Hot (runtime) config:");
                    let hot = &result["hot"];
                    let rl = &hot["rate_limit"];
                    println!(
                        "  rate_limit.default_requests_per_second:  {}",
                        rl["default_requests_per_second"]
                    );
                    println!(
                        "  rate_limit.default_burst_capacity:       {}",
                        rl["default_burst_capacity"]
                    );
                    println!(
                        "  rate_limit.default_per_ip_limit:         {}",
                        rl["default_per_ip_limit"]
                    );
                    let ebpf = &hot["ebpf"];
                    println!(
                        "  ebpf.fd_soft_limit:                      {}",
                        ebpf["fd_soft_limit"]
                    );
                    println!(
                        "  ebpf.fd_hard_limit:                       {}",
                        ebpf["fd_hard_limit"]
                    );
                    println!(
                        "  ebpf.mem_low_threshold_pages:             {}",
                        ebpf["mem_low_threshold_pages"]
                    );
                    println!(
                        "  ebpf.mem_critical_threshold_pages:       {}",
                        ebpf["mem_critical_threshold_pages"]
                    );
                    println!(
                        "  ebpf.disk_slow_threshold_ns:              {}",
                        ebpf["disk_slow_threshold_ns"]
                    );
                    println!(
                        "  ebpf.tcp_conn_limit_per_pid:              {}",
                        ebpf["tcp_conn_limit_per_pid"]
                    );
                    println!(
                        "  ebpf.syscall_rate_limit:                  {}",
                        ebpf["syscall_rate_limit"]
                    );
                    let gc = &hot["gc"];
                    println!(
                        "  gc.gc_interval_secs:                      {}",
                        gc["gc_interval_secs"]
                    );
                    println!(
                        "  gc.disk_warning_threshold:                {}",
                        gc["disk_warning_threshold"]
                    );
                    let hlth = &hot["health"];
                    println!(
                        "  health.check_interval_secs:               {}",
                        hlth["check_interval_secs"]
                    );
                    println!(
                        "  health.default_idle_timeout_secs:        {}",
                        hlth["default_idle_timeout_secs"]
                    );
                    let log = &hot["logging"];
                    println!(
                        "  logging.level:                            {}",
                        log["level"]
                    );
                }
                Ok(())
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let bus = match &cli.nats_creds {
        Some(creds) => messaging::NatsBus::connect_secure(&cli.nats_url, creds).await?,
        None => messaging::NatsBus::connect(&cli.nats_url).await?,
    };
    let auth_token = resolve_auth_token(cli.auth_token.as_deref());
    let http = build_http_client(auth_token.as_deref());

    match cli.command {
        Commands::Deploy(args) => cmds::deploy::run(args, &bus, &cli.node_api, &http).await?,
        Commands::Remove { app_id } => cmds::deploy::remove(&app_id, &bus).await?,
        Commands::List => cmds::list::run(&cli.node_api, &http).await?,
        Commands::Instances => cmds::list::instances(&cli.node_api, &http).await?,
        Commands::Routes(args) => cmds::routes::run(args, &bus).await?,
        Commands::Secrets(args) => cmds::secrets::run(args, &bus).await?,
        Commands::Logs { app_id } => cmds::logs::run(&app_id, &cli.node_api, &http).await?,
        Commands::Status => cmds::status::run(&cli.node_api, &http).await?,
        Commands::Platform(args) => cmds::platform::run(args, &bus, &cli.node_api, &http).await?,
        Commands::Gc(args) => cmds::gc::run(args, &bus, &cli.node_api, &http).await?,
        Commands::Node { target: _, action } => action.run(&cli.node_api, &http).await?,
        Commands::Cluster => cmds::node::cluster_health(&bus).await?,
        Commands::Billing { store_path, action } => match action {
            BillingAction::Report {
                tenant,
                start_ms,
                end_ms,
            } => cmds::billing::report(&store_path, &tenant, start_ms, end_ms).await?,
            BillingAction::Verify => cmds::billing::verify(&store_path).await?,
            BillingAction::Records { app, tenant, last } => {
                cmds::billing::records(&store_path, app.as_deref(), tenant.as_deref(), last).await?
            }
            BillingAction::Export { output } => cmds::billing::export(&store_path, &output).await?,
        },
        Commands::Policy { action } => cmds::policy::run(action, &cli.node_api, &http).await?,
    }
    Ok(())
}

/// Build an HTTP client with an optional Bearer token in the default headers.
fn build_http_client(token: Option<&str>) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();

    if let Some(t) = token {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", t)) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        } else {
            eprintln!(
                "warning: auth token contains invalid header characters — sending without auth"
            );
        }
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP client")
}

/// Resolve the auth token from: CLI flag > env var > config file.
fn resolve_auth_token(cli_token: Option<&str>) -> Option<String> {
    // 1. CLI flag (highest priority)
    if cli_token.is_some() {
        return cli_token.map(|s| s.to_string());
    }

    // 2. Environment variable
    if let Ok(t) = std::env::var("WASM_CTL_AUTH_TOKEN") {
        return Some(t);
    }

    // 3. Config file (~/.wasm-ctl/config.toml)
    if let Some(home) = dirs_home_dir() {
        let config_path = home.join(".wasm-ctl").join("config.toml");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(value) = toml::from_str::<toml::Value>(&content) {
                    if let Some(token) = value
                        .get("auth")
                        .and_then(|a| a.get("token"))
                        .and_then(|t| t.as_str())
                    {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Best-effort home directory resolution (avoids adding the `dirs` crate).
fn dirs_home_dir() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(std::path::PathBuf::from)
}

/// Check an HTTP response for authentication-related errors and produce
/// user-friendly error messages.
pub fn handle_auth_response(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!(
            "Authentication failed. Set --auth-token or WASM_CTL_AUTH_TOKEN environment variable."
        );
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!(
            "Permission denied. Your token has read-only access but this operation requires write access.\n\
             Use the write token (auth.write_token in config.toml)."
        );
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        anyhow::bail!("Admin API rate limit exceeded. Wait a moment and try again.");
    }
    if status.is_server_error() {
        anyhow::bail!("Server error: {}", status);
    }
    Ok(response)
}
