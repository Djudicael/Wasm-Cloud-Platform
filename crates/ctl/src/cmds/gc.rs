// crates/ctl/src/cmds/gc.rs
use clap::Subcommand;
use common::gc::GcConfig;

#[derive(clap::Args)]
pub struct GcArgs {
    #[command(subcommand)]
    pub command: GcCommands,
}

#[derive(Subcommand)]
pub enum GcCommands {
    /// View current GC configuration
    Config,

    /// Update GC configuration
    ConfigSet {
        /// Number of artifact versions to keep per app
        #[arg(long)]
        artifact_keep: Option<usize>,

        /// Number of days to retain metrics
        #[arg(long)]
        metrics_retain: Option<u32>,

        /// Undeploy grace period in seconds
        #[arg(long)]
        undeploy_grace: Option<u64>,

        /// GC interval in seconds
        #[arg(long)]
        gc_interval: Option<u64>,

        /// Disk warning threshold (0.0-1.0)
        #[arg(long)]
        disk_warning_threshold: Option<f64>,
    },

    /// Force an immediate GC run
    Run {
        /// Target node ID (optional)
        #[arg(long)]
        node: Option<String>,
    },

    /// View disk usage
    Disk {
        /// Target node ID (optional)
        #[arg(long)]
        node: Option<String>,
    },
}

pub async fn run(
    args: GcArgs,
    _bus: &messaging::NatsBus,
    node_api: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    match args.command {
        GcCommands::Config => {
            show_config(node_api, http).await?;
        }
        GcCommands::ConfigSet {
            artifact_keep,
            metrics_retain,
            undeploy_grace,
            gc_interval,
            disk_warning_threshold,
        } => {
            update_config(
                node_api,
                http,
                artifact_keep,
                metrics_retain,
                undeploy_grace,
                gc_interval,
                disk_warning_threshold,
            )
            .await?;
        }
        GcCommands::Run { node } => {
            trigger_gc(node_api, http, node).await?;
        }
        GcCommands::Disk { node } => {
            show_disk_usage(node_api, http, node).await?;
        }
    }
    Ok(())
}

async fn show_config(node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    println!("📋 GC Configuration");
    println!();

    let config_url = format!("{}/api/gc/config", node_api);
    let resp = http.get(&config_url).send().await?;

    if !resp.status().is_success() {
        println!("⚠️  Could not fetch GC config: {}", resp.status());
        println!("Using default configuration:");
        let config = GcConfig::default();
        print_config(&config);
        return Ok(());
    }

    let config: GcConfig = resp.json().await?;
    print_config(&config);

    Ok(())
}

fn print_config(config: &GcConfig) {
    println!(
        "  artifact_keep_versions:  {}",
        config.artifact_keep_versions
    );
    println!("  metrics_retain_days:     {}", config.metrics_retain_days);
    println!(
        "  undeploy_grace_secs:     {} ({} hour)",
        config.undeploy_grace_secs,
        config.undeploy_grace_secs / 3600
    );
    println!(
        "  gc_interval_secs:        {} ({} minutes)",
        config.gc_interval_secs,
        config.gc_interval_secs / 60
    );
    println!(
        "  disk_warning_threshold:  {:.0}%",
        config.disk_warning_threshold * 100.0
    );
}

async fn update_config(
    node_api: &str,
    http: &reqwest::Client,
    artifact_keep: Option<usize>,
    metrics_retain: Option<u32>,
    undeploy_grace: Option<u64>,
    gc_interval: Option<u64>,
    disk_warning_threshold: Option<f64>,
) -> anyhow::Result<()> {
    println!("🔧 Updating GC Configuration");

    // Get current config
    let config_url = format!("{}/api/gc/config", node_api);
    let current: GcConfig = match http.get(&config_url).send().await {
        Ok(resp) => resp.json().await.unwrap_or_default(),
        Err(_) => GcConfig::default(),
    };

    // Apply updates
    let updated = GcConfig {
        artifact_keep_versions: artifact_keep.unwrap_or(current.artifact_keep_versions),
        metrics_retain_days: metrics_retain.unwrap_or(current.metrics_retain_days),
        undeploy_grace_secs: undeploy_grace.unwrap_or(current.undeploy_grace_secs),
        gc_interval_secs: gc_interval.unwrap_or(current.gc_interval_secs),
        disk_warning_threshold: disk_warning_threshold.unwrap_or(current.disk_warning_threshold),
    };

    // Send update
    let resp = http.put(&config_url).json(&updated).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Failed to update config: {}", resp.status());
    }

    println!("✅ Configuration updated");
    println!();
    print_config(&updated);

    Ok(())
}

async fn trigger_gc(
    node_api: &str,
    http: &reqwest::Client,
    node: Option<String>,
) -> anyhow::Result<()> {
    println!("🗑️  Triggering garbage collection");

    let gc_url = if let Some(node_id) = node {
        format!("{}/api/gc/run?node={}", node_api, node_id)
    } else {
        format!("{}/api/gc/run", node_api)
    };

    let resp = http.post(&gc_url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("GC trigger failed: {}", resp.status());
    }

    let result: serde_json::Value = resp.json().await?;

    println!("✅ GC complete");
    println!();
    println!(
        "  Artifacts deleted:      {}",
        result["artifacts_deleted"].as_u64().unwrap_or(0)
    );
    println!(
        "  Raw Wasm deleted:       {}",
        result["raw_wasm_deleted"].as_u64().unwrap_or(0)
    );
    println!(
        "  Configs deleted:        {}",
        result["configs_deleted"].as_u64().unwrap_or(0)
    );
    println!(
        "  Metric buckets pruned:  {}",
        result["metric_buckets_deleted"].as_u64().unwrap_or(0)
    );
    println!(
        "  Apps purged:            {}",
        result["apps_purged"].as_u64().unwrap_or(0)
    );

    Ok(())
}

async fn show_disk_usage(
    node_api: &str,
    http: &reqwest::Client,
    node: Option<String>,
) -> anyhow::Result<()> {
    println!("💾 Disk Usage");
    println!();

    let disk_url = if let Some(node_id) = node {
        format!("{}/api/gc/disk?node={}", node_api, node_id)
    } else {
        format!("{}/api/gc/disk", node_api)
    };

    let resp = http.get(&disk_url).send().await?;

    if !resp.status().is_success() {
        println!("⚠️  Could not fetch disk usage: {}", resp.status());
        return Ok(());
    }

    let result: serde_json::Value = resp.json().await?;

    let file_size_bytes = result["file_size_bytes"].as_u64().unwrap_or(0);
    let file_size_mb = file_size_bytes / 1_048_576;
    let available_bytes = result["available_bytes"].as_u64().unwrap_or(0);
    let available_gb = available_bytes / 1_073_741_824;
    let usage_ratio = result["usage_ratio"].as_f64().unwrap_or(0.0);

    println!("  redb file size:  {} MB", file_size_mb);
    println!("  available disk:  {} GB", available_gb);
    println!("  usage ratio:     {:.2}%", usage_ratio * 100.0);

    if usage_ratio > 0.80 {
        println!();
        println!("⚠️  WARNING: Disk usage exceeds 80% threshold!");
        println!("   Consider increasing disk space or reducing retention.");
    }

    Ok(())
}
