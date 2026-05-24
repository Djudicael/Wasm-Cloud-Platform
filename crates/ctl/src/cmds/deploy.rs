// crates/ctl/src/cmds/deploy.rs
use anyhow::Result;
use clap::Args;
use colored::Colorize;
use common::artifact_transfer::{
    ArtifactManifestBatchRequest, ArtifactManifestBatchResponse,
    ArtifactUploadAuthorizationResponse,
};
use common::types::{AppConfig, AppId, ClusterNodeRecord, FuelQuota, MemoryPages};
use hex;
use indicatif::{ProgressBar, ProgressStyle};
use messaging::{events::Event, NatsBus};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Args)]
pub struct DeployArgs {
    /// Application name (e.g. "api-users")
    #[arg(long)]
    app: Option<String>,

    /// Version string (e.g. "v2")
    #[arg(long)]
    version: Option<String>,

    /// Namespace (default = "default")
    #[arg(long, default_value = "default")]
    namespace: String,

    /// Path to the .wasm binary
    #[arg(long)]
    wasm: Option<String>,

    /// Path to a deployment manifest TOML file
    #[arg(long)]
    manifest: Option<String>,

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

    // ── Policy flags ──────────────────────────────────────────────────
    /// Apply a pre-defined policy profile (http_api, background_worker, static_site, database_proxy, unrestricted)
    #[arg(long)]
    policy_profile: Option<String>,

    /// Allow outbound TCP (overrides profile)
    #[arg(long)]
    policy_network_allow_outbound_tcp: Option<bool>,

    /// Allow outbound UDP (overrides profile)
    #[arg(long)]
    policy_network_allow_outbound_udp: Option<bool>,

    /// Allow DNS lookups (overrides profile)
    #[arg(long)]
    policy_network_allow_dns: Option<bool>,

    /// Comma-separated allowed CIDRs (e.g. "10.0.0.0/8,172.16.0.0/12")
    #[arg(long)]
    policy_network_allowed_cidrs: Option<String>,

    /// Comma-separated denied CIDRs (e.g. "169.254.169.254/32")
    #[arg(long)]
    policy_network_denied_cidrs: Option<String>,

    /// Max concurrent outbound connections
    #[arg(long)]
    policy_network_max_outbound_connections: Option<u32>,

    /// Max egress bytes (0 = unlimited)
    #[arg(long)]
    policy_network_max_egress_bytes: Option<u64>,

    /// Max open file descriptors
    #[arg(long)]
    policy_fs_max_open_fds: Option<u32>,

    /// Max filesystem write bytes (0 = unlimited)
    #[arg(long)]
    policy_fs_max_write_bytes: Option<u64>,

    /// Allow file creation
    #[arg(long)]
    policy_fs_allow_file_create: Option<bool>,

    /// Allow file deletion
    #[arg(long)]
    policy_fs_allow_file_delete: Option<bool>,

    /// Comma-separated allowed filesystem paths
    #[arg(long)]
    policy_fs_allowed_paths: Option<String>,

    // ── Gateway flags ─────────────────────────────────────────────────
    /// Gateway auth policy: none, authenticated, roles
    #[arg(long)]
    gateway_auth: Option<String>,

    /// Comma-separated roles for gateway auth (when policy=roles)
    #[arg(long, value_delimiter = ',')]
    gateway_roles: Vec<String>,

    /// Keycloak client ID for role checking
    #[arg(long)]
    gateway_oidc_client: Option<String>,

    /// Comma-separated allowed CORS origins
    #[arg(long, value_delimiter = ',')]
    gateway_cors_origins: Vec<String>,

    /// Allow credentials in CORS
    #[arg(long)]
    gateway_cors_credentials: bool,

    /// Gateway rate limit: requests per second
    #[arg(long)]
    gateway_rps: Option<u32>,

    /// Gateway rate limit burst capacity
    #[arg(long)]
    gateway_rps_burst: Option<u32>,

    /// Make gateway rate limit distributed across nodes
    #[arg(long)]
    gateway_rps_distributed: bool,
}

fn parse_env_var(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got: {s}"))
}

#[derive(serde::Deserialize)]
struct ClusterNodeRegistryResponse {
    nodes: Vec<ClusterNodeRecord>,
    #[serde(default = "default_cluster_node_staleness_secs")]
    active_staleness_secs: u64,
}

fn default_cluster_node_staleness_secs() -> u64 {
    120
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

fn select_target_node_ids(
    nodes: Vec<ClusterNodeRecord>,
    upload_source_node_id: Option<&str>,
    max_staleness_secs: u64,
) -> Vec<String> {
    nodes
        .into_iter()
        .filter(|node| !node.is_stale(max_staleness_secs))
        .map(|node| node.node_id)
        .filter(|node_id| upload_source_node_id != Some(node_id.as_str()))
        .collect()
}

async fn request_per_node_manifests(
    http: &reqwest::Client,
    node_api: &str,
    sha256: &str,
    target_node_ids: &[String],
) -> Result<Vec<common::artifact_transfer::ArtifactManifestAudienceBinding>> {
    if target_node_ids.is_empty() {
        return Ok(Vec::new());
    }

    let authorize_url = format!(
        "{}/artifacts/{}/authorize",
        node_api.trim_end_matches('/'),
        sha256
    );
    let response = http
        .post(&authorize_url)
        .json(&ArtifactManifestBatchRequest {
            audiences: target_node_ids.to_vec(),
        })
        .send()
        .await?;

    if !response.status().is_success() {
        anyhow::bail!(
            "artifact manifest authorization failed: HTTP {} from {}",
            response.status(),
            authorize_url
        );
    }

    Ok(response
        .json::<ArtifactManifestBatchResponse>()
        .await?
        .manifests)
}

pub async fn run(
    args: DeployArgs,
    bus: &NatsBus,
    default_node_api: &str,
    http: &reqwest::Client,
) -> Result<()> {
    // If manifest is provided, parse it and use its values (CLI flags override manifest values)
    let manifest = if let Some(ref path) = args.manifest {
        Some(super::manifest::DeployManifest::from_toml(path)?)
    } else {
        None
    };

    // Resolve app name, version, wasm path from manifest or CLI flags
    let app_name = args
        .app
        .clone()
        .or_else(|| manifest.as_ref().map(|m| m.app.name.clone()))
        .ok_or_else(|| anyhow::anyhow!("--app is required when no manifest is provided"))?;
    let version = args
        .version
        .clone()
        .or_else(|| manifest.as_ref().map(|m| m.app.version.clone()))
        .unwrap_or_else(|| "v1".to_string());
    let namespace = if args.namespace != "default" {
        args.namespace.clone()
    } else {
        manifest
            .as_ref()
            .map(|m| m.app.namespace.clone())
            .unwrap_or_else(|| args.namespace.clone())
    };
    let wasm_path = args
        .wasm
        .clone()
        .or_else(|| manifest.as_ref().map(|m| m.app.wasm_artifact.clone()))
        .ok_or_else(|| anyhow::anyhow!("--wasm is required when no manifest is provided"))?;

    let app_id = AppId::new_namespaced(&namespace, &app_name, &version);

    // 1. Read the .wasm file
    let wasm_bytes = std::fs::read(&wasm_path)
        .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", wasm_path, e))?;

    let size_bytes = wasm_bytes.len() as u64;

    // 2. Compute SHA-256
    let sha256 = hex::encode(Sha256::digest(&wasm_bytes));
    println!("{}", "Deploying application:".bold());
    println!("  App ID:  {}", app_id.0.cyan());
    println!("  Namespace: {}", namespace.green());
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
    let upload_authorization = resp
        .json::<ArtifactUploadAuthorizationResponse>()
        .await
        .ok();
    println!("{} Artifact uploaded to {}", "✓".green(), upload_url);

    let upload_source_node_id = upload_authorization
        .as_ref()
        .and_then(|authorization| authorization.signed_get_manifest.as_ref())
        .map(|manifest| manifest.manifest.issuer.clone());

    let registered_nodes = load_cluster_node_registry(http, node_api).await?;
    let target_node_ids = select_target_node_ids(
        registered_nodes.nodes,
        upload_source_node_id.as_deref(),
        registered_nodes.active_staleness_secs,
    );

    let per_node_manifests =
        match request_per_node_manifests(http, node_api, &sha256, &target_node_ids).await {
            Ok(manifests) => manifests,
            Err(e) => {
                eprintln!(
                    "warning: failed to request per-node artifact manifests: {}",
                    e
                );
                Vec::new()
            }
        };

    // 4. Build config from manifest (if any), then overlay CLI flags
    let (config, gateway_config, api_keys) = if let Some(manifest) = manifest {
        let mut config = manifest.to_app_config();
        // Override with CLI flags
        config.id = app_id.clone();
        if args.fuel != 500_000_000 {
            config.fuel_quota = FuelQuota(args.fuel);
        }
        if args.memory_mb != 128 {
            config.memory_limit = MemoryPages(args.memory_mb * 16);
        }
        if args.max_instances != 10 {
            config.max_instances = args.max_instances;
        }
        if args.idle_timeout != 300 {
            config.idle_timeout_secs = args.idle_timeout;
        }
        if !args.env_vars.is_empty() {
            for (k, v) in &args.env_vars {
                config.env_vars.insert(k.clone(), v.clone());
            }
        }
        if !args.secret_keys.is_empty() {
            config.secret_keys = args.secret_keys.clone();
        }
        let policy = build_policy_config(&args)?;
        if policy.is_some() {
            config.policy = policy;
        }

        let gateway_config = manifest.to_gateway_config();
        (config, gateway_config, manifest.api_keys)
    } else {
        let policy = build_policy_config(&args)?;
        let gateway_config = build_gateway_config(&args);
        let config = AppConfig {
            id: app_id.clone(),
            fuel_quota: FuelQuota(args.fuel),
            memory_limit: MemoryPages(args.memory_mb * 16),
            max_instances: args.max_instances,
            idle_timeout_secs: args.idle_timeout,
            wasm_bind_port: 8080,
            env_vars: args.env_vars.into_iter().collect::<HashMap<_, _>>(),
            secret_keys: args.secret_keys,
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy,
            namespace: namespace.clone(),
        };
        (config, gateway_config, Vec::new())
    };

    if target_node_ids.is_empty() {
        eprintln!(
            "warning: the authoritative cluster node registry contains no active peer nodes beyond the upload source; no per-node artifact manifests were needed"
        );
    }

    // 5. Publish deploy event
    let event = Event::DeployApp {
        app_id: app_id.clone(),
        config,
        artifact_url,
        artifact_transfer_manifests: per_node_manifests,
        expected_hash: Some(sha256),
        size_bytes,
    };
    bus.publish(&event).await?;

    // 6. Publish gateway config if present
    if let Some(gateway_config) = gateway_config {
        let gw_event = Event::GatewayConfigUpdate {
            app_id: app_id.clone(),
            config: gateway_config,
        };
        bus.publish(&gw_event).await?;
        println!(
            "{} Gateway config published for {}",
            "✓".green(),
            app_id.0.cyan()
        );
    }

    // 7. Publish API keys if present
    if !api_keys.is_empty() {
        // API keys are stored via the admin API / node storage.
        // For now, we publish them as part of the deploy by sending
        // a direct HTTP request to the node's admin API.
        let url = format!("{}/admin/api_keys/{}", node_api, app_id.0);
        let resp = http.post(&url).json(&api_keys).send().await?;
        if resp.status().is_success() {
            println!("{} API keys stored for {}", "✓".green(), app_id.0.cyan());
        } else {
            println!("⚠ Failed to store API keys: {}", resp.status());
        }
    }

    println!(
        "{} Deploy event published for {} — all nodes are compiling.",
        "✓".green(),
        app_id.0.cyan()
    );
    Ok(())
}

fn build_gateway_config(args: &DeployArgs) -> Option<common::types::GatewayRouteConfig> {
    let mut config = common::types::GatewayRouteConfig::default();
    let mut has_config = false;

    if let Some(ref auth) = args.gateway_auth {
        config.auth = match auth.as_str() {
            "none" => common::types::AuthPolicy::None,
            "authenticated" => common::types::AuthPolicy::Authenticated,
            "roles" => common::types::AuthPolicy::Roles {
                allowed_roles: args.gateway_roles.clone(),
                client_id: args.gateway_oidc_client.clone(),
            },
            _ => common::types::AuthPolicy::None,
        };
        has_config = true;
    }

    if !args.gateway_cors_origins.is_empty() {
        config.cors = Some(common::types::CorsPolicy {
            allowed_origins: args.gateway_cors_origins.clone(),
            allowed_methods: common::types::CorsPolicy::default_methods(),
            allowed_headers: common::types::CorsPolicy::default_headers(),
            expose_headers: vec![],
            allow_credentials: args.gateway_cors_credentials,
            max_age_secs: 86400,
        });
        has_config = true;
    }

    if args.gateway_rps.is_some() || args.gateway_rps_burst.is_some() {
        config.rate_limit = Some(common::types::RouteRateLimit {
            requests_per_second: args.gateway_rps.unwrap_or(1000),
            burst_capacity: args.gateway_rps_burst.unwrap_or(50),
            distributed: args.gateway_rps_distributed,
        });
        has_config = true;
    }

    if has_config {
        Some(config)
    } else {
        None
    }
}

/// Build a `PolicyConfig` from the CLI flags.
///
/// If `--policy-profile` is given, start from that profile and overlay any
/// explicit `--policy-network-*` / `--policy-fs-*` overrides on top.
/// If no profile is given but individual flags are present, build a config
/// with only the specified overrides (defaults fill in on `resolve()`).
fn build_policy_config(args: &DeployArgs) -> Result<Option<common::policy::PolicyConfig>> {
    use common::policy::{FilesystemPolicyConfig, NetworkPolicyConfig, PolicyProfile};

    // Start from a profile if specified
    let mut config = match args.policy_profile.as_deref() {
        Some("http_api") => Some(PolicyProfile::HttpApi.to_config()),
        Some("background_worker") => Some(PolicyProfile::BackgroundWorker.to_config()),
        Some("static_site") => Some(PolicyProfile::StaticSite.to_config()),
        Some("database_proxy") => Some(PolicyProfile::DatabaseProxy.to_config()),
        Some("unrestricted") => Some(PolicyProfile::Unrestricted.to_config()),
        Some(other) => {
            anyhow::bail!(
                "Unknown policy profile '{}'. Available: http_api, background_worker, static_site, database_proxy, unrestricted",
                other
            );
        }
        None => None,
    };

    // If any individual policy flag is set, we need a config to overlay onto
    let has_network_overrides = args.policy_network_allow_outbound_tcp.is_some()
        || args.policy_network_allow_outbound_udp.is_some()
        || args.policy_network_allow_dns.is_some()
        || args.policy_network_allowed_cidrs.is_some()
        || args.policy_network_denied_cidrs.is_some()
        || args.policy_network_max_outbound_connections.is_some()
        || args.policy_network_max_egress_bytes.is_some();

    let has_fs_overrides = args.policy_fs_max_open_fds.is_some()
        || args.policy_fs_max_write_bytes.is_some()
        || args.policy_fs_allow_file_create.is_some()
        || args.policy_fs_allow_file_delete.is_some()
        || args.policy_fs_allowed_paths.is_some();

    if has_network_overrides || has_fs_overrides {
        config = Some(config.unwrap_or_default());

        let cfg = config.as_mut().unwrap();

        // Overlay network overrides
        if has_network_overrides {
            let net = cfg.network.get_or_insert_with(NetworkPolicyConfig::default);
            if let Some(v) = args.policy_network_allow_outbound_tcp {
                net.allow_outbound_tcp = Some(v);
            }
            if let Some(v) = args.policy_network_allow_outbound_udp {
                net.allow_outbound_udp = Some(v);
            }
            if let Some(v) = args.policy_network_allow_dns {
                net.allow_dns = Some(v);
            }
            if let Some(ref cidrs) = args.policy_network_allowed_cidrs {
                net.allowed_cidrs = Some(parse_cidr_list(cidrs)?);
            }
            if let Some(ref cidrs) = args.policy_network_denied_cidrs {
                net.denied_cidrs = Some(parse_cidr_list(cidrs)?);
            }
            if let Some(v) = args.policy_network_max_outbound_connections {
                net.max_outbound_connections = Some(v);
            }
            if let Some(v) = args.policy_network_max_egress_bytes {
                net.max_egress_bytes = Some(v);
            }
        }

        // Overlay filesystem overrides
        if has_fs_overrides {
            let fs = cfg
                .filesystem
                .get_or_insert_with(FilesystemPolicyConfig::default);
            if let Some(v) = args.policy_fs_max_open_fds {
                fs.max_open_fds = Some(v);
            }
            if let Some(v) = args.policy_fs_max_write_bytes {
                fs.max_fs_write_bytes = Some(v);
            }
            if let Some(v) = args.policy_fs_allow_file_create {
                fs.allow_file_create = Some(v);
            }
            if let Some(v) = args.policy_fs_allow_file_delete {
                fs.allow_file_delete = Some(v);
            }
            if let Some(ref paths) = args.policy_fs_allowed_paths {
                fs.allowed_paths = Some(paths.split(',').map(|s| s.trim().to_string()).collect());
            }
        }
    }

    // Validate CIDRs early so we reject bad deployments
    if let Some(ref cfg) = config {
        if let Some(ref net) = cfg.network {
            if let Some(ref cidrs) = net.allowed_cidrs {
                for cidr in cidrs {
                    if cidr.parse::<ipnet::IpNet>().is_err() {
                        anyhow::bail!("Invalid allowed CIDR: {:?}", cidr);
                    }
                }
            }
            if let Some(ref cidrs) = net.denied_cidrs {
                for cidr in cidrs {
                    if cidr.parse::<ipnet::IpNet>().is_err() {
                        anyhow::bail!("Invalid denied CIDR: {:?}", cidr);
                    }
                }
            }
        }
    }

    Ok(config)
}

/// Parse a comma-separated CIDR string into a Vec<String>.
fn parse_cidr_list(cidrs: &str) -> Result<Vec<String>> {
    Ok(cidrs
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
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

#[cfg(test)]
mod tests {
    use super::{
        build_gateway_config, load_cluster_node_registry, request_per_node_manifests,
        select_target_node_ids, DeployArgs,
    };
    use axum::{
        extract::State,
        routing::{get, post},
        Json, Router,
    };
    use clap::Parser;
    use common::{
        artifact_transfer::{
            ArtifactManifestBatchRequest, ArtifactManifestBatchResponse, ArtifactTransferAuthority,
        },
        health::NodeHealthStatus,
        types::ClusterNodeRecord,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct TestState {
        nodes: Vec<ClusterNodeRecord>,
        requested_audiences: Arc<Mutex<Vec<String>>>,
    }

    fn active_node(node_id: &str, last_seen_unix_secs: u64) -> ClusterNodeRecord {
        ClusterNodeRecord {
            node_id: node_id.to_string(),
            last_seen_unix_secs,
            joined_at_unix_secs: Some(last_seen_unix_secs),
            health_status: NodeHealthStatus::Healthy,
            proxy_address: Some(format!("{node_id}.internal:8080")),
            artifact_server_url: Some(format!("http://{node_id}.internal:9091")),
            protocol_version: Some(common::protocol::PROTOCOL_VERSION),
            binary_version: Some(common::protocol::BINARY_VERSION.to_string()),
            secret_transport_public_key: None,
            accepting_requests: Some(true),
            active_instances: Some(1),
            deployed_apps: Some(1),
        }
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[derive(Parser)]
    struct DeployCliTestHarness {
        #[command(flatten)]
        args: DeployArgs,
    }

    #[test]
    fn test_deploy_gateway_rate_limit_defaults_to_node_local() {
        let parsed = DeployCliTestHarness::parse_from([
            "wasm-ctl",
            "--app",
            "api",
            "--wasm",
            "./test.wasm",
            "--gateway-rps",
            "100",
            "--gateway-rps-burst",
            "20",
        ]);

        let gateway = build_gateway_config(&parsed.args).unwrap();
        assert!(!gateway.rate_limit.unwrap().distributed);
    }

    #[test]
    fn test_deploy_gateway_rate_limit_supports_explicit_distributed_opt_in() {
        let parsed = DeployCliTestHarness::parse_from([
            "wasm-ctl",
            "--app",
            "api",
            "--wasm",
            "./test.wasm",
            "--gateway-rps",
            "100",
            "--gateway-rps-burst",
            "20",
            "--gateway-rps-distributed",
        ]);

        let gateway = build_gateway_config(&parsed.args).unwrap();
        assert!(gateway.rate_limit.unwrap().distributed);
    }

    #[tokio::test]
    async fn test_registry_backed_manifest_fanout_uses_exact_active_peers() {
        let authority = ArtifactTransferAuthority::derive("node-1", &[9u8; 32]);
        let requested_audiences = Arc::new(Mutex::new(Vec::new()));
        let state = TestState {
            nodes: vec![
                active_node("node-1", now_secs()),
                active_node("node-2", now_secs()),
                active_node("node-3", now_secs().saturating_sub(10_000)),
            ],
            requested_audiences: requested_audiences.clone(),
        };

        async fn cluster_nodes(State(state): State<TestState>) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "nodes": state.nodes,
                "active_staleness_secs": 60,
            }))
        }

        async fn authorize(
            State(state): State<TestState>,
            Json(body): Json<ArtifactManifestBatchRequest>,
        ) -> Json<ArtifactManifestBatchResponse> {
            *state.requested_audiences.lock().await = body.audiences.clone();
            let authority = ArtifactTransferAuthority::derive("node-1", &[9u8; 32]);
            Json(ArtifactManifestBatchResponse {
                sha256: "abc123".to_string(),
                manifests: body
                    .audiences
                    .into_iter()
                    .map(|audience_node_id| {
                        common::artifact_transfer::ArtifactManifestAudienceBinding {
                            artifact_transfer_manifest: authority
                                .issue_read_manifest_for_audience("abc123", &audience_node_id),
                            audience_node_id,
                        }
                    })
                    .collect(),
            })
        }

        let app = Router::new()
            .route("/admin/cluster/nodes", get(cluster_nodes))
            .route("/artifacts/abc123/authorize", post(authorize))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base_url = format!("http://{}", addr);
        let registry = load_cluster_node_registry(&http, &base_url).await.unwrap();
        assert_eq!(registry.active_staleness_secs, 60);
        let target_node_ids = select_target_node_ids(
            registry.nodes,
            Some("node-1"),
            registry.active_staleness_secs,
        );
        assert_eq!(target_node_ids, vec!["node-2".to_string()]);

        let manifests = request_per_node_manifests(&http, &base_url, "abc123", &target_node_ids)
            .await
            .unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].audience_node_id, "node-2");
        assert_eq!(
            *requested_audiences.lock().await,
            vec!["node-2".to_string()]
        );
        assert_eq!(
            manifests[0].artifact_transfer_manifest.manifest.issuer,
            authority.local_node_id()
        );
    }
}
