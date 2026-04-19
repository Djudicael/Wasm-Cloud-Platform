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
    let http = reqwest::Client::new();

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
    }
    Ok(())
}
