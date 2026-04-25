// crates/ctl/src/cmds/app.rs
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Args)]
pub struct AppArgs {
    #[command(subcommand)]
    pub action: AppAction,
}

#[derive(Subcommand)]
pub enum AppAction {
    /// List deployed applications
    List {
        /// Filter by namespace
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Show the effective manifest for an app
    Manifest {
        /// App ID (name:version or namespace/name:version)
        app_id: String,
    },
}

pub async fn run(args: AppArgs, node_api: &str, http: &reqwest::Client) -> Result<()> {
    match args.action {
        AppAction::List { namespace } => {
            let url = format!("{}/admin/apps?namespace={}", node_api, namespace);
            let resp = http.get(&url).send().await?;
            if resp.status().is_success() {
                let body: serde_json::Value = resp.json().await?;
                // The server may return either an array of apps or an error object
                if let Some(apps) = body.as_array() {
                    if apps.is_empty() {
                        println!("No apps found in namespace '{}'", namespace);
                    } else {
                        println!("{}", format!("Apps in namespace '{}':", namespace).bold());
                        for app in apps {
                            let id = app["id"].as_str().unwrap_or("unknown");
                            let instances = app["instances"].as_u64().unwrap_or(0);
                            println!("  {} ({} instances)", id.cyan(), instances);
                        }
                    }
                } else if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                    anyhow::bail!("Server error: {}", err);
                } else {
                    anyhow::bail!("Unexpected response from server");
                }
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Failed to list apps: {} — {}", status, body);
            }
            Ok(())
        }
        AppAction::Manifest { app_id } => {
            let url = format!("{}/admin/apps/{}/manifest", node_api, app_id);
            let resp = http.get(&url).send().await?;
            if resp.status().is_success() {
                let body: serde_json::Value = resp.json().await?;
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Failed to fetch manifest: {} — {}", status, body);
            }
            Ok(())
        }
    }
}
