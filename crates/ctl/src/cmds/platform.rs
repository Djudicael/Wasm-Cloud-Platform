use clap::Subcommand;
use common::upgrade_provenance::{
    NodeBinaryReleaseProvenance, SignedNodeBinaryReleaseProvenance, SignedReleaseKeyDelegation,
};
use ed25519_dalek::{Signer, SigningKey};
use messaging::events::{node_upgrade_signature_payload, Event};
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

        /// Optional path to a 32-byte Ed25519 signing key encoded as hex.
        #[arg(long)]
        signing_key_file: Option<String>,

        /// Optional path to a root-signed release-key delegation JSON file.
        /// When provided with `--signing-key-file`, ctl emits delegated release provenance
        /// instead of only a detached event signature.
        #[arg(long)]
        provenance_delegation_file: Option<String>,

        /// Optional repository URL included in release provenance metadata.
        #[arg(long)]
        provenance_source_repository: Option<String>,

        /// Optional source commit SHA included in release provenance metadata.
        #[arg(long)]
        provenance_source_commit_sha: Option<String>,

        /// Optional workflow reference included in release provenance metadata.
        #[arg(long)]
        provenance_build_workflow_ref: Option<String>,

        /// Optional build/run identifier included in release provenance metadata.
        #[arg(long)]
        provenance_build_run_id: Option<String>,

        /// Release provenance TTL in seconds.
        #[arg(long, default_value_t = 86400)]
        provenance_ttl_secs: u64,
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

struct UpgradeRequest {
    binary_url: String,
    sha256: String,
    protocol_version: u32,
    binary_version: String,
    target_node: Option<String>,
    signing_key_file: Option<String>,
    provenance_delegation_file: Option<String>,
    provenance_source_repository: Option<String>,
    provenance_source_commit_sha: Option<String>,
    provenance_build_workflow_ref: Option<String>,
    provenance_build_run_id: Option<String>,
    provenance_ttl_secs: u64,
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
            signing_key_file,
            provenance_delegation_file,
            provenance_source_repository,
            provenance_source_commit_sha,
            provenance_build_workflow_ref,
            provenance_build_run_id,
            provenance_ttl_secs,
        } => {
            initiate_upgrade(
                UpgradeRequest {
                    binary_url,
                    sha256,
                    protocol_version,
                    binary_version,
                    target_node,
                    signing_key_file,
                    provenance_delegation_file,
                    provenance_source_repository,
                    provenance_source_commit_sha,
                    provenance_build_workflow_ref,
                    provenance_build_run_id,
                    provenance_ttl_secs,
                },
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

    println!("Reading binary from {}", binary_path);
    let binary_bytes = std::fs::read(binary_path)?;

    let mut hasher = Sha256::new();
    hasher.update(&binary_bytes);
    let sha256 = hex::encode(hasher.finalize());

    println!("SHA-256: {}", sha256);
    println!("Uploading to {}...", artifact_url);

    let upload_url = format!("{}/artifacts/{}", artifact_url, sha256);
    let resp = http.put(&upload_url).body(binary_bytes).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Upload failed with status: {}", resp.status());
    }

    println!("Binary uploaded successfully");
    println!("  Protocol version: {}", protocol_version);
    println!("  Binary version:   {}", binary_version);
    println!("  SHA-256:          {}", sha256);
    println!();
    println!("To upgrade the cluster, run:");
    println!("  wasm-ctl platform upgrade \\");
    println!("    --binary-url {}/artifacts/{} \\", artifact_url, sha256);
    println!("    --sha256 {} \\", sha256);
    println!("    --protocol-version {} \\", protocol_version);
    println!("    --binary-version {}", binary_version);

    Ok(())
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_upgrade_signing_key(path: &str) -> anyhow::Result<SigningKey> {
    let raw = std::fs::read_to_string(path)?;
    let decoded = hex::decode(raw.trim())?;
    let key_bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("upgrade signing key must decode to exactly 32 bytes"))?;
    Ok(SigningKey::from_bytes(&key_bytes))
}

fn load_signed_release_key_delegation(path: &str) -> anyhow::Result<SignedReleaseKeyDelegation> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(Into::into)
}

async fn initiate_upgrade(request: UpgradeRequest, bus: &NatsBus) -> anyhow::Result<()> {
    let UpgradeRequest {
        binary_url,
        sha256,
        protocol_version,
        binary_version,
        target_node,
        signing_key_file,
        provenance_delegation_file,
        provenance_source_repository,
        provenance_source_commit_sha,
        provenance_build_workflow_ref,
        provenance_build_run_id,
        provenance_ttl_secs,
    } = request;

    println!("Initiating platform upgrade");
    println!("  Binary URL:       {}", binary_url);
    println!("  SHA-256:          {}", sha256);
    println!("  Protocol version: {}", protocol_version);
    println!("  Binary version:   {}", binary_version);

    if let Some(ref node) = target_node {
        println!("  Target node:      {}", node);
    } else {
        println!("  Target:           All nodes (rolling upgrade)");
    }

    let target_node = target_node.unwrap_or_else(|| "*".to_string());
    let (signature_ed25519, release_provenance) = match (
        signing_key_file.as_deref(),
        provenance_delegation_file.as_deref(),
    ) {
        (Some(signing_key_path), Some(delegation_path)) => {
            let signing_key = load_upgrade_signing_key(signing_key_path)?;
            let delegation = load_signed_release_key_delegation(delegation_path)?;
            let delegated_public_key = hex::encode(signing_key.verifying_key().to_bytes());
            if delegation.delegation.public_key_ed25519.trim() != delegated_public_key {
                anyhow::bail!(
                    "delegation public key does not match the provided provenance signing key"
                );
            }

            let issued_at_ms = now_unix_ms();
            let provenance = NodeBinaryReleaseProvenance {
                version: 1,
                delegation_key_id: delegation.delegation.key_id.clone(),
                binary_url: binary_url.clone(),
                binary_sha256: sha256.clone(),
                new_protocol_version: protocol_version,
                new_binary_version: binary_version.clone(),
                source_repository: provenance_source_repository,
                source_commit_sha: provenance_source_commit_sha,
                build_workflow_ref: provenance_build_workflow_ref,
                build_run_id: provenance_build_run_id,
                issued_at_ms,
                expires_at_ms: issued_at_ms
                    .saturating_add(provenance_ttl_secs.saturating_mul(1000)),
            };
            (
                None,
                Some(SignedNodeBinaryReleaseProvenance::sign(
                    provenance,
                    delegation,
                    &signing_key,
                )),
            )
        }
        (Some(signing_key_path), None) => {
            let signing_key = load_upgrade_signing_key(signing_key_path)?;
            let payload = node_upgrade_signature_payload(
                &target_node,
                &binary_url,
                &sha256,
                protocol_version,
                &binary_version,
            );
            (
                Some(hex::encode(signing_key.sign(&payload).to_bytes())),
                None,
            )
        }
        (None, Some(_)) => {
            anyhow::bail!("--provenance-delegation-file requires --signing-key-file");
        }
        (None, None) => (None, None),
    };

    let event = Event::NodeUpgrade {
        target_node,
        binary_url,
        binary_sha256: sha256,
        signature_ed25519,
        release_provenance,
        new_protocol_version: protocol_version,
        new_binary_version: binary_version,
    };

    bus.publish(&event).await?;

    println!("Upgrade event published");
    println!();
    println!("Monitor progress with:");
    println!("  wasm-ctl platform status");

    Ok(())
}

async fn check_upgrade_status(node_api: &str, http: &reqwest::Client) -> anyhow::Result<()> {
    println!("Cluster Upgrade Status");
    println!();

    let status_url = format!("{}/api/cluster/status", node_api);
    let resp = http.get(&status_url).send().await?;

    if !resp.status().is_success() {
        println!("Could not fetch cluster status: {}", resp.status());
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
    println!("Rolling back node: {}", node_id);

    let rollback_url = format!("{}/api/nodes/{}/rollback", node_api, node_id);
    let resp = http.post(&rollback_url).send().await?;

    if !resp.status().is_success() {
        anyhow::bail!("Rollback failed with status: {}", resp.status());
    }

    println!("Rollback initiated");
    println!("  The node will restart with the previous binary version");

    Ok(())
}
