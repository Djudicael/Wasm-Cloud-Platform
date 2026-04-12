// crates/ctl/src/main.rs
use clap::{Parser, Subcommand};

mod cmds {
    pub mod deploy;
    pub mod list;
    pub mod logs;
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
    }
    Ok(())
}
