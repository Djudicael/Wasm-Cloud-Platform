use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::types::{AppId, ClusterNodeRecord};
use messaging::{events::Event, NatsBus};
use secrets::{encrypt_for_peer, SecretTransportEnvelope};

#[derive(serde::Deserialize)]
struct ClusterNodeRegistryResponse {
    nodes: Vec<ClusterNodeRecord>,
    #[serde(default = "default_cluster_node_staleness_secs")]
    active_staleness_secs: u64,
}

fn default_cluster_node_staleness_secs() -> u64 {
    120
}

#[derive(Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    cmd: SecretsCmd,
}

#[derive(Subcommand)]
enum SecretsCmd {
    /// Set a secret value for an application
    Set {
        #[arg(long)]
        app: String,
        #[arg(long)]
        key: String,
        /// If not provided, reads from stdin (safe, not visible in shell history)
        #[arg(long)]
        value: Option<String>,
    },
    /// Delete a secret (not yet implemented)
    Delete {
        #[arg(long)]
        app: String,
        #[arg(long)]
        key: String,
    },
}

async fn load_cluster_node_registry(
    http: &reqwest::Client,
    node_api: &str,
) -> Result<ClusterNodeRegistryResponse> {
    let registry_url = format!("{}/admin/cluster/nodes", node_api.trim_end_matches('/'));
    let response = http.get(&registry_url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "cluster node registry request failed: HTTP {} from {}",
            response.status(),
            registry_url
        );
    }
    let mut registry = response.json::<ClusterNodeRegistryResponse>().await?;
    registry.nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(registry)
}

fn select_secret_targets(
    nodes: Vec<ClusterNodeRecord>,
    max_staleness_secs: u64,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut targets = Vec::new();
    for node in nodes
        .into_iter()
        .filter(|node| !node.is_stale(max_staleness_secs))
    {
        let public_key_hex = node.secret_transport_public_key.ok_or_else(|| {
            anyhow::anyhow!(
                "active node {} is missing secret transport public key in cluster registry",
                node.node_id
            )
        })?;
        let public_key_bytes = hex::decode(public_key_hex.trim()).map_err(|e| {
            anyhow::anyhow!(
                "active node {} has invalid secret transport public key hex: {}",
                node.node_id,
                e
            )
        })?;
        targets.push((node.node_id, public_key_bytes));
    }
    Ok(targets)
}

pub async fn run(
    args: SecretsArgs,
    bus: &NatsBus,
    node_api: &str,
    http: &reqwest::Client,
) -> Result<()> {
    match args.cmd {
        SecretsCmd::Set { app, key, value } => {
            let plaintext = match value {
                Some(v) => v,
                None => rpassword::prompt_password(format!("Value for {}: ", key.cyan()))?,
            };

            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let app_id = AppId::new(name, version);

            let registry = load_cluster_node_registry(http, node_api).await?;
            let targets = select_secret_targets(registry.nodes, registry.active_staleness_secs)?;
            if targets.is_empty() {
                anyhow::bail!(
                    "authoritative cluster node registry contains no active nodes for secret distribution"
                );
            }

            for (target_node_id, public_key_bytes) in targets {
                let ciphertext = encrypt_for_peer(&public_key_bytes, plaintext.as_bytes())?;
                let event = Event::SecretUpdate {
                    app_id: app_id.clone(),
                    key: key.clone(),
                    target_node_id: Some(target_node_id),
                    secret: SecretTransportEnvelope::node_transport_ciphertext(ciphertext),
                };
                bus.publish(&event).await?;
            }

            println!(
                "{} Secret '{}' set for {}",
                "\u{2713}".green(),
                key.cyan(),
                app.yellow()
            );
        }
        SecretsCmd::Delete { app, key } => {
            anyhow::bail!(
                "Secret delete for {}/{} - not yet implemented (add Event::SecretDelete)",
                app,
                key
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_node(
        node_id: &str,
        last_seen_unix_secs: u64,
        public_key_hex: &str,
    ) -> ClusterNodeRecord {
        ClusterNodeRecord {
            node_id: node_id.to_string(),
            last_seen_unix_secs,
            joined_at_unix_secs: Some(last_seen_unix_secs),
            health_status: common::health::NodeHealthStatus::Healthy,
            proxy_address: Some(format!("{node_id}.internal:9000")),
            artifact_server_url: Some(format!("http://{node_id}.internal:9091")),
            protocol_version: Some(common::protocol::PROTOCOL_VERSION),
            binary_version: Some(common::protocol::BINARY_VERSION.to_string()),
            secret_transport_public_key: Some(public_key_hex.to_string()),
            accepting_requests: Some(true),
            active_instances: Some(1),
            deployed_apps: Some(1),
        }
    }

    #[test]
    fn test_select_secret_targets_filters_stale_nodes_and_requires_public_keys() {
        let receiver = secrets::BootstrapKeyPair::generate();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let targets = select_secret_targets(
            vec![
                active_node("node-a", now, &hex::encode(receiver.public_bytes())),
                active_node(
                    "node-stale",
                    now.saturating_sub(500),
                    &hex::encode(receiver.public_bytes()),
                ),
            ],
            120,
        )
        .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "node-a");
    }
}
