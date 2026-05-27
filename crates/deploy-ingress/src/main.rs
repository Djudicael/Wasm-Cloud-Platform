use anyhow::Context;
use axum::{
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use clap::Parser;
use common::{
    artifact_transfer::ArtifactTransferAuthority,
    auth::{AuthConfig, Permission},
    deploy::{
        ArtifactCredentialSetRequest, ArtifactCredentialSetResponse, DeployIntentRequest,
        DeployIntentResponse,
    },
    error::PlatformError,
    types::{AppId, ClusterNodeRecord},
};
use messaging::{events::Event, NatsBus};
use node::handlers::ingest_remote_artifact;
use secrets::{crypto::SymmetricKey, LocalSecretProvider, SecretProvider};
use sha2::Digest;
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use storage::{artifact_server::ArtifactPeerTokenConfig, Store};
use tokio::sync::RwLock;
use tracing::{info, warn};

const ARTIFACT_CREDENTIALS_APP_ID: &str = "_platform/artifact-credentials:v1";
const CLUSTER_NODE_STALE_AFTER_SECS: u64 = 120;

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_ID",
        default_value = "deploy-ingress-0"
    )]
    ingress_id: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_NATS_URL",
        default_value = "nats://127.0.0.1:4222"
    )]
    nats_url: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_NATS_CREDS")]
    nats_creds: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_DB_PATH",
        default_value = "/tmp/wasm-deploy-ingress/state.redb"
    )]
    db_path: PathBuf,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_BIND_ADDRESS",
        default_value = "127.0.0.1"
    )]
    bind_address: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_PORT", default_value_t = 9092)]
    deploy_port: u16,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_ARTIFACT_PORT",
        default_value_t = 9091
    )]
    artifact_port: u16,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ADVERTISED_ARTIFACT_URL")]
    advertised_artifact_url: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_AUTH_ENABLED",
        default_value_t = false
    )]
    auth_enabled: bool,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_AUTH_READ_TOKEN")]
    auth_read_token: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_AUTH_WRITE_TOKEN")]
    auth_write_token: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_KEY_SOURCE",
        default_value = "generate"
    )]
    key_source: String,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_KEY_FILE")]
    key_file: Option<String>,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_AUDIT_PATH",
        default_value = "/tmp/wasm-deploy-ingress/audit.jsonl"
    )]
    audit_path: PathBuf,
}

#[derive(Clone)]
struct AppState {
    ingress_id: String,
    auth: Arc<RwLock<AuthConfig>>,
    store: Store,
    secret_provider: Arc<LocalSecretProvider>,
    bus: NatsBus,
    artifact_server_url: String,
    artifact_transfer_authority: ArtifactTransferAuthority,
    audit_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "deploy_ingress=info,messaging=info,storage=info".to_string()),
        )
        .with_target(true)
        .init();

    let args = Args::parse();
    let store = Store::open(&args.db_path)?;
    let kek = load_kek(&args)?;
    let artifact_transfer_authority =
        ArtifactTransferAuthority::derive(&args.ingress_id, kek.as_bytes());
    let secret_provider = Arc::new(LocalSecretProvider::new(store.clone(), kek));
    let mut bus = if let Some(creds) = args.nats_creds.as_deref() {
        NatsBus::connect_secure(&args.nats_url, creds).await?
    } else {
        NatsBus::connect(&args.nats_url).await?
    };
    bus.set_node_id(args.ingress_id.clone());
    bus.setup_jetstream().await?;

    let artifact_server_url = args
        .advertised_artifact_url
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", args.artifact_port));
    let auth = Arc::new(RwLock::new(AuthConfig {
        enabled: args.auth_enabled,
        read_token: args.auth_read_token.clone(),
        write_token: args.auth_write_token.clone(),
        require_tls: false,
        ..Default::default()
    }));
    auth.read().await.validate().map_err(anyhow::Error::msg)?;

    subscribe_cluster_updates(&bus, store.clone()).await?;

    let state = AppState {
        ingress_id: args.ingress_id.clone(),
        auth,
        store: store.clone(),
        secret_provider,
        bus,
        artifact_server_url: artifact_server_url.clone(),
        artifact_transfer_authority: artifact_transfer_authority.clone(),
        audit_path: args.audit_path.clone(),
    };

    let deploy_app = Router::new()
        .route("/health", get(health))
        .route("/deploy/intent", post(deploy_intent))
        .route("/deploy/artifact-credentials", put(set_artifact_credential))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024));

    let artifact_app = storage::artifact_server::artifact_router(
        store,
        Vec::<ArtifactPeerTokenConfig>::new(),
        Some(artifact_transfer_authority),
    );

    let deploy_addr = socket_addr(&args.bind_address, args.deploy_port)?;
    let artifact_addr = socket_addr(&args.bind_address, args.artifact_port)?;

    info!(
        ingress_id = %args.ingress_id,
        deploy_addr = %deploy_addr,
        artifact_addr = %artifact_addr,
        artifact_server_url = %artifact_server_url,
        "deploy ingress starting"
    );

    let deploy_listener = tokio::net::TcpListener::bind(deploy_addr).await?;
    let artifact_listener = tokio::net::TcpListener::bind(artifact_addr).await?;

    tokio::try_join!(
        async move {
            axum::serve(
                deploy_listener,
                deploy_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .context("deploy ingress server failed")
        },
        async move {
            axum::serve(
                artifact_listener,
                artifact_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .context("deploy ingress artifact server failed")
        }
    )?;

    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    let auth = state.auth.read().await;
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let result = auth.authenticate(auth_header);
    let required = if request.method() == Method::GET {
        Permission::Read
    } else {
        Permission::Write
    };
    if result.permission < required {
        return (
            if result.permission == Permission::None {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            },
            Json(serde_json::json!({
                "error": if result.permission == Permission::None { "unauthorized" } else { "forbidden" },
            })),
        )
            .into_response();
    }

    next.run(request).await
}

async fn set_artifact_credential(
    State(state): State<AppState>,
    Json(request): Json<ArtifactCredentialSetRequest>,
) -> impl IntoResponse {
    if request.key.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "key must not be empty" })),
        );
    }

    let app_id = AppId(ARTIFACT_CREDENTIALS_APP_ID.to_string());
    match state
        .secret_provider
        .set(&app_id, request.key.trim(), &request.value)
        .await
    {
        Ok(()) => {
            write_audit(
                &state.audit_path,
                &serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "ingress_id": state.ingress_id,
                    "event": "artifact_credential_set",
                    "key": request.key.trim(),
                }),
            );
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(ArtifactCredentialSetResponse {
                        key: request.key.trim().to_string(),
                    })
                    .unwrap(),
                ),
            )
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "credential_store_failed",
                "message": err.to_string(),
            })),
        ),
    }
}

async fn deploy_intent(
    State(state): State<AppState>,
    Json(request): Json<DeployIntentRequest>,
) -> impl IntoResponse {
    match process_deploy_intent(&state, request).await {
        Ok(response) => (
            StatusCode::ACCEPTED,
            Json(serde_json::to_value(response).unwrap()),
        ),
        Err(err) => {
            let status = match &err {
                PlatformError::ConfigValidation(_) => StatusCode::BAD_REQUEST,
                PlatformError::Security(_) => StatusCode::FORBIDDEN,
                PlatformError::External { .. } => StatusCode::BAD_GATEWAY,
                PlatformError::Storage { .. }
                | PlatformError::Internal(_)
                | PlatformError::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(serde_json::json!({
                    "error": "deploy_intent_failed",
                    "message": err.to_string(),
                })),
            )
        }
    }
}

async fn process_deploy_intent(
    state: &AppState,
    request: DeployIntentRequest,
) -> Result<DeployIntentResponse, PlatformError> {
    validate_deploy_intent_request(&request)?;

    let ingress = ingest_remote_artifact(
        &state.store,
        state.secret_provider.as_ref(),
        &state.artifact_server_url,
        &state.artifact_transfer_authority,
        &state.ingress_id,
        CLUSTER_NODE_STALE_AFTER_SECS,
        request.artifact.clone(),
    )
    .await?;

    state
        .bus
        .publish(&Event::DeployApp {
            app_id: request.app_id.clone(),
            config: request.config.clone(),
            artifact_url: ingress.artifact_url.clone(),
            artifact_transfer_manifests: ingress.artifact_transfer_manifests.clone(),
            expected_hash: Some(ingress.expected_hash.clone()),
            size_bytes: ingress.size_bytes,
        })
        .await
        .map_err(PlatformError::from)?;

    let gateway_config_published = if let Some(gateway_config) = request.gateway_config.clone() {
        state
            .bus
            .publish(&Event::GatewayConfigUpdate {
                app_id: request.app_id.clone(),
                config: gateway_config,
            })
            .await
            .map_err(PlatformError::from)?;
        true
    } else {
        false
    };

    if !request.api_keys.is_empty() {
        state
            .store
            .save_api_keys(&request.app_id.0, &request.api_keys)?;
        let _ = state
            .bus
            .publish(&Event::GatewayConfigUpdate {
                app_id: request.app_id.clone(),
                config: request.gateway_config.clone().unwrap_or_default(),
            })
            .await;
    }

    write_audit(
        &state.audit_path,
        &serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "ingress_id": state.ingress_id,
            "event": "deploy_intent_accepted",
            "app_id": request.app_id.0,
            "artifact_url": ingress.artifact_url,
            "expected_hash": ingress.expected_hash,
            "size_bytes": ingress.size_bytes,
            "gateway_config_published": gateway_config_published,
            "api_key_count": request.api_keys.len(),
            "artifact_source_reference": request.artifact.reference,
            "artifact_source_url": request.artifact.url,
        }),
    );

    Ok(DeployIntentResponse {
        app_id: request.app_id,
        artifact_url: ingress.artifact_url,
        expected_hash: ingress.expected_hash,
        size_bytes: ingress.size_bytes,
        source_node_id: ingress.source_node_id,
        artifact_transfer_manifests: ingress.artifact_transfer_manifests,
        gateway_config_published,
        api_key_count: request.api_keys.len(),
    })
}

fn validate_deploy_intent_request(request: &DeployIntentRequest) -> Result<(), PlatformError> {
    if request.app_id != request.config.id {
        return Err(PlatformError::config_validation(format!(
            "deploy intent app_id {} does not match config.id {}",
            request.app_id.0, request.config.id.0
        )));
    }
    if request.config.namespace.trim() != request.app_id.namespace() {
        return Err(PlatformError::config_validation(format!(
            "deploy intent namespace {} does not match app_id namespace {}",
            request.config.namespace,
            request.app_id.namespace()
        )));
    }
    if request
        .config
        .secret_keys
        .iter()
        .any(|key| key.trim().is_empty() || key.contains('='))
    {
        return Err(PlatformError::config_validation(
            "deploy intent secret references must be non-empty names, not inline values",
        ));
    }
    if request.artifact.reference.is_none() && request.artifact.sha256.trim().is_empty() {
        return Err(PlatformError::config_validation(
            "remote HTTP artifact sources require sha256",
        ));
    }
    Ok(())
}

async fn subscribe_cluster_updates(bus: &NatsBus, store: Store) -> Result<(), PlatformError> {
    let store_for_load = store.clone();
    bus.subscribe("node.load.>", move |event| {
        let store = store_for_load.clone();
        async move {
            if let Event::NodeLoad {
                node_id,
                active_instances,
                proxy_address,
                ..
            } = event
            {
                let now = now_unix_secs();
                let mut record = store
                    .load_cluster_node(&node_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| ClusterNodeRecord {
                        node_id: node_id.clone(),
                        last_seen_unix_secs: now,
                        joined_at_unix_secs: Some(now),
                        health_status: common::health::NodeHealthStatus::Healthy,
                        proxy_address: Some(proxy_address.clone()),
                        artifact_server_url: None,
                        protocol_version: Some(common::protocol::PROTOCOL_VERSION),
                        binary_version: None,
                        secret_transport_public_key: None,
                        accepting_requests: Some(true),
                        active_instances: Some(active_instances),
                        deployed_apps: None,
                    });
                record.last_seen_unix_secs = now;
                record.proxy_address = Some(proxy_address);
                record.active_instances = Some(active_instances);
                let _ = store.save_cluster_node(&record);
            }
        }
    })
    .await?;

    let store_for_join = store.clone();
    bus.subscribe("cluster.node_joined.>", move |event| {
        let store = store_for_join.clone();
        async move {
            if let Event::NodeJoined {
                node_id,
                artifact_server_url,
                protocol_version,
                binary_version,
                ..
            } = event
            {
                let now = now_unix_secs();
                let mut record = store
                    .load_cluster_node(&node_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| ClusterNodeRecord {
                        node_id: node_id.clone(),
                        last_seen_unix_secs: now,
                        joined_at_unix_secs: Some(now),
                        health_status: common::health::NodeHealthStatus::Healthy,
                        proxy_address: None,
                        artifact_server_url: Some(artifact_server_url.clone()),
                        protocol_version: Some(protocol_version),
                        binary_version: Some(binary_version.clone()),
                        secret_transport_public_key: None,
                        accepting_requests: Some(true),
                        active_instances: Some(0),
                        deployed_apps: Some(0),
                    });
                record.last_seen_unix_secs = now;
                record.artifact_server_url = Some(artifact_server_url);
                record.protocol_version = Some(protocol_version);
                if !binary_version.trim().is_empty() {
                    record.binary_version = Some(binary_version);
                }
                let _ = store.save_cluster_node(&record);
            }
        }
    })
    .await?;

    Ok(())
}

fn load_kek(args: &Args) -> anyhow::Result<SymmetricKey> {
    match args.key_source.as_str() {
        "generate" => Ok(SymmetricKey::generate()),
        "file" => {
            let key_file = args
                .key_file
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("key_source=file requires --key-file"))?;
            let bytes = std::fs::read(key_file)
                .with_context(|| format!("failed to read key file {key_file}"))?;
            symm_key_from_exact_32(&bytes, &format!("key file {key_file}"))
        }
        spec if spec.starts_with("env:") => load_kek_from_env_spec(spec),
        spec if spec.starts_with("passphrase-env:") => {
            let passphrase = load_passphrase_from_env_spec(spec)?;
            let digest = sha2::Sha256::digest(passphrase.as_bytes());
            symm_key_from_exact_32(&digest[..32], "passphrase-env digest")
        }
        other => Err(anyhow::anyhow!(
            "unsupported key_source '{}'; supported values are generate, file, env:VAR, passphrase-env:VAR",
            other
        )),
    }
}

fn symm_key_from_exact_32(bytes: &[u8], source: &str) -> anyhow::Result<SymmetricKey> {
    if bytes.len() != 32 {
        anyhow::bail!(
            "{source} must contain exactly 32 bytes, found {} bytes",
            bytes.len()
        );
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Ok(SymmetricKey::from_bytes(key))
}

fn load_kek_from_env_spec(spec: &str) -> anyhow::Result<SymmetricKey> {
    let var_name = spec
        .strip_prefix("env:")
        .ok_or_else(|| anyhow::anyhow!("invalid env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    let trimmed = raw.trim();
    if trimmed.len() == 64 {
        let decoded =
            hex::decode(trimmed).map_err(|e| anyhow::anyhow!("failed to decode hex KEK: {e}"))?;
        return symm_key_from_exact_32(&decoded, &format!("environment variable {var_name}"));
    }
    symm_key_from_exact_32(raw.as_bytes(), &format!("environment variable {var_name}"))
}

fn load_passphrase_from_env_spec(spec: &str) -> anyhow::Result<String> {
    let var_name = spec
        .strip_prefix("passphrase-env:")
        .ok_or_else(|| anyhow::anyhow!("invalid passphrase env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    if raw.trim().is_empty() {
        anyhow::bail!("environment variable {var_name} must not be empty");
    }
    Ok(raw)
}

fn socket_addr(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("invalid bind address {host}"))?;
    Ok(SocketAddr::new(ip, port))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_audit(path: &Path, payload: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            warn!(error = %err, path = %parent.display(), "failed to create audit directory");
            return;
        }
    }
    match serde_json::to_string(payload) {
        Ok(line) => {
            if let Err(err) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| {
                    use std::io::Write;
                    writeln!(file, "{line}")
                })
            {
                warn!(error = %err, path = %path.display(), "failed to append deploy ingress audit record");
            }
        }
        Err(err) => warn!(error = %err, "failed to serialize deploy ingress audit record"),
    }
}
