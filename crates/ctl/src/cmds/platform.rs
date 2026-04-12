// crates/ctl/src/cmds/platform.rs
use clap::Subcommand;
use messaging::events::Event;
use messaging::NatsBus;

#[derive(clap::Args)]
pub struct PlatformArgs {
    #[command(subcommand)]
    pub command: PlatformCommands,
}

#[derive(Subcommand)]
pub enum PlatformCommands {
    /// Upload a new platform binary to artifact storage
    Upload {
        /// Path to the new binary
        #[arg(long)]
        binary_path: String,

        /// Target artifact server URL
        #[arg(long)]
        artifact_url: String,

        /// Protocol version of the new binary
        #[arg(long)]
        protocol_version: u32,

        /// Binary version string (e.g., "0.2.0")
        #[arg(long)]
        binary_version: String,
    },

    /// Initiate a rolling upgrade of the cluster
    Upgrade {
        /// URL where the new binary can be downloaded
        #[arg(long)]
        binary_url: String,

        /// SHA-256 hash of the new binary
        #[arg(long)]
        sha256: String,

        /// Protocol version of the new binary
        #[arg(long)]
        protocol_version: u32,

        /// Binary version string
        #[arg(long)]
        binary_version: String,

        /// Target specific node (optional, defaults to all nodes)
        #[arg(long)]
        target_node: Option<String>,
    },

    /// Check cluster upgrade status
    Status,

    /// Rollback a node to the previous binary version
    Rollback {
        /// Node ID to rollback
        #[arg(long)]
        node_id: String,
    },
}

pub async fn run(
    args: PlatformArgs,
    bus: &NatsBus,
    node_api: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    match args.command {
        PlatformCommands::Upload {
            binary_path,
            artifact_url,
            protocol_version,
            binary_version,
        } => {
            upload_binary(
                &binary_path,
                &artifact_url,
                protocol_version,
                &binary_version,
                http,
            )
            .await?;
        }
        PlatformCommands::Upgrade {
            binary_url,
            sha256,
            protocol_version,
            binary_version,
            target_node,
        } => {
            initiate_upgrade(
                &binary_url,
                &sha256,
                protocol_version,
                &binary_version,
                target_node,
                bus,
            )
            .await?;
        }
        PlatformCommands::Status => {
            check_upgrade_status(node_api, http).await?;
        }
        PlatformCommands::Rollback { node_id } => {
            rollback_node(&node_id, node_api, http).await?;
        }
    }
    Ok(())
}

async fn upload_binary(
    binary_path: &str,
    artifact_url: &str,
    protocol_version: u32,
    binary_version: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    println!("📦 Reading binary from {}", binary_path);
    let binary_bytes = std::fs::read(binary_path)?;

    // Compute SHA-256
    let mut hasher = Sha256::new();
    hasher.update(&binary_bytes);
    let sha256 = format!("{:x}", hasher.finalize());

    println!("🔐 SHA-256: {}", sha256);
    println!("📤 Uploading to {}...", artifact_url);

    let upload_url = format!("{}/artifacts/{}", artifact_url, sha256);
    let resp = http.put(&upload_url).body(binary_bytes).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Upload failed with status: {}", resp.status());
    }

    println!("✅ Binary uploaded successfully");
    println!("   Protocol version: {}", protocol_version);
    println!("   Binary version:   {}", binary_version);
    println!("   SHA-256:          {}", sha256);
    println!();
    println!("To upgrade the cluster, run:");
    println!("  wasm-ctl platform upgrade \\");
    println!("    --binary-url {}/artifacts/{} \\", artifact_url, sha256);
    println!("    --sha256 {} \\", sha256);
    println!("    --protocol-version {} \\", protocol_version);
    println!("    --binary-version {}", binary_version);

    Ok(())
}

async fn initiate_upgrade(
    binary_url: &str,
    sha256: &str,
    protocol_version: u32,
    binary_version: &str,
    target_node: Option<String>,
    bus: &NatsBus,
) -> anyhow::Result<()> {
    println!("🚀 Initiating platform upgrade");
    println!("   Binary URL:       {}", binary_url);
    println!("   SHA-256:          {}", sha256);
    println!("   Protocol version: {}", protocol_version);
    println!("   Binary version:   {}", binary_version);

    if let Some(ref node) = target_node {
        println!("   Target node:      {}", node);
    } else {
        println!("   Target:           All nodes (rolling upgrade)");
    }

    let event = Event::NodeUpgrade {
        target_node: target_node.unwrap_or_else(|| "*".to_string()),
        binary_url: binary_url.to_string(),
        binary_sha256: sha256.to_string(),
        new_protocol_version: protocol_version,
        new_binary_version: binary_version.to_string(),
    };

    bus.publish(&event).await?;

    println!("✅ Upgrade event published");
    println!();
    println!("Monitor progress with:");
    println!("  wasm-ctl platform status");

    Ok(())
}

async fn check_upgrade_status(node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    println!("📊 Cluster Upgrade Status");
    println!();

    // Query the cluster status endpoint
    let status_url = format!("{}/api/cluster/status", node_api);
    let resp = http.get(&status_url).send().await?;

    if !resp.status().is_success() {
        println!("⚠️  Could not fetch cluster status: {}", resp.status());
        return Ok(());
    }

    let status: serde_json::Value = resp.json().await?;

    println!("{}", serde_json::to_string_pretty(&status)?);

    Ok(())
}

async fn rollback_node(
    node_id: &str,
    node_api: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    println!("⏮️  Rolling back node: {}", node_id);

    let rollback_url = format!("{}/api/nodes/{}/rollback", node_api, node_id);
    let resp = http.post(&rollback_url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Rollback failed with status: {}", resp.status());
    }

    println!("✅ Rollback initiated");
    println!("   The node will restart with the previous binary version");

    Ok(())
}
