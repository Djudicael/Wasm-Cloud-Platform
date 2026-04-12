// crates/ctl/src/cmds/routes.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::types::{AppId, Route};
use messaging::{events::Event, NatsBus};

#[derive(Args)]
pub struct RoutesArgs {
    #[command(subcommand)]
    cmd: RoutesCmd,
}

#[derive(Subcommand)]
enum RoutesCmd {
    /// Add a new route
    Add {
        /// Host header to match (e.g. "api.myapp.com")
        #[arg(long)]
        host: String,
        /// Target app ID (e.g. "api-users:v2")
        #[arg(long)]
        app: String,
    },
    /// Remove an existing route
    Remove {
        #[arg(long)]
        host: String,
    },
    /// List all routes (fetched from node API)
    List,
}

pub async fn run(args: RoutesArgs, bus: &NatsBus) -> Result<()> {
    match args.cmd {
        RoutesCmd::Add { host, app } => {
            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let event = Event::RouteAdd {
                route: Route {
                    host: host.clone(),
                    app_id: AppId::new(name, version),
                    path_prefix: "/".to_string(),
                    strip_prefix: false,
                    created_at: now,
                    updated_at: now,
                },
            };
            bus.publish(&event).await?;
            println!(
                "{} Route added: {} → {}",
                "✓".green(),
                host.cyan(),
                app.yellow()
            );
        }
        RoutesCmd::Remove { host } => {
            bus.publish(&Event::RouteRemove { host: host.clone() })
                .await?;
            println!("{} Route removed: {}", "✓".green(), host.cyan());
        }
        RoutesCmd::List => {
            println!("Use `wasm-ctl status` to fetch routes from the node API");
        }
    }
    Ok(())
}
