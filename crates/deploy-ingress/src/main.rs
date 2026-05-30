use anyhow::Context;
use async_nats::jetstream;
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use clap::Parser;
use common::{
    artifact_transfer::ArtifactTransferAuthority,
    auth::{AuthConfig, Permission},
    deploy::{
        ArtifactCredentialSetRequest, ArtifactCredentialSetResponse, ArtifactVerificationRecord,
        DeployIntentRequest, DeployIntentResponse,
    },
    error::PlatformError,
    types::{AppId, ClusterNodeRecord},
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use messaging::{events::Event, NatsBus};
use node::handlers::ingest_remote_artifact;
use secrets::{
    crypto::{decrypt, encrypt, EncryptedBlob, SymmetricKey},
    LocalSecretProvider, SecretProvider,
};
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
const ACTIVE_NODE_WAIT_TIMEOUT_SECS: u64 = 10;
const DEPLOY_INGRESS_LEASE_KEY: &str = "leader";
const DEFAULT_HA_LEASE_BUCKET: &str = "DEPLOY_INGRESS_HA";
const DEFAULT_CREDENTIAL_BUCKET: &str = "DEPLOY_INGRESS_CREDENTIALS";

#[derive(Clone)]
struct SignaturePolicy {
    require_signature: bool,
    allowed_issuers: Vec<String>,
    allowed_repositories: Vec<String>,
    allowed_namespaces: Vec<String>,
}

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
    #[arg(long, env = "WASM_DEPLOY_INGRESS_HA_ENABLED", default_value_t = true)]
    ha_enabled: bool,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_BUCKET",
        default_value = DEFAULT_HA_LEASE_BUCKET
    )]
    ha_lease_bucket: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_CREDENTIAL_BUCKET",
        default_value = DEFAULT_CREDENTIAL_BUCKET
    )]
    credential_bucket: String,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS",
        default_value_t = 30
    )]
    ha_lease_ttl_secs: u64,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS",
        default_value_t = 10
    )]
    ha_lease_refresh_secs: u64,
    #[arg(
        long,
        env = "WASM_DEPLOY_INGRESS_REQUIRE_SIGNATURE",
        default_value_t = false
    )]
    require_signature: bool,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_ISSUERS")]
    allowed_issuers: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES")]
    allowed_repositories: Option<String>,
    #[arg(long, env = "WASM_DEPLOY_INGRESS_ALLOWED_NAMESPACES")]
    allowed_namespaces: Option<String>,
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
    credential_kv: jetstream::kv::Store,
    credential_kek_bytes: [u8; 32],
    ha_enabled: bool,
    leader_state: Arc<RwLock<LeaderState>>,
    signature_policy: SignaturePolicy,
}

#[derive(Debug, Clone, serde::Serialize)]
struct LeaderState {
    is_leader: bool,
    leader_ingress_id: Option<String>,
    leader_artifact_server_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LeaderLeaseRecord {
    ingress_id: String,
    artifact_server_url: String,
    updated_at_unix_secs: u64,
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
    let credential_kek_bytes = *kek.as_bytes();
    let artifact_transfer_authority =
        ArtifactTransferAuthority::derive(&args.ingress_id, kek.as_bytes());
    let secret_provider = Arc::new(LocalSecretProvider::new(
        store.clone(),
        SymmetricKey::from_bytes(credential_kek_bytes),
    ));
    let mut bus = if let Some(creds) = args.nats_creds.as_deref() {
        NatsBus::connect_secure(&args.nats_url, creds).await?
    } else {
        NatsBus::connect(&args.nats_url).await?
    };
    bus.set_node_id(args.ingress_id.clone());
    bus.setup_jetstream().await?;
    let js = jetstream::new(bus.client().clone());
    let leader_kv = get_or_create_kv(
        &js,
        &args.ha_lease_bucket,
        Some(std::time::Duration::from_secs(args.ha_lease_ttl_secs)),
    )
    .await?;
    let credential_kv = get_or_create_kv(&js, &args.credential_bucket, None).await?;

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
    let leader_state = Arc::new(RwLock::new(LeaderState {
        is_leader: !args.ha_enabled,
        leader_ingress_id: if args.ha_enabled {
            None
        } else {
            Some(args.ingress_id.clone())
        },
        leader_artifact_server_url: if args.ha_enabled {
            None
        } else {
            Some(artifact_server_url.clone())
        },
    }));

    let state = AppState {
        ingress_id: args.ingress_id.clone(),
        auth,
        store: store.clone(),
        secret_provider,
        bus,
        artifact_server_url: artifact_server_url.clone(),
        artifact_transfer_authority: artifact_transfer_authority.clone(),
        audit_path: args.audit_path.clone(),
        credential_kv,
        credential_kek_bytes,
        ha_enabled: args.ha_enabled,
        leader_state,
        signature_policy: SignaturePolicy {
            require_signature: args.require_signature,
            allowed_issuers: parse_csv_list(args.allowed_issuers.as_deref()),
            allowed_repositories: parse_csv_list(args.allowed_repositories.as_deref()),
            allowed_namespaces: parse_csv_list(args.allowed_namespaces.as_deref()),
        },
    };

    if args.ha_enabled {
        start_leader_lease_task(
            state.clone(),
            leader_kv,
            std::time::Duration::from_secs(args.ha_lease_refresh_secs),
        );
        subscribe_artifact_replication(&state).await?;
    }

    let deploy_app = Router::new()
        .route("/health", get(health))
        .route("/deploy/intent", post(deploy_intent))
        .route("/deploy/artifact-credentials", put(set_artifact_credential))
        .route(
            "/artifacts/{sha256}/verification",
            get(get_artifact_verification),
        )
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

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let leader_state = state.leader_state.read().await.clone();
    Json(serde_json::json!({
        "status": "ok",
        "ingress_id": state.ingress_id,
        "ha_enabled": state.ha_enabled,
        "is_leader": leader_state.is_leader,
        "leader_ingress_id": leader_state.leader_ingress_id,
        "leader_artifact_server_url": leader_state.leader_artifact_server_url,
    }))
}

async fn get_artifact_verification(
    State(state): State<AppState>,
    AxumPath(sha256): AxumPath<String>,
) -> impl IntoResponse {
    match state.store.load_artifact_verification(&sha256) {
        Ok(Some(record)) => (StatusCode::OK, Json(serde_json::to_value(record).unwrap())),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "artifact_verification_not_found",
                "message": format!("no verification record found for artifact {}", sha256),
            })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "artifact_verification_lookup_failed",
                "message": err.to_string(),
            })),
        ),
    }
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

    if request.method() != Method::GET
        && state.ha_enabled
        && !state.leader_state.read().await.is_leader
    {
        let leader = state.leader_state.read().await.clone();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "deploy_ingress_not_leader",
                "leader_ingress_id": leader.leader_ingress_id,
                "leader_artifact_server_url": leader.leader_artifact_server_url,
            })),
        )
            .into_response();
    }

    next.run(request).await
}

fn parse_csv_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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
    let encrypted = match encrypt_credential_value(state.credential_kek_bytes, &request.value) {
        Ok(value) => value,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "credential_encrypt_failed",
                    "message": err.to_string(),
                })),
            )
        }
    };
    if let Err(err) = state
        .credential_kv
        .put(request.key.trim(), encrypted.into_bytes().into())
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "credential_store_failed",
                "message": err.to_string(),
            })),
        );
    }
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
    let audit_app_id = request.app_id.0.clone();
    let audit_artifact_reference = request.artifact.reference.clone();
    let audit_artifact_url = request.artifact.url.clone();
    let audit_artifact_sha256 = request.artifact.sha256.clone();
    match process_deploy_intent(&state, request).await {
        Ok(response) => (
            StatusCode::ACCEPTED,
            Json(serde_json::to_value(response).unwrap()),
        ),
        Err(err) => {
            write_audit(
                &state.audit_path,
                &serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "ingress_id": state.ingress_id,
                    "event": "deploy_intent_rejected",
                    "app_id": audit_app_id,
                    "artifact_source_reference": audit_artifact_reference,
                    "artifact_source_url": audit_artifact_url,
                    "artifact_sha256": audit_artifact_sha256,
                    "message": err.to_string(),
                }),
            );
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
    ensure_local_artifact_credential(state, request.artifact.credential_ref.as_deref()).await?;
    let target_node_ids = wait_for_active_cluster_target_nodes(
        &state.store,
        &state.ingress_id,
        CLUSTER_NODE_STALE_AFTER_SECS,
    )
    .await?;

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
    let verification = verify_artifact_signature(
        &state.signature_policy,
        &ingress.expected_hash,
        &request.artifact,
    )?;
    state
        .store
        .save_artifact_verification(&ingress.expected_hash, &verification)?;
    let artifact_transfer_manifests = target_node_ids
        .into_iter()
        .map(
            |audience_node_id| common::artifact_transfer::ArtifactManifestAudienceBinding {
                artifact_transfer_manifest: state
                    .artifact_transfer_authority
                    .issue_read_manifest_for_audience(&ingress.expected_hash, &audience_node_id),
                audience_node_id,
            },
        )
        .collect::<Vec<_>>();
    publish_artifact_replication(state, &ingress).await?;

    state
        .bus
        .publish(&Event::DeployApp {
            app_id: request.app_id.clone(),
            config: request.config.clone(),
            artifact_url: ingress.artifact_url.clone(),
            artifact_transfer_manifests: artifact_transfer_manifests.clone(),
            expected_hash: Some(ingress.expected_hash.clone()),
            size_bytes: ingress.size_bytes,
        })
        .await?;

    let gateway_config_published = if let Some(gateway_config) = request.gateway_config.clone() {
        state
            .bus
            .publish(&Event::GatewayConfigUpdate {
                app_id: request.app_id.clone(),
                config: gateway_config,
            })
            .await?;
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
            "artifact_verification": verification,
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
        artifact_transfer_manifests,
        gateway_config_published,
        api_key_count: request.api_keys.len(),
    })
}

async fn wait_for_active_cluster_target_nodes(
    store: &Store,
    self_node_id: &str,
    stale_after_secs: u64,
) -> Result<Vec<String>, PlatformError> {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(ACTIVE_NODE_WAIT_TIMEOUT_SECS);
    loop {
        let active_target_nodes = store
            .list_cluster_nodes()?
            .into_iter()
            .filter(|node| !node.is_stale(stale_after_secs))
            .map(|node| node.node_id)
            .filter(|node_id| node_id != self_node_id)
            .collect::<Vec<_>>();
        if !active_target_nodes.is_empty() {
            return Ok(active_target_nodes);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(PlatformError::external(
                "no active target cluster nodes registered in deploy ingress before deploy intent timeout",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn ensure_local_artifact_credential(
    state: &AppState,
    credential_ref: Option<&str>,
) -> Result<(), PlatformError> {
    let Some(credential_ref) = credential_ref else {
        return Ok(());
    };
    let app_id = AppId(ARTIFACT_CREDENTIALS_APP_ID.to_string());
    if state
        .secret_provider
        .get(&app_id, credential_ref)
        .await
        .is_ok()
    {
        return Ok(());
    }
    let Some(entry) = state
        .credential_kv
        .entry(credential_ref)
        .await
        .map_err(PlatformError::messaging_source)?
    else {
        return Err(PlatformError::security(format!(
            "artifact credential '{}' not found in shared deploy-ingress credential store",
            credential_ref
        )));
    };
    let encrypted = std::str::from_utf8(entry.value.as_ref()).map_err(|e| {
        PlatformError::encryption_with_msg("artifact credential entry is not utf-8", e)
    })?;
    let plaintext = decrypt_credential_value(state.credential_kek_bytes, encrypted)?;
    state
        .secret_provider
        .set(&app_id, credential_ref, &plaintext)
        .await
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

fn verify_artifact_signature(
    policy: &SignaturePolicy,
    artifact_sha256: &str,
    artifact: &common::deploy::RemoteArtifactSource,
) -> Result<ArtifactVerificationRecord, PlatformError> {
    let signature = artifact.signature.as_ref();
    if policy.require_signature && signature.is_none() {
        return Err(PlatformError::security(
            "artifact signature is required by deploy-ingress policy",
        ));
    }

    let Some(signature_meta) = signature else {
        return Ok(ArtifactVerificationRecord {
            sha256: artifact_sha256.to_string(),
            verified: false,
            algorithm: None,
            issuer: None,
            repository: None,
            namespace: None,
            public_key_sha256: None,
            verified_at_unix_secs: now_unix_secs(),
        });
    };

    if signature_meta.algorithm.to_lowercase() != "ed25519" {
        return Err(PlatformError::security(format!(
            "unsupported artifact signature algorithm: {}",
            signature_meta.algorithm
        )));
    }

    enforce_allowed_claim(
        "issuer",
        signature_meta.issuer.as_deref(),
        &policy.allowed_issuers,
    )?;
    enforce_allowed_claim(
        "repository",
        signature_meta.repository.as_deref(),
        &policy.allowed_repositories,
    )?;
    enforce_allowed_claim(
        "namespace",
        signature_meta.namespace.as_deref(),
        &policy.allowed_namespaces,
    )?;

    let public_key_bytes = STANDARD.decode(&signature_meta.public_key).map_err(|e| {
        PlatformError::security(format!(
            "invalid artifact signature public key encoding: {e}"
        ))
    })?;
    let signature_bytes = STANDARD.decode(&signature_meta.signature).map_err(|e| {
        PlatformError::security(format!("invalid artifact signature encoding: {e}"))
    })?;

    let public_key_sha256 = hex::encode(sha2::Sha256::digest(&public_key_bytes));
    let public_key: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| PlatformError::security("artifact signature public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|e| PlatformError::security(format!("invalid Ed25519 public key: {e}")))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|e| PlatformError::security(format!("invalid Ed25519 signature: {e}")))?;

    let claims = serde_json::to_vec(&serde_json::json!({
        "sha256": artifact_sha256,
        "issuer": signature_meta.issuer,
        "repository": signature_meta.repository,
        "namespace": signature_meta.namespace,
    }))
    .map_err(|e| {
        PlatformError::internal(format!(
            "failed to serialize artifact signature claims: {e}"
        ))
    })?;

    verifying_key.verify(&claims, &signature).map_err(|e| {
        PlatformError::security(format!("artifact signature verification failed: {e}"))
    })?;

    Ok(ArtifactVerificationRecord {
        sha256: artifact_sha256.to_string(),
        verified: true,
        algorithm: Some("ed25519".to_string()),
        issuer: signature_meta.issuer.clone(),
        repository: signature_meta.repository.clone(),
        namespace: signature_meta.namespace.clone(),
        public_key_sha256: Some(public_key_sha256),
        verified_at_unix_secs: now_unix_secs(),
    })
}

fn enforce_allowed_claim(
    claim_name: &str,
    value: Option<&str>,
    allowed: &[String],
) -> Result<(), PlatformError> {
    if allowed.is_empty() {
        return Ok(());
    }
    let Some(value) = value else {
        return Err(PlatformError::security(format!(
            "artifact signature is missing required {} claim",
            claim_name
        )));
    };
    if allowed.iter().any(|candidate| candidate == value) {
        return Ok(());
    }
    Err(PlatformError::security(format!(
        "artifact signature {} claim '{}' is not allowed by deploy-ingress policy",
        claim_name, value
    )))
}

async fn publish_artifact_replication(
    state: &AppState,
    ingress: &common::deploy::RemoteArtifactIngressResponse,
) -> Result<(), PlatformError> {
    if !state.ha_enabled {
        return Ok(());
    }
    state
        .bus
        .publish(&Event::DeployIngressArtifactReplicated {
            source_ingress_id: state.ingress_id.clone(),
            artifact_url: ingress.artifact_url.clone(),
            expected_hash: ingress.expected_hash.clone(),
            size_bytes: ingress.size_bytes,
        })
        .await
}

async fn subscribe_artifact_replication(state: &AppState) -> Result<(), PlatformError> {
    let state = state.clone();
    let consumer_name = format!("deploy_ingress_replication_{}", state.ingress_id);
    let bus = state.bus.clone();
    bus.subscribe_durable(
        "PLATFORM",
        &consumer_name,
        Some("platform.deploy_ingress.artifact.>"),
        move |event| {
            let state = state.clone();
            async move {
                if let Event::DeployIngressArtifactReplicated {
                    source_ingress_id,
                    artifact_url,
                    expected_hash,
                    ..
                } = event
                {
                    if source_ingress_id != state.ingress_id {
                        replicate_artifact_locally(&state, &artifact_url, &expected_hash).await?;
                    }
                }
                Ok(())
            }
        },
    )
    .await
}

async fn replicate_artifact_locally(
    state: &AppState,
    artifact_url: &str,
    expected_hash: &str,
) -> Result<(), PlatformError> {
    if state.store.raw_wasm_exists(expected_hash)? {
        return Ok(());
    }
    let response = reqwest::Client::new()
        .get(artifact_url)
        .send()
        .await
        .map_err(PlatformError::external_source)?;
    if !response.status().is_success() {
        return Err(PlatformError::external(format!(
            "artifact replication fetch failed with HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(PlatformError::external_source)?;
    let actual_hash = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_hash != expected_hash {
        return Err(PlatformError::security(format!(
            "artifact replication hash mismatch: expected {}, got {}",
            expected_hash, actual_hash
        )));
    }
    state.store.save_raw_wasm(expected_hash, &bytes)?;
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

async fn get_or_create_kv(
    js: &jetstream::Context,
    bucket: &str,
    max_age: Option<std::time::Duration>,
) -> anyhow::Result<jetstream::kv::Store> {
    match js
        .create_key_value(jetstream::kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            max_age: max_age.unwrap_or_default(),
            ..Default::default()
        })
        .await
    {
        Ok(store) => Ok(store),
        Err(_) => js
            .get_key_value(bucket)
            .await
            .with_context(|| format!("failed to create or load KV bucket {bucket}")),
    }
}

fn start_leader_lease_task(
    state: AppState,
    leader_kv: jetstream::kv::Store,
    refresh_interval: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut last_revision: Option<u64> = None;
        loop {
            if let Err(err) = renew_or_observe_leader(&state, &leader_kv, &mut last_revision).await
            {
                warn!(error = %err, "deploy ingress leader lease update failed");
            }
            tokio::time::sleep(refresh_interval).await;
        }
    });
}

async fn renew_or_observe_leader(
    state: &AppState,
    leader_kv: &jetstream::kv::Store,
    last_revision: &mut Option<u64>,
) -> Result<(), PlatformError> {
    let record = LeaderLeaseRecord {
        ingress_id: state.ingress_id.clone(),
        artifact_server_url: state.artifact_server_url.clone(),
        updated_at_unix_secs: now_unix_secs(),
    };
    let payload = serde_json::to_vec(&record)?;

    if let Some(revision) = *last_revision {
        match leader_kv
            .update(DEPLOY_INGRESS_LEASE_KEY, payload.clone().into(), revision)
            .await
        {
            Ok(new_revision) => {
                *last_revision = Some(new_revision);
                set_leader_state(state, &record, true).await;
                return Ok(());
            }
            Err(err) => {
                warn!(error = %err, "deploy ingress leader lease renew lost; observing current leader");
                *last_revision = None;
            }
        }
    }

    match leader_kv
        .create(DEPLOY_INGRESS_LEASE_KEY, payload.into())
        .await
    {
        Ok(revision) => {
            *last_revision = Some(revision);
            set_leader_state(state, &record, true).await;
            Ok(())
        }
        Err(_) => {
            let current = leader_kv
                .entry(DEPLOY_INGRESS_LEASE_KEY)
                .await
                .map_err(PlatformError::messaging_source)?;
            if let Some(entry) = current {
                let leader: LeaderLeaseRecord = serde_json::from_slice(entry.value.as_ref())?;
                if leader.ingress_id == state.ingress_id {
                    *last_revision = Some(entry.revision);
                    set_leader_state(state, &leader, true).await;
                } else {
                    *last_revision = None;
                    set_leader_state(state, &leader, false).await;
                }
            } else {
                set_unknown_leader_state(state).await;
            }
            Ok(())
        }
    }
}

async fn set_leader_state(state: &AppState, leader: &LeaderLeaseRecord, is_leader: bool) {
    let mut guard = state.leader_state.write().await;
    guard.is_leader = is_leader;
    guard.leader_ingress_id = Some(leader.ingress_id.clone());
    guard.leader_artifact_server_url = Some(leader.artifact_server_url.clone());
}

async fn set_unknown_leader_state(state: &AppState) {
    let mut guard = state.leader_state.write().await;
    guard.is_leader = false;
    guard.leader_ingress_id = None;
    guard.leader_artifact_server_url = None;
}

fn encrypt_credential_value(kek_bytes: [u8; 32], value: &str) -> Result<String, PlatformError> {
    let key = SymmetricKey::from_bytes(kek_bytes);
    let encrypted = encrypt(&key, value.as_bytes())?;
    Ok(hex::encode(encrypted.0))
}

fn decrypt_credential_value(
    kek_bytes: [u8; 32],
    encrypted_hex: &str,
) -> Result<String, PlatformError> {
    let key = SymmetricKey::from_bytes(kek_bytes);
    let bytes = hex::decode(encrypted_hex)
        .map_err(|e| PlatformError::encryption_with_msg("invalid credential ciphertext hex", e))?;
    let plaintext = decrypt(&key, &EncryptedBlob(bytes))?;
    String::from_utf8(plaintext).map_err(|e| {
        PlatformError::encryption_with_msg("credential plaintext is not valid utf-8", e)
    })
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

#[cfg(test)]
mod tests {
    use super::{verify_artifact_signature, SignaturePolicy};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use common::deploy::{ArtifactSignature, RemoteArtifactSource};
    use ed25519_dalek::{Signer, SigningKey};

    fn sign_artifact_claims(
        sha256: &str,
        issuer: Option<&str>,
        repository: Option<&str>,
        namespace: Option<&str>,
    ) -> ArtifactSignature {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let claims = serde_json::to_vec(&serde_json::json!({
            "sha256": sha256,
            "issuer": issuer,
            "repository": repository,
            "namespace": namespace,
        }))
        .unwrap();
        let signature = signing_key.sign(&claims);
        ArtifactSignature {
            algorithm: "ed25519".to_string(),
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
            signature: STANDARD.encode(signature.to_bytes()),
            issuer: issuer.map(ToOwned::to_owned),
            repository: repository.map(ToOwned::to_owned),
            namespace: namespace.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn test_verify_artifact_signature_accepts_valid_signed_claims() {
        let sha256 = "ab".repeat(32);
        let signature = sign_artifact_claims(
            &sha256,
            Some("https://token.actions.githubusercontent.com"),
            Some("example-org/hello-api"),
            Some("production"),
        );
        let artifact = RemoteArtifactSource {
            reference: Some("oci://ghcr.io/example-org/hello-api:v1".to_string()),
            url: String::new(),
            sha256: String::new(),
            credential_ref: Some("ghcr-reader".to_string()),
            signature: Some(signature),
        };
        let policy = SignaturePolicy {
            require_signature: true,
            allowed_issuers: vec!["https://token.actions.githubusercontent.com".to_string()],
            allowed_repositories: vec!["example-org/hello-api".to_string()],
            allowed_namespaces: vec!["production".to_string()],
        };

        let record = verify_artifact_signature(&policy, &sha256, &artifact).unwrap();
        assert!(record.verified);
        assert_eq!(
            record.issuer.as_deref(),
            Some("https://token.actions.githubusercontent.com")
        );
    }

    #[test]
    fn test_verify_artifact_signature_rejects_disallowed_repository() {
        let sha256 = "cd".repeat(32);
        let signature = sign_artifact_claims(
            &sha256,
            Some("issuer-a"),
            Some("example-org/other-api"),
            Some("production"),
        );
        let artifact = RemoteArtifactSource {
            reference: Some("oci://ghcr.io/example-org/other-api:v1".to_string()),
            url: String::new(),
            sha256: String::new(),
            credential_ref: None,
            signature: Some(signature),
        };
        let policy = SignaturePolicy {
            require_signature: true,
            allowed_issuers: vec!["issuer-a".to_string()],
            allowed_repositories: vec!["example-org/hello-api".to_string()],
            allowed_namespaces: vec![],
        };

        let err = verify_artifact_signature(&policy, &sha256, &artifact).unwrap_err();
        assert!(err.to_string().contains("repository"));
    }
}
