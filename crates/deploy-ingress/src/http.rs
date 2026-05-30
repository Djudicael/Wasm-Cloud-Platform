use crate::{
    app::{
        AppState, ArtifactReferencePolicy, ACTIVE_NODE_WAIT_TIMEOUT_SECS,
        ARTIFACT_CREDENTIALS_APP_ID, CLUSTER_NODE_STALE_AFTER_SECS,
    },
    audit::write_audit,
    crypto::{decrypt_credential_value, encrypt_credential_value},
    ha::publish_artifact_replication,
    signature::verify_artifact_signature,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use common::{
    auth::Permission,
    deploy::{
        ArtifactCredentialSetRequest, ArtifactCredentialSetResponse, DeployIntentRequest,
        DeployIntentResponse,
    },
    error::PlatformError,
    types::AppId,
};
use messaging::events::Event;
use node::handlers::{ingest_remote_artifact, oci_reference_is_digest_pinned};
use secrets::SecretProvider;
use storage::Store;

pub fn router(state: AppState) -> Router {
    Router::new()
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
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
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
    validate_deploy_intent_request(&state.artifact_reference_policy, &request)?;
    ensure_local_artifact_credential(state, request.artifact.credential_ref.as_deref()).await?;
    let target_node_ids = wait_for_active_cluster_target_nodes(
        &state.store,
        &state.ingress_id,
        CLUSTER_NODE_STALE_AFTER_SECS,
    )
    .await?;

    // Deploy ingress normalizes remote fetch, verification, and internal distribution so
    // CI only submits an intent while the platform owns the actual artifact movement.
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

fn validate_deploy_intent_request(
    artifact_reference_policy: &ArtifactReferencePolicy,
    request: &DeployIntentRequest,
) -> Result<(), PlatformError> {
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
    if artifact_reference_policy.require_oci_digest_refs {
        if let Some(reference) = request.artifact.reference.as_deref() {
            if !oci_reference_is_digest_pinned(reference)? {
                return Err(PlatformError::security(
                    "deploy-ingress policy requires OCI artifact references to be digest-pinned",
                ));
            }
        }
    }
    Ok(())
}
