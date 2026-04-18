// crates/ctl/src/cmds/deploy.rs
use anyhow::Result;
use clap::Args;
use colored::Colorize;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use indicatif::{ProgressBar, ProgressStyle};
use messaging::{events::Event, NatsBus};
use sha2::{Digest, Sha256};
use hex;
use std::collections::HashMap;

#[derive(Args)]
pub struct DeployArgs {
    /// Application name (e.g. "api-users")
    #[arg(long)]
    app: String,

    /// Version string (e.g. "v2")
    #[arg(long, default_value = "v1")]
    version: String,

    /// Path to the .wasm binary
    #[arg(long)]
    wasm: String,

    /// Fuel quota (CPU units per request)
    #[arg(long, default_value = "500000000")]
    fuel: u64,

    /// Memory limit in MB
    #[arg(long, default_value = "128")]
    memory_mb: u32,

    /// Max concurrent instances on this node
    #[arg(long, default_value = "10")]
    max_instances: u32,

    /// Idle timeout in seconds
    #[arg(long, default_value = "300")]
    idle_timeout: u64,

    /// Environment variables (KEY=VALUE, repeatable)
    #[arg(long = "env", value_parser = parse_env_var)]
    env_vars: Vec<(String, String)>,

    /// Secret keys to inject (names only, not values)
    #[arg(long = "secret")]
    secret_keys: Vec<String>,

    /// Node API URL to upload the artifact to (overrides global --node-api)
    #[arg(long)]
    node_api: Option<String>,
}

fn parse_env_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got: {s}"))
}

pub async fn run(
    args: DeployArgs,
    bus: &NatsBus,
    default_node_api: &str,
    http: &reqwest::Client,
) -> Result<()> {
    let app_id = AppId::new(&args.app, &args.version);

    // 1. Read the .wasm file
    let wasm_bytes = std::fs::read(&args.wasm)
        .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", args.wasm, e))?;

    let size_bytes = wasm_bytes.len() as u64;

    // 2. Compute SHA-256
    let sha256 = hex::encode(Sha256::digest(&wasm_bytes));
    println!("{}", "Deploying application:".bold());
    println!("  App ID:  {}", app_id.0.cyan());
    println!("  SHA-256: {}", sha256.yellow());
    println!(
        "  Size:    {} bytes ({:.1} MB)",
        size_bytes,
        size_bytes as f64 / 1_048_576.0
    );

    // 3. Upload the binary to the artifact server
    let node_api = args.node_api.as_deref().unwrap_or(default_node_api);
    let upload_url = format!("{}/artifacts/{}", node_api, sha256);
    let artifact_url = upload_url.clone();

    println!("\n{}", "Uploading artifact...".bold());
    let pb = ProgressBar::new(size_bytes);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap()
            .progress_chars("=>-"),
    );

    let resp = http.put(&upload_url).body(wasm_bytes).send().await?;

    pb.finish_with_message("uploaded");

    if !resp.status().is_success() {
        anyhow::bail!("Artifact upload failed: {}", resp.status());
    }
    println!("{} Artifact uploaded to {}", "✓".green(), upload_url);

    // 4. Build AppConfig
    let config = AppConfig {
        id: app_id.clone(),
        fuel_quota: FuelQuota(args.fuel),
        memory_limit: MemoryPages(args.memory_mb * 16), // 1 MB = 16 pages of 64KB
        max_instances: args.max_instances,
        idle_timeout_secs: args.idle_timeout,
        wasm_bind_port: 8080,
        env_vars: args.env_vars.into_iter().collect::<HashMap<_, _>>(),
        secret_keys: args.secret_keys,
        extended_limits: None,
        health_check_path: None,
        db_max_connections: None,
        rate_limit: None, // Use default rate limiting
        tenant_id: None,
    };

    // 5. Publish deploy event
    let event = Event::DeployApp {
        app_id: app_id.clone(),
        config,
        artifact_url,
        expected_hash: Some(sha256),
        size_bytes,
    };
    bus.publish(&event).await?;

    println!(
        "{} Deploy event published for {} — all nodes are compiling.",
        "✓".green(),
        app_id.0.cyan()
    );
    Ok(())
}

pub async fn remove(app_id_str: &str, bus: &NatsBus) -> Result<()> {
    let (name, version) = app_id_str
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("app_id must be <name>:<version>"))?;
    let event = Event::RemoveApp {
        app_id: AppId::new(name, version),
    };
    bus.publish(&event).await?;
    println!(
        "{} Remove event published for {}",
        "✓".green(),
        app_id_str.cyan()
    );
    Ok(())
}
