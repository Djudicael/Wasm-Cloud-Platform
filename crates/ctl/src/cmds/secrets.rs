use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use common::deploy::ArtifactCredentialSetRequest;
use common::types::{AppId, ClusterNodeRecord};
use messaging::{events::Event, NatsBus};
use secrets::{encrypt_for_peer, SecretTransportEnvelope};

const ARTIFACT_CREDENTIALS_APP: &str = "_platform/artifact-credentials:v1";

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
        /// Inline value (prefer the hidden prompt or --value-file)
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        /// Read the value from a mode-0600 file, or use '-' for standard input
        #[arg(long, value_name = "PATH", conflicts_with = "value")]
        value_file: Option<std::path::PathBuf>,
    },
    /// Set a credential used for remote artifact fetch during deploy ingress
    SetArtifactCredential {
        #[arg(long)]
        key: String,
        /// Inline value (prefer the hidden prompt or --value-file)
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        /// Read the value from a mode-0600 file, or use '-' for standard input
        #[arg(long, value_name = "PATH", conflicts_with = "value")]
        value_file: Option<std::path::PathBuf>,
    },
    /// Revoke a secret on every node recorded in the authoritative registry
    Delete {
        #[arg(long)]
        app: String,
        #[arg(long)]
        key: String,
    },
}

fn read_secret_value(
    value: Option<String>,
    value_file: Option<std::path::PathBuf>,
    key: &str,
) -> Result<String> {
    let mut value = if let Some(value) = value {
        value
    } else if let Some(path) = value_file {
        if path.as_os_str() == "-" {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut input)?;
            input
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path)?.permissions().mode();
                if mode & 0o077 != 0 {
                    anyhow::bail!(
                        "secret value file {} must use mode 0600 or stricter",
                        path.display()
                    );
                }
            }
            std::fs::read_to_string(&path)?
        }
    } else {
        rpassword::prompt_password(format!("Value for {}: ", key.cyan()))?
    };
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.is_empty() {
        anyhow::bail!("secret value must not be empty");
    }
    Ok(value)
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

async fn distribute_secret_value(
    bus: &NatsBus,
    http: &reqwest::Client,
    node_api: &str,
    app_id: AppId,
    key: String,
    plaintext: String,
) -> Result<()> {
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

    Ok(())
}

fn select_delete_targets(mut nodes: Vec<ClusterNodeRecord>) -> Vec<String> {
    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    nodes.into_iter().map(|node| node.node_id).collect()
}

async fn distribute_secret_delete(
    bus: &NatsBus,
    http: &reqwest::Client,
    node_api: &str,
    app_id: AppId,
    key: String,
) -> Result<()> {
    let registry = load_cluster_node_registry(http, node_api).await?;
    let targets = select_delete_targets(registry.nodes);
    if targets.is_empty() {
        anyhow::bail!(
            "authoritative cluster node registry contains no nodes for secret revocation"
        );
    }
    for target_node_id in targets {
        bus.publish(&Event::SecretDelete {
            app_id: app_id.clone(),
            key: key.clone(),
            target_node_id,
        })
        .await?;
    }
    Ok(())
}

async fn store_artifact_credential_via_deploy_api(
    http: &reqwest::Client,
    deploy_api: &str,
    key: String,
    plaintext: String,
) -> Result<()> {
    let url = format!(
        "{}/deploy/artifact-credentials",
        deploy_api.trim_end_matches('/')
    );
    let response = http
        .put(&url)
        .json(&ArtifactCredentialSetRequest {
            key,
            value: plaintext,
        })
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "artifact credential request failed: HTTP {} from {}",
            response.status(),
            url
        );
    }
    Ok(())
}

pub async fn run(
    args: SecretsArgs,
    bus: &NatsBus,
    node_api: &str,
    deploy_api: Option<&str>,
    http: &reqwest::Client,
) -> Result<()> {
    match args.cmd {
        SecretsCmd::Set {
            app,
            key,
            value,
            value_file,
        } => {
            let plaintext = read_secret_value(value, value_file, &key)?;

            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let app_id = AppId::new(name, version);
            distribute_secret_value(bus, http, node_api, app_id.clone(), key.clone(), plaintext)
                .await?;

            println!(
                "{} Secret '{}' set for {}",
                "\u{2713}".green(),
                key.cyan(),
                app.yellow()
            );
        }
        SecretsCmd::SetArtifactCredential {
            key,
            value,
            value_file,
        } => {
            let plaintext = read_secret_value(value, value_file, &key)?;
            if let Some(deploy_api) = deploy_api {
                store_artifact_credential_via_deploy_api(http, deploy_api, key.clone(), plaintext)
                    .await?;
            } else {
                let app_id = AppId(ARTIFACT_CREDENTIALS_APP.to_string());
                distribute_secret_value(bus, http, node_api, app_id, key.clone(), plaintext)
                    .await?;
            }
            println!(
                "{} Artifact credential '{}' set for {}",
                "\u{2713}".green(),
                key.cyan(),
                ARTIFACT_CREDENTIALS_APP.yellow()
            );
        }
        SecretsCmd::Delete { app, key } => {
            let (name, version) = app
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("app must be <name>:<version>"))?;
            let app_id = AppId::new(name, version);
            distribute_secret_delete(bus, http, node_api, app_id, key.clone()).await?;
            println!(
                "{} Secret '{}' revoked for {}",
                "\u{2713}".green(),
                key.cyan(),
                app.yellow()
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

    #[test]
    fn delete_targets_include_stale_nodes_for_eventual_revocation() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nodes = vec![
            active_node("node-b", now.saturating_sub(500), "00"),
            active_node("node-a", now, "00"),
        ];
        assert_eq!(select_delete_targets(nodes), vec!["node-a", "node-b"]);
    }
}
