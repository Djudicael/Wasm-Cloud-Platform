use common::{
    artifact_transfer::{
        ArtifactManifestAudienceBinding, ArtifactTransferAuthority,
        BootstrapArtifactFetchAuthorization, SignedArtifactTransferManifest,
        ARTIFACT_TRANSFER_MANIFEST_HEADER, ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER,
    },
    deploy::{
        DeployIntentRequest, DeployIntentResponse, RemoteArtifactIngressResponse,
        RemoteArtifactSource,
    },
    error::PlatformError,
    types::{AppId, ClusterNodeRecord},
};
use messaging::events::Event;
use proxy::dns_webhook::DnsWebhookManager;
use proxy::node_table::NodeLoadTable;
use proxy::router::HostRouter;
use proxy::upstream::UpstreamRegistry;
use reqwest::Url;
use runtime::WasmRuntime;
use secrets::{
    encrypt_for_peer, BootstrapKeyPair, SecretProvider, SecretTransportEntry,
    SecretTransportEnvelope, SecretTransportPayload,
};
use sha2::Digest;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use storage::Store;
use supervisor::Supervisor;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

const ARTIFACT_CREDENTIALS_APP_ID: &str = "_platform/artifact-credentials:v1";
const OCI_ACCEPT_HEADER: &str = concat!(
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.artifact.manifest.v1+json"
);
const MAX_REMOTE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

fn is_loopback_artifact_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .map(|host| {
            let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
            trimmed.eq_ignore_ascii_case("localhost")
                || trimmed
                    .parse::<IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn validate_peer_artifact_url(
    new_node_id: &str,
    peer_artifact_url: &str,
) -> Result<(), PlatformError> {
    if is_loopback_artifact_url(peer_artifact_url) {
        return Err(PlatformError::config_validation(format!(
            "node {} advertised loopback artifact URL {} which is invalid for remote cluster exchange",
            new_node_id, peer_artifact_url
        )));
    }
    Ok(())
}

pub(crate) const BOOTSTRAP_PENDING_META_KEY: &str = "bootstrap.pending_session";
pub(crate) const BOOTSTRAP_APPLIED_META_KEY: &str = "bootstrap.applied_session";

pub(crate) struct BootstrapSessionState {
    pub session_id: String,
    pub nonce: String,
    pub keypair: BootstrapKeyPair,
    pub applied: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BootstrapSessionRecord {
    session_id: String,
    nonce: String,
    applied_at_ms: Option<u64>,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn merge_cluster_node_record(
    existing: Option<ClusterNodeRecord>,
    node_id: &str,
) -> ClusterNodeRecord {
    existing.unwrap_or_else(|| ClusterNodeRecord::new(node_id.to_string(), now_unix_secs()))
}

fn artifact_credentials_app_id() -> AppId {
    AppId(ARTIFACT_CREDENTIALS_APP_ID.to_string())
}

async fn resolve_artifact_authorization_header(
    secret_provider: &dyn SecretProvider,
    credential_ref: &str,
) -> Result<String, PlatformError> {
    let app_id = artifact_credentials_app_id();
    let value = secret_provider.get(&app_id, credential_ref).await?;
    let trimmed = value.trim();
    if let Some(header) = trimmed.strip_prefix("authorization:") {
        return Ok(header.trim().to_string());
    }
    Ok(format!("Bearer {trimmed}"))
}

fn trim_artifact_base_url(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

fn registry_base_url(registry: &str) -> String {
    let host = registry
        .split(':')
        .next()
        .unwrap_or(registry)
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    let scheme = if is_loopback { "http" } else { "https" };
    format!("{scheme}://{registry}")
}

#[derive(Debug, Clone)]
struct OciReference {
    registry: String,
    repository: String,
    reference: String,
    is_digest: bool,
}

#[derive(Debug, serde::Deserialize)]
struct OciPlatform {
    architecture: String,
    os: String,
    #[serde(default)]
    _variant: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OciDescriptor {
    #[serde(rename = "mediaType")]
    _media_type: Option<String>,
    digest: String,
    #[serde(default)]
    platform: Option<OciPlatform>,
}

#[derive(Debug, serde::Deserialize)]
struct OciManifestDocument {
    #[serde(rename = "mediaType")]
    _media_type: Option<String>,
    #[serde(default)]
    config: Option<OciDescriptor>,
    #[serde(default)]
    layers: Vec<OciDescriptor>,
    #[serde(default)]
    manifests: Vec<OciDescriptor>,
}

fn normalized_host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn preferred_oci_platforms() -> Vec<(&'static str, &'static str)> {
    vec![
        (std::env::consts::OS, normalized_host_architecture()),
        ("wasi", "wasm"),
        ("wasi", "wasm32"),
        ("wasip2", "wasm"),
        ("wasip2", "wasm32"),
    ]
}

fn select_oci_manifest_descriptor(manifests: &[OciDescriptor]) -> Option<&OciDescriptor> {
    for (os, arch) in preferred_oci_platforms() {
        if let Some(descriptor) = manifests.iter().find(|descriptor| {
            descriptor
                .platform
                .as_ref()
                .map(|platform| {
                    platform.os.eq_ignore_ascii_case(os)
                        && platform.architecture.eq_ignore_ascii_case(arch)
                })
                .unwrap_or(false)
        }) {
            return Some(descriptor);
        }
    }

    if manifests
        .iter()
        .all(|descriptor| descriptor.platform.is_none())
    {
        manifests.first()
    } else {
        None
    }
}

fn parse_oci_reference(reference: &str) -> Result<OciReference, PlatformError> {
    let without_scheme = reference.strip_prefix("oci://").ok_or_else(|| {
        PlatformError::config_validation("OCI artifact reference must start with oci://")
    })?;
    let slash_index = without_scheme.find('/').ok_or_else(|| {
        PlatformError::config_validation(
            "OCI artifact reference must include a registry and repository path",
        )
    })?;
    let registry = &without_scheme[..slash_index];
    let repo_and_ref = &without_scheme[slash_index + 1..];
    let last_slash = repo_and_ref.rfind('/').unwrap_or(0);
    let at_index = repo_and_ref.rfind('@');
    let colon_index = repo_and_ref.rfind(':');

    let (repository, reference_value) = if let Some(at_index) = at_index {
        (&repo_and_ref[..at_index], &repo_and_ref[at_index + 1..])
    } else if let Some(colon_index) = colon_index {
        if colon_index <= last_slash {
            return Err(PlatformError::config_validation(
                "OCI artifact reference must include a tag or digest",
            ));
        }
        (
            &repo_and_ref[..colon_index],
            &repo_and_ref[colon_index + 1..],
        )
    } else {
        return Err(PlatformError::config_validation(
            "OCI artifact reference must include a tag or digest",
        ));
    };

    if registry.trim().is_empty()
        || repository.trim().is_empty()
        || reference_value.trim().is_empty()
    {
        return Err(PlatformError::config_validation(
            "OCI artifact reference contains an empty registry, repository, or tag/digest",
        ));
    }

    Ok(OciReference {
        registry: registry.to_string(),
        repository: repository.to_string(),
        reference: reference_value.to_string(),
        is_digest: at_index.is_some(),
    })
}

pub fn oci_reference_is_digest_pinned(reference: &str) -> Result<bool, PlatformError> {
    Ok(parse_oci_reference(reference)?.is_digest)
}

async fn send_authenticated_get(
    client: &reqwest::Client,
    url: &str,
    authorization: Option<&str>,
    accept: Option<&str>,
) -> Result<reqwest::Response, PlatformError> {
    let mut request = client.get(url);
    if let Some(authorization) = authorization {
        request = request.header(reqwest::header::AUTHORIZATION, authorization);
    }
    if let Some(accept) = accept {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    request.send().await.map_err(PlatformError::external_source)
}

async fn fetch_http_artifact_bytes(
    url: &str,
    expected_hash: &str,
    authorization: Option<&str>,
) -> Result<Vec<u8>, PlatformError> {
    let parsed = Url::parse(url)
        .map_err(|e| PlatformError::config_validation(format!("invalid artifact URL: {e}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(PlatformError::config_validation(format!(
                "unsupported artifact URL scheme: {other}"
            )))
        }
    }

    let client = reqwest::Client::new();
    let response = send_authenticated_get(&client, parsed.as_str(), authorization, None).await?;
    if !response.status().is_success() {
        return Err(PlatformError::external(format!(
            "artifact download failed with HTTP {}",
            response.status()
        )));
    }
    let bytes = read_response_bytes_with_limit(
        response,
        MAX_REMOTE_ARTIFACT_BYTES,
        "remote artifact download",
    )
    .await?;
    let actual_hash = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_hash != expected_hash {
        return Err(PlatformError::security(format!(
            "artifact sha256 mismatch: expected {}, got {}",
            expected_hash, actual_hash
        )));
    }
    Ok(bytes.to_vec())
}

fn strip_sha256_prefix(digest: &str) -> Result<String, PlatformError> {
    let hash = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| PlatformError::config_validation("OCI digest must use sha256"))?;
    if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(PlatformError::config_validation(
            "OCI digest contains an invalid sha256 value",
        ));
    }
    Ok(hash.to_lowercase())
}

async fn fetch_oci_manifest_document(
    client: &reqwest::Client,
    registry: &str,
    repository: &str,
    reference: &str,
    authorization: Option<&str>,
) -> Result<OciManifestDocument, PlatformError> {
    let manifest_url = format!(
        "{}/v2/{repository}/manifests/{reference}",
        registry_base_url(registry)
    );
    let response = send_authenticated_get(
        client,
        &manifest_url,
        authorization,
        Some(OCI_ACCEPT_HEADER),
    )
    .await?;
    if !response.status().is_success() {
        return Err(PlatformError::external(format!(
            "OCI manifest fetch failed with HTTP {}",
            response.status()
        )));
    }
    response
        .json::<OciManifestDocument>()
        .await
        .map_err(PlatformError::external_source)
}

async fn resolve_oci_blob_digest(
    client: &reqwest::Client,
    reference: &OciReference,
    authorization: Option<&str>,
) -> Result<String, PlatformError> {
    let mut current_ref = reference.reference.clone();
    for _ in 0..4 {
        let manifest = fetch_oci_manifest_document(
            client,
            &reference.registry,
            &reference.repository,
            &current_ref,
            authorization,
        )
        .await?;

        if let Some(descriptor) = manifest.layers.first() {
            return strip_sha256_prefix(&descriptor.digest);
        }
        if let Some(descriptor) = select_oci_manifest_descriptor(&manifest.manifests) {
            current_ref = descriptor.digest.clone();
            continue;
        }
        if !manifest.manifests.is_empty() {
            return Err(PlatformError::config_validation(
                "OCI manifest list did not contain a descriptor matching this node platform",
            ));
        }
        if let Some(descriptor) = manifest.config.as_ref() {
            return strip_sha256_prefix(&descriptor.digest);
        }
        return Err(PlatformError::config_validation(
            "OCI manifest did not contain a fetchable blob descriptor",
        ));
    }

    Err(PlatformError::config_validation(
        "OCI manifest resolution exceeded recursion limit",
    ))
}

async fn fetch_oci_artifact_bytes(
    reference: &str,
    authorization: Option<&str>,
) -> Result<(String, Vec<u8>), PlatformError> {
    let parsed = parse_oci_reference(reference)?;
    let client = reqwest::Client::new();
    let blob_hash = if parsed.reference.starts_with("sha256:") {
        strip_sha256_prefix(&parsed.reference)?
    } else {
        resolve_oci_blob_digest(&client, &parsed, authorization).await?
    };
    let blob_url = format!(
        "{}/v2/{}/blobs/sha256:{}",
        registry_base_url(&parsed.registry),
        parsed.repository,
        blob_hash
    );
    let bytes = fetch_http_artifact_bytes(&blob_url, &blob_hash, authorization).await?;
    Ok((blob_hash, bytes))
}

async fn read_response_bytes_with_limit(
    mut response: reqwest::Response,
    max_bytes: u64,
    context: &str,
) -> Result<Vec<u8>, PlatformError> {
    if let Some(content_length) = response.content_length() {
        if content_length > max_bytes {
            return Err(PlatformError::security(format!(
                "{context} exceeds maximum size: {} bytes > {} bytes",
                content_length, max_bytes
            )));
        }
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(PlatformError::external_source)?
    {
        let next_len = bytes.len() as u64 + chunk.len() as u64;
        if next_len > max_bytes {
            return Err(PlatformError::security(format!(
                "{context} exceeds maximum size: {} bytes > {} bytes",
                next_len, max_bytes
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

pub async fn ingest_remote_artifact(
    store: &Store,
    secret_provider: &dyn SecretProvider,
    artifact_server_url: &str,
    artifact_transfer_authority: &ArtifactTransferAuthority,
    node_id: &str,
    cluster_node_stale_after_secs: u64,
    artifact: RemoteArtifactSource,
) -> Result<RemoteArtifactIngressResponse, PlatformError> {
    let authorization = if let Some(credential_ref) = artifact.credential_ref.as_deref() {
        Some(resolve_artifact_authorization_header(secret_provider, credential_ref).await?)
    } else {
        None
    };

    let (expected_hash, bytes) = if let Some(reference) = artifact.reference.as_deref() {
        let (expected_hash, bytes) =
            fetch_oci_artifact_bytes(reference, authorization.as_deref()).await?;
        (expected_hash, bytes)
    } else {
        if artifact.url.trim().is_empty() {
            return Err(PlatformError::config_validation(
                "remote artifact source must include either url or reference",
            ));
        }
        if artifact.sha256.trim().is_empty() {
            return Err(PlatformError::config_validation(
                "remote artifact URL sources require sha256",
            ));
        }
        let expected_hash = artifact.sha256.to_lowercase();
        let bytes =
            fetch_http_artifact_bytes(&artifact.url, &expected_hash, authorization.as_deref())
                .await?;
        (expected_hash, bytes)
    };

    let bytes = if let Some(existing) = store.load_raw_wasm(&expected_hash)? {
        existing
    } else {
        store.save_raw_wasm(&expected_hash, &bytes)?;
        bytes
    };

    let target_node_ids: Vec<String> = store
        .list_cluster_nodes()?
        .into_iter()
        .filter(|node| !node.is_stale(cluster_node_stale_after_secs))
        .map(|node| node.node_id)
        .filter(|peer_id| peer_id != node_id)
        .collect();

    let artifact_transfer_manifests = target_node_ids
        .into_iter()
        .map(|audience_node_id| ArtifactManifestAudienceBinding {
            artifact_transfer_manifest: artifact_transfer_authority
                .issue_read_manifest_for_audience(&expected_hash, &audience_node_id),
            audience_node_id,
        })
        .collect();

    Ok(RemoteArtifactIngressResponse {
        source_node_id: node_id.to_string(),
        artifact_url: format!(
            "{}/artifacts/{}",
            trim_artifact_base_url(artifact_server_url),
            expected_hash
        ),
        expected_hash,
        size_bytes: bytes.len() as u64,
        artifact_transfer_manifests,
    })
}

fn audit_deploy_intent(
    node_id: &str,
    event_type: supervisor::audit::AuditEventType,
    app_id: &AppId,
    artifact: &RemoteArtifactSource,
    result: &str,
    detail: serde_json::Value,
) {
    let source_kind = if artifact.reference.is_some() {
        "oci"
    } else {
        "http"
    };
    let source = artifact
        .reference
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(artifact.url.as_str());
    supervisor::audit::write_audit_event(
        "/var/log/wasm-node/audit.jsonl",
        &supervisor::audit::AuditEvent {
            timestamp: chrono::Utc::now().timestamp_millis() as u64,
            node_id: node_id.to_string(),
            event_type,
            actor: "admin:deploy_intent".to_string(),
            app_id: app_id.0.clone(),
            details: serde_json::json!({
                "artifact_source_kind": source_kind,
                "artifact_source": source,
                "expected_sha256": artifact.sha256,
                "credential_ref": artifact.credential_ref,
                "result": result,
                "detail": detail,
            }),
        },
    );
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
    for route in &request.routes {
        if route.app_id != request.app_id {
            return Err(PlatformError::config_validation(format!(
                "deploy intent route host {} targets app_id {} instead of {}",
                route.host, route.app_id.0, request.app_id.0
            )));
        }
        if route.host.trim().is_empty() {
            return Err(PlatformError::config_validation(
                "deploy intent route host cannot be empty",
            ));
        }
    }
    Ok(())
}

pub async fn process_deploy_intent(
    ctx: DeployIntentContext<'_>,
    request: DeployIntentRequest,
) -> Result<DeployIntentResponse, PlatformError> {
    validate_deploy_intent_request(&request)?;

    let ingress = ingest_remote_artifact(
        ctx.store,
        ctx.secret_provider,
        ctx.artifact_server_url,
        ctx.artifact_transfer_authority,
        ctx.node_id,
        ctx.cluster_node_stale_after_secs,
        request.artifact.clone(),
    )
    .await
    .inspect_err(|err| {
        audit_deploy_intent(
            ctx.node_id,
            supervisor::audit::AuditEventType::AdminApiCall,
            &request.app_id,
            &request.artifact,
            "artifact_ingest_failed",
            serde_json::json!({
                "error": err.to_string(),
            }),
        );
    })?;

    ctx.bus
        .publish(&Event::DeployApp {
            app_id: request.app_id.clone(),
            config: request.config.clone(),
            artifact_url: ingress.artifact_url.clone(),
            artifact_transfer_manifests: ingress.artifact_transfer_manifests.clone(),
            expected_hash: Some(ingress.expected_hash.clone()),
            size_bytes: ingress.size_bytes,
        })
        .await?;

    let gateway_config_published = if let Some(gateway_config) = request.gateway_config.clone() {
        ctx.bus
            .publish(&Event::GatewayConfigUpdate {
                app_id: request.app_id.clone(),
                config: gateway_config,
            })
            .await?;
        true
    } else {
        false
    };

    for route in &request.routes {
        ctx.bus
            .publish(&Event::RouteAdd {
                route: route.clone(),
            })
            .await?;
    }

    if !request.api_keys.is_empty() {
        ctx.store
            .save_api_keys(&request.app_id.0, &request.api_keys)?;
        let _ = ctx
            .bus
            .publish(&Event::GatewayConfigUpdate {
                app_id: request.app_id.clone(),
                config: request.gateway_config.unwrap_or_default(),
            })
            .await;
    }

    audit_deploy_intent(
        ctx.node_id,
        supervisor::audit::AuditEventType::AppDeployed,
        &request.app_id,
        &request.artifact,
        "accepted",
        serde_json::json!({
            "artifact_url": ingress.artifact_url,
            "size_bytes": ingress.size_bytes,
            "gateway_config_published": gateway_config_published,
            "route_count": request.routes.len(),
            "api_key_count": request.api_keys.len(),
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
        route_count: request.routes.len(),
        api_key_count: request.api_keys.len(),
    })
}

pub struct DeployIntentContext<'a> {
    pub store: &'a Store,
    pub secret_provider: &'a dyn SecretProvider,
    pub artifact_server_url: &'a str,
    pub artifact_transfer_authority: &'a ArtifactTransferAuthority,
    pub node_id: &'a str,
    pub cluster_node_stale_after_secs: u64,
    pub bus: &'a messaging::NatsBus,
}

fn extract_proxy_host(address: &str) -> Option<String> {
    Url::parse(&format!("http://{address}"))
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
}

fn decode_plaintext_secret(envelope: &SecretTransportEnvelope) -> Result<String, PlatformError> {
    if envelope.version != SecretTransportEnvelope::VERSION_1 {
        return Err(PlatformError::messaging(format!(
            "unsupported secret transport version {}",
            envelope.version
        )));
    }

    match &envelope.payload {
        SecretTransportPayload::PlaintextUtf8V1 { value } => Ok(value.clone()),
        other => Err(PlatformError::messaging(format!(
            "unexpected secret payload variant for secret rotation: {:?}",
            other
        ))),
    }
}

fn decrypt_node_transport_secret(
    keypair: &BootstrapKeyPair,
    envelope: &SecretTransportEnvelope,
) -> Result<String, PlatformError> {
    if envelope.version != SecretTransportEnvelope::VERSION_1 {
        return Err(PlatformError::messaging(format!(
            "unsupported secret transport version {}",
            envelope.version
        )));
    }

    let ciphertext = match &envelope.payload {
        SecretTransportPayload::NodeTransportCiphertextV1 { ciphertext } => ciphertext,
        other => {
            return Err(PlatformError::messaging(format!(
                "unexpected node transport secret payload variant: {:?}",
                other
            )))
        }
    };

    let plaintext_bytes = keypair.decrypt(ciphertext)?;
    String::from_utf8(plaintext_bytes)
        .map_err(|e| PlatformError::encryption_with_msg("node transport secret not valid UTF-8", e))
}

fn decrypt_bootstrap_secret(
    keypair: &BootstrapKeyPair,
    envelope: &SecretTransportEnvelope,
) -> Result<String, PlatformError> {
    if envelope.version != SecretTransportEnvelope::VERSION_1 {
        return Err(PlatformError::messaging(format!(
            "unsupported bootstrap secret transport version {}",
            envelope.version
        )));
    }

    let ciphertext = match &envelope.payload {
        SecretTransportPayload::BootstrapPeerCiphertextV1 { ciphertext } => ciphertext,
        other => {
            return Err(PlatformError::messaging(format!(
                "unexpected bootstrap secret payload variant: {:?}",
                other
            )))
        }
    };

    let plaintext_bytes = keypair.decrypt(ciphertext)?;
    String::from_utf8(plaintext_bytes)
        .map_err(|e| PlatformError::encryption_with_msg("bootstrap secret not valid UTF-8", e))
}

async fn apply_secret_update<S: SecretProvider + ?Sized>(
    secret_provider: &S,
    transport_keypair: &BootstrapKeyPair,
    app_id: &AppId,
    key: &str,
    secret: &SecretTransportEnvelope,
) -> Result<(), PlatformError> {
    let plaintext = match &secret.payload {
        SecretTransportPayload::PlaintextUtf8V1 { .. } => decode_plaintext_secret(secret)?,
        SecretTransportPayload::NodeTransportCiphertextV1 { .. } => {
            decrypt_node_transport_secret(transport_keypair, secret)?
        }
        other => {
            return Err(PlatformError::messaging(format!(
                "unexpected secret payload variant for secret rotation: {:?}",
                other
            )))
        }
    };
    secret_provider.set(app_id, key, &plaintext).await
}

fn persist_applied_bootstrap_session(
    store: &Store,
    session_id: &str,
    nonce: &str,
) -> Result<(), PlatformError> {
    let record = BootstrapSessionRecord {
        session_id: session_id.to_string(),
        nonce: nonce.to_string(),
        applied_at_ms: Some(now_unix_ms()),
    };
    let json = serde_json::to_string(&record).map_err(|e| {
        PlatformError::storage_with_msg("failed to serialize bootstrap metadata", e)
    })?;
    store
        .save_meta(BOOTSTRAP_APPLIED_META_KEY, &json)
        .map_err(PlatformError::storage_source)?;
    store
        .delete_meta(BOOTSTRAP_PENDING_META_KEY)
        .map_err(PlatformError::storage_source)?;
    Ok(())
}

pub struct EventDispatcher {
    pub supervisor: Arc<Supervisor>,
    pub upstream: Arc<UpstreamRegistry>,
    pub host_router: Arc<HostRouter>,
    pub store: Store,
    pub runtime: WasmRuntime,
    pub node_id: String,
    pub artifact_server_url: String,
    pub artifact_transfer_authority: ArtifactTransferAuthority,
    pub upgrade_signing_public_key: Option<String>,
    pub secret_provider: Arc<dyn SecretProvider>,
    pub secret_transport_keypair: Arc<BootstrapKeyPair>,
    pub(crate) bootstrap_session: Option<Arc<Mutex<BootstrapSessionState>>>,
    pub bus: messaging::NatsBus,
    pub dns_webhook: Option<DnsWebhookManager>,
    pub node_table: Arc<NodeLoadTable>,
    pub cluster_node_stale_after_secs: u64,
    /// In-memory gateway cache (also updated when persistent storage changes).
    pub gateway: Option<Arc<proxy::gateway::Gateway>>,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) -> Result<(), PlatformError> {
        let event_name = format!("{:?}", std::mem::discriminant(&event));
        tracing::info!(event = %event_name, "received event in handler");

        match event {
            Event::DeployApp {
                app_id,
                config,
                artifact_url,
                artifact_transfer_manifests,
                expected_hash,
                size_bytes,
            } => {
                info!(
                    "🚀 Handling DeployApp for app_id: {}, url: {}",
                    app_id.0, artifact_url
                );
                self.handle_deploy(
                    app_id,
                    config,
                    artifact_url,
                    artifact_transfer_manifests,
                    expected_hash,
                    size_bytes,
                )
                .await
            }
            Event::RemoveApp { app_id } => self.handle_remove(app_id).await,
            Event::RouteAdd { route } => {
                self.store.save_route(&route)?;
                self.host_router
                    .add_route(
                        route.host.clone(),
                        route.path_prefix.clone(),
                        route.app_id.clone(),
                        route.strip_prefix,
                    )
                    .await;
                info!(host = %route.host, app = %route.app_id.0, "route added");
                if let Some(ref webhook) = self.dns_webhook {
                    webhook
                        .notify_route_change("add", &route.host, &route.app_id.0)
                        .await;
                }
                Ok(())
            }
            Event::RouteRemove { host } => {
                // Load route to get app_id and path_prefix for webhook before deleting.
                // A missing pre-delete lookup is not fatal — it only affects webhook context.
                let existing = self.store.load_route(&host).ok().flatten();
                let app_id = existing.as_ref().map(|r| r.app_id.clone());
                let path_prefix = existing
                    .as_ref()
                    .map(|r| r.path_prefix.clone())
                    .unwrap_or_default();
                self.store.delete_route(&host)?;
                self.host_router.remove_route(&host, &path_prefix).await;
                info!(host, "route removed");
                if let Some(ref webhook) = self.dns_webhook {
                    if let Some(app_id) = app_id {
                        webhook
                            .notify_route_change("remove", &host, &app_id.0)
                            .await;
                    }
                }
                Ok(())
            }
            Event::InstanceReady {
                app_id,
                addr,
                node_id,
            } => {
                // Only register if it's from a DIFFERENT node
                // (our own instances are registered directly by the Supervisor)
                if node_id != self.our_node_id() {
                    self.upstream
                        .add(
                            &app_id,
                            proxy::upstream::UpstreamEndpoint { addr, h2c: false },
                        )
                        .await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance registered");
                }
                Ok(())
            }
            Event::InstanceDead {
                app_id,
                addr,
                node_id,
            } => {
                if node_id != self.our_node_id() {
                    self.upstream.remove(&app_id, &addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
                }
                Ok(())
            }
            Event::SecretUpdate {
                app_id,
                key,
                target_node_id,
                secret,
            } => {
                if let Some(target_node_id) = target_node_id {
                    if target_node_id != self.our_node_id() {
                        return Ok(());
                    }
                }
                info!(app = %app_id.0, key, "received secret rotation");
                apply_secret_update(
                    self.secret_provider.as_ref(),
                    self.secret_transport_keypair.as_ref(),
                    &app_id,
                    &key,
                    &secret,
                )
                .await?;
                Ok(())
            }
            Event::ConfigUpdate { app_id, config } => {
                self.store.save_config(&config)?;
                info!(app = %app_id.0, "config updated");
                Ok(())
            }
            Event::NodeLoad {
                node_id,
                cpu_percent: _,
                fuel_budget_used_percent,
                active_instances,
                proxy_address,
            } => {
                let mut cluster_node =
                    merge_cluster_node_record(self.store.load_cluster_node(&node_id)?, &node_id);
                cluster_node.last_seen_unix_secs = now_unix_secs();
                cluster_node.proxy_address = Some(proxy_address.clone());
                cluster_node.active_instances = Some(active_instances);
                self.store.save_cluster_node(&cluster_node)?;

                // Update node table for cross-node routing decisions
                use proxy::node_table::NodeEntry;
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let entry = NodeEntry {
                    node_id: node_id.clone(),
                    proxy_address,
                    fuel_used_percent: fuel_budget_used_percent,
                    active_instances,
                    last_seen: now,
                    health_status: common::health::NodeHealthStatus::Healthy,
                };
                self.node_table.update(entry).await;

                // Update DNS webhook with node IPs for webhook notifications
                if let Some(ref webhook) = self.dns_webhook {
                    let nodes = self.node_table.nodes.read().await;
                    let ips: Vec<String> = nodes
                        .values()
                        .filter_map(|n| extract_proxy_host(&n.proxy_address))
                        .collect();
                    drop(nodes);
                    webhook.set_node_ips(ips).await;
                }
                Ok(())
            }
            Event::NodeJoined {
                node_id,
                bootstrap_session_id,
                bootstrap_nonce,
                artifact_server_url,
                public_key_bytes,
                protocol_version,
                binary_version,
            } => {
                self.handle_node_joined(
                    node_id,
                    bootstrap_session_id,
                    bootstrap_nonce,
                    artifact_server_url,
                    public_key_bytes,
                    protocol_version,
                    binary_version,
                )
                .await
            }
            Event::StateSnapshot {
                for_node_id,
                bootstrap_session_id,
                bootstrap_nonce,
                configs,
                routes,
                encrypted_secrets,
                gateway_configs,
                api_keys,
                artifact_fetches,
                artifact_hashes,
            } => {
                if for_node_id == self.node_id {
                    self.handle_state_snapshot(
                        bootstrap_session_id,
                        bootstrap_nonce,
                        configs,
                        routes,
                        encrypted_secrets,
                        gateway_configs,
                        api_keys,
                        artifact_fetches,
                        artifact_hashes,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            Event::NodeUpgrade { .. } => {
                self.handle_node_upgrade(event).await;
                Ok(())
            }
            Event::NodeUpgradeComplete {
                node_id,
                new_binary_version,
                new_protocol_version,
            } => {
                info!(
                    node = %node_id,
                    version = %new_binary_version,
                    protocol = new_protocol_version,
                    "node upgrade completed"
                );
                // Check if we were waiting for this node
                // Re-evaluate our upgrade status if we're in a rolling upgrade
                Ok(())
            }
            Event::NodeDraining {
                node_id,
                drain_timeout_secs,
            } => {
                if node_id == self.node_id {
                    info!(
                        timeout_secs = drain_timeout_secs,
                        "beginning graceful shutdown"
                    );
                    self.begin_graceful_shutdown(drain_timeout_secs).await;
                } else {
                    info!(node = %node_id, "peer node draining");
                }
                Ok(())
            }

            // ── eBPF Monitor Events ──────────────────────────────────────
            Event::NodeUnderPressure {
                node_id,
                pressure_level,
            } => {
                if node_id == self.node_id {
                    // Our own pressure events are handled directly by the
                    // eBPF ActionDispatcher. Also mark ourselves unhealthy in
                    // the routing table so the local proxy can shed traffic.
                    warn!(
                        pressure_level,
                        "local node under pressure — excluding self from least-loaded routing"
                    );
                    self.node_table.mark_unhealthy(&node_id).await;
                } else {
                    // A peer node is under pressure — stop steering traffic to it.
                    warn!(
                        node = %node_id,
                        pressure_level,
                        "peer node under pressure — removing from routing"
                    );
                    self.node_table.mark_unhealthy(&node_id).await;
                    if pressure_level >= 2 {
                        // Critical pressure: also remove all upstream entries
                        tracing::warn!(
                            node = %node_id,
                            "peer node under CRITICAL pressure — removing all upstream entries"
                        );
                    }
                }
                Ok(())
            }

            Event::NodePressureRecovered { node_id } => {
                if node_id == self.node_id {
                    info!("our node pressure recovered");
                    self.node_table.mark_healthy(&node_id).await;
                } else {
                    info!(node = %node_id, "peer node pressure recovered — restoring in routing");
                    self.node_table.mark_healthy(&node_id).await;
                }
                Ok(())
            }

            Event::SecurityIncident {
                node_id,
                app_id,
                pid,
                syscall_nr,
                category,
            } => {
                tracing::error!(
                    node = %node_id,
                    app = %app_id,
                    pid,
                    syscall_nr,
                    category,
                    "SECURITY INCIDENT: privileged syscall detected by eBPF monitor"
                );
                // If the incident is from our node, the eBPF ActionDispatcher
                // already killed the instance. For remote nodes, we log and
                // could quarantine the artifact hash if we had a cluster-wide
                // artifact blocklist. For now, we just log the incident.
                if node_id != self.node_id {
                    warn!(
                        node = %node_id,
                        app = %app_id,
                        "security incident on peer node — consider quarantining artifact"
                    );
                }
                Ok(())
            }

            // ── Health Events ─────────────────────────────────────────────
            Event::NodeHealthChanged {
                node_id,
                status,
                cause,
                active_instances,
                accepting_requests,
                timestamp: _,
            } => {
                tracing::info!(
                    node = %node_id,
                    status = %status,
                    cause = ?cause,
                    active_instances,
                    accepting_requests,
                    "node health status changed"
                );
                // Update our node table with the new health status
                let health_status = match status.as_str() {
                    "healthy" => common::health::NodeHealthStatus::Healthy,
                    "degraded" => common::health::NodeHealthStatus::Degraded,
                    "unhealthy" => common::health::NodeHealthStatus::Unhealthy,
                    _ => common::health::NodeHealthStatus::Degraded,
                };
                self.node_table.update_health(&node_id, health_status).await;
                let mut cluster_node =
                    merge_cluster_node_record(self.store.load_cluster_node(&node_id)?, &node_id);
                cluster_node.last_seen_unix_secs = now_unix_secs();
                cluster_node.health_status = health_status;
                cluster_node.active_instances = Some(active_instances);
                cluster_node.accepting_requests = Some(accepting_requests);
                self.store.save_cluster_node(&cluster_node)?;
                Ok(())
            }

            Event::NodeHealthSnapshot {
                node_id,
                status,
                active_instances,
                deployed_apps,
                nats_connected,
                disk_free_mb,
                memory_used_mb,
                ..
            } => {
                tracing::debug!(
                    node = %node_id,
                    status = %status,
                    active_instances,
                    deployed_apps,
                    nats_connected,
                    disk_free_mb,
                    memory_used_mb,
                    "node health snapshot received"
                );
                // Update node table health status from snapshot
                let health_status = match status.as_str() {
                    "healthy" => common::health::NodeHealthStatus::Healthy,
                    "degraded" => common::health::NodeHealthStatus::Degraded,
                    "unhealthy" => common::health::NodeHealthStatus::Unhealthy,
                    _ => common::health::NodeHealthStatus::Degraded,
                };
                self.node_table.update_health(&node_id, health_status).await;
                let mut cluster_node =
                    merge_cluster_node_record(self.store.load_cluster_node(&node_id)?, &node_id);
                cluster_node.last_seen_unix_secs = now_unix_secs();
                cluster_node.health_status = health_status;
                cluster_node.active_instances = Some(active_instances);
                cluster_node.deployed_apps = Some(deployed_apps);
                self.store.save_cluster_node(&cluster_node)?;
                Ok(())
            }

            Event::GatewayConfigUpdate { app_id, config } => {
                info!(app = %app_id.0, "received gateway config update");
                self.store.save_gateway_config(&app_id.0, &config)?;
                // Keep the in-memory gateway cache in sync so the internal
                // proxy can enforce endpoint policies without reloading from disk.
                if let Some(ref gw) = self.gateway {
                    gw.set_route_config(&app_id.0, config).await;
                }
                Ok(())
            }
            Event::GatewayConfigRemove { app_id } => {
                info!(app = %app_id.0, "received gateway config remove");
                self.store.delete_gateway_config(&app_id.0)?;
                if let Some(ref gw) = self.gateway {
                    gw.remove_route_config(&app_id.0).await;
                }
                Ok(())
            }
            // ── Configuration Hot-Reload ──────────────────────────────────
            Event::ConfigHotReload { node_id, changes } => {
                tracing::info!(
                    node = %node_id,
                    changes = ?changes,
                    "peer node changed hot-reloadable config (informational only, not auto-applied)"
                );
                // Design decision: Config changes are NOT auto-propagated.
                // Each node's operator controls its own configuration.
                // The event is informational — it alerts operators that the
                // cluster's configuration may have diverged.
                Ok(())
            }
            Event::DeployIngressArtifactReplicated { .. } => Ok(()),
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        artifact_url: String,
        artifact_transfer_manifests: Vec<ArtifactManifestAudienceBinding>,
        expected_hash: Option<String>,
        size_bytes: u64,
    ) -> Result<(), PlatformError> {
        tracing::info!(app = %app_id.0, "handle_deploy invoked");

        let sha256 = expected_hash
            .clone()
            .ok_or_else(|| PlatformError::messaging("deploy event missing expected_hash"))?;

        info!(
            app = %app_id.0,
            url = %artifact_url,
            size_mb = size_bytes as f64 / 1_048_576.0,
            "deploying artifact"
        );

        // 1. Check local cache first (another node may have already stored it)
        let wasm_bytes = if self.store.raw_wasm_exists(&sha256)? {
            info!(sha256, "artifact already in local cache");
            self.store.load_raw_wasm(&sha256)?.ok_or_else(|| {
                PlatformError::storage("artifact vanished between exists and load")
            })?
        } else {
            // 2. Fetch from the source node
            info!(url = %artifact_url, "fetching artifact via HTTP");
            let targeted_manifest = artifact_transfer_manifests
                .iter()
                .find(|binding| binding.audience_node_id == self.node_id)
                .map(|binding| &binding.artifact_transfer_manifest)
                .ok_or_else(|| {
                    PlatformError::messaging(format!(
                        "deploy event missing audience-bound artifact manifest for node {} (artifact {})",
                        self.node_id, sha256
                    ))
                })?;
            let bytes = fetch_artifact(
                &artifact_url,
                Some(self.node_id.as_str()),
                Some(targeted_manifest),
                &sha256,
            )
            .await
            .map_err(PlatformError::external)?;

            // Hash already verified by download_and_verify_bytes.
            // Persisting raw bytes is part of successful deploy processing.
            self.store.save_raw_wasm(&sha256, &bytes)?;
            bytes
        };

        info!(app = %app_id.0, bytes = wasm_bytes.len(), "artifact ready, compiling");

        // 5. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let artifact = tokio::task::spawn_blocking(move || runtime.compile(&wasm_bytes))
            .await
            .map_err(|e| PlatformError::runtime(format!("spawn_blocking panic: {e}")))?;
        let artifact_bytes = artifact?;

        // 6. Store compiled artifact, config, and hash
        self.store.store_artifact(&app_id, &artifact_bytes)?;
        self.store.save_config(&config)?;
        self.store.save_artifact_hash(&app_id, &sha256)?;
        info!(
            app = %app_id.0,
            "deploy complete, waiting for first request"
        );
        Ok(())
    }

    async fn handle_remove(&self, app_id: AppId) -> Result<(), PlatformError> {
        info!(app = %app_id.0, "removing app");

        // Kill all running instances first (this creates billing records)
        self.supervisor.kill_all_instances(&app_id).await?;

        // Mark app as undeployed - starts grace period
        // Actual deletion happens after grace period expires in GC loop
        self.store.mark_undeployed(&app_id.0)?;

        // Note: We don't immediately delete artifacts/configs anymore
        // The GC loop will purge them after the grace period
        info!(app = %app_id.0, "app marked for deletion, grace period started");
        Ok(())
    }

    fn our_node_id(&self) -> String {
        self.node_id.clone()
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_node_joined(
        &self,
        new_node_id: String,
        bootstrap_session_id: String,
        bootstrap_nonce: String,
        peer_artifact_url: String,
        peer_public_key: Vec<u8>,
        protocol_version: u32,
        binary_version: String,
    ) -> Result<(), PlatformError> {
        info!(
            new_node = %new_node_id,
            protocol = protocol_version,
            version = %binary_version,
            "node joined cluster"
        );

        if new_node_id == self.node_id {
            tracing::debug!(new_node = %new_node_id, "ignoring our own NodeJoined event");
            return Ok(());
        }

        validate_peer_artifact_url(&new_node_id, &peer_artifact_url)?;

        let mut cluster_node =
            merge_cluster_node_record(self.store.load_cluster_node(&new_node_id)?, &new_node_id);
        let now_secs = now_unix_secs();
        cluster_node.last_seen_unix_secs = now_secs;
        cluster_node.joined_at_unix_secs =
            Some(cluster_node.joined_at_unix_secs.unwrap_or(now_secs));
        cluster_node.artifact_server_url = Some(peer_artifact_url.clone());
        cluster_node.protocol_version = Some(protocol_version);
        cluster_node.binary_version = Some(binary_version.clone());
        self.store.save_cluster_node(&cluster_node)?;

        info!(
            new_node = %new_node_id,
            our_node = %self.node_id,
            "sending state snapshot to new node"
        );

        // 1. Collect all configs
        let app_ids = self.store.list_apps()?;
        let mut configs = Vec::with_capacity(app_ids.len());
        for id in &app_ids {
            if let Some(config) = self.store.load_config(id)? {
                configs.push(config);
            }
        }

        // 2. Collect all routes
        let routes = self.store.list_routes()?;

        // 3. Encrypt secrets for each app using the canonical transport envelope
        let mut encrypted_secrets: Vec<SecretTransportEntry> = Vec::new();
        for config in &configs {
            let keys = self.secret_provider.list_keys(&config.id).await?;
            for key in keys {
                let value = self.secret_provider.get(&config.id, &key).await?;
                let encrypted = encrypt_for_peer(&peer_public_key, value.as_bytes())?;
                encrypted_secrets.push(SecretTransportEntry {
                    app_id: config.id.0.clone(),
                    key,
                    envelope: SecretTransportEnvelope::bootstrap_peer_ciphertext(encrypted),
                });
            }
        }

        // 4. Collect gateway policy state and artifact hashes
        let gateway_configs = self.store.list_gateway_configs()?;
        let mut api_keys = Vec::new();
        let mut artifact_hashes = Vec::new();
        let mut artifact_fetches = Vec::new();
        for config in &configs {
            if let Some(hash) = self.store.get_artifact_sha256(&config.id)? {
                artifact_hashes.push((config.id.0.clone(), hash.clone()));
                artifact_fetches.push(BootstrapArtifactFetchAuthorization {
                    app_id: config.id.0.clone(),
                    sha256: hash.clone(),
                    artifact_url: format!("{}/artifacts/{}", self.artifact_server_url, hash),
                    artifact_transfer_manifest: Some(
                        self.artifact_transfer_authority
                            .issue_read_manifest_for_audience(&hash, &new_node_id),
                    ),
                });
            }
            let keys = self.store.load_api_keys(&config.id.0)?;
            if !keys.is_empty() {
                api_keys.push((config.id.0.clone(), keys));
            }
        }

        // 5. Publish the snapshot event.
        // Bootstrap coordination is explicit: every eligible existing node may respond,
        // but the joining node accepts only the first valid session/nonce-matching
        // snapshot and ignores duplicates for the same session afterwards.
        let event = Event::StateSnapshot {
            for_node_id: new_node_id.clone(),
            bootstrap_session_id,
            bootstrap_nonce,
            configs,
            routes,
            encrypted_secrets,
            gateway_configs,
            api_keys,
            artifact_fetches,
            artifact_hashes: artifact_hashes.clone(),
        };

        info!(
            new_node = %new_node_id,
            apps = artifact_hashes.len(),
            fetch_manifests = artifact_hashes.len(),
            peer_artifact_url = %peer_artifact_url,
            "snapshot prepared with signed artifact fetch authorizations"
        );

        // Publish the snapshot event via NATS
        self.bus.publish(&event).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_state_snapshot(
        &self,
        bootstrap_session_id: String,
        bootstrap_nonce: String,
        configs: Vec<common::types::AppConfig>,
        routes: Vec<common::types::Route>,
        encrypted_secrets: Vec<SecretTransportEntry>,
        gateway_configs: Vec<(String, common::types::GatewayRouteConfig)>,
        api_keys: Vec<(String, Vec<common::types::ApiKeyRecord>)>,
        artifact_fetches: Vec<BootstrapArtifactFetchAuthorization>,
        artifact_hashes: Vec<(String, String)>,
    ) -> Result<(), PlatformError> {
        info!(
            session = %bootstrap_session_id,
            apps = configs.len(),
            routes = routes.len(),
            secrets = encrypted_secrets.len(),
            "received state snapshot"
        );

        let Some(bootstrap_session) = self.bootstrap_session.as_ref() else {
            tracing::warn!(
                session = %bootstrap_session_id,
                "ignoring snapshot because this node is not awaiting bootstrap"
            );
            return Ok(());
        };

        let mut bootstrap_state = bootstrap_session.lock().await;
        if bootstrap_state.applied {
            tracing::info!(
                session = %bootstrap_session_id,
                "ignoring duplicate snapshot after bootstrap already completed"
            );
            return Ok(());
        }
        if bootstrap_state.session_id != bootstrap_session_id
            || bootstrap_state.nonce != bootstrap_nonce
        {
            tracing::warn!(
                expected_session = %bootstrap_state.session_id,
                received_session = %bootstrap_session_id,
                "ignoring stale or mismatched bootstrap snapshot"
            );
            return Ok(());
        }

        // 1. Store configs
        for config in &configs {
            self.store.save_config(config)?;
        }

        // 2. Store routes and load into HostRouter
        for route in &routes {
            self.store.save_route(route)?;
            self.host_router
                .add_route(
                    route.host.clone(),
                    route.path_prefix.clone(),
                    route.app_id.clone(),
                    route.strip_prefix,
                )
                .await;
        }

        // 3. Import gateway policy state before secrets/artifacts so the node's
        // routing/auth policy converges with the rest of the cluster during bootstrap.
        for (app_id, config) in gateway_configs {
            self.store.save_gateway_config(&app_id, &config)?;
            if let Some(ref gw) = self.gateway {
                gw.set_route_config(&app_id, config).await;
            }
        }

        for (app_id, keys) in api_keys {
            self.store.save_api_keys(&app_id, &keys)?;
            if !keys.is_empty() {
                let validator = proxy::gateway::api_key::ApiKeyValidator::new(keys);
                if let Some(ref gw) = self.gateway {
                    gw.set_api_key_validator(&app_id, validator).await;
                }
            }
        }

        // 4. Decrypt and store secrets
        for SecretTransportEntry {
            app_id: app_id_str,
            key,
            envelope,
        } in encrypted_secrets
        {
            let app_id = AppId(app_id_str.clone());
            let plaintext = decrypt_bootstrap_secret(&bootstrap_state.keypair, &envelope)?;
            self.secret_provider.set(&app_id, &key, &plaintext).await?;
            info!(app = app_id_str, key, "secret decrypted and stored");
        }

        // 5. Store artifact hashes
        for (app_id_str, sha256) in &artifact_hashes {
            let app_id = AppId(app_id_str.clone());
            self.store.save_artifact_hash(&app_id, sha256)?;
        }

        // 6. Fetch or locate artifacts, then compile them.
        let artifact_fetches_by_sha: HashMap<String, BootstrapArtifactFetchAuthorization> =
            artifact_fetches
                .into_iter()
                .map(|fetch| (fetch.sha256.clone(), fetch))
                .collect();

        for (app_id_str, sha256) in artifact_hashes {
            let app_id = AppId(app_id_str.clone());

            let artifact = if let Some(fetch) = artifact_fetches_by_sha.get(&sha256) {
                match fetch_artifact(
                    &fetch.artifact_url,
                    Some(self.node_id.as_str()),
                    fetch.artifact_transfer_manifest.as_ref(),
                    &sha256,
                )
                .await
                {
                    Ok(raw) => {
                        self.store.save_raw_wasm(&sha256, &raw)?;
                        info!(
                            app = %app_id_str,
                            sha256,
                            url = %fetch.artifact_url,
                            "artifact fetched from peer via signed bootstrap manifest"
                        );
                        Some(raw)
                    }
                    Err(e) => {
                        warn!(
                            app = %app_id_str,
                            sha256,
                            url = %fetch.artifact_url,
                            error = %e,
                            "failed to fetch artifact from bootstrap snapshot authorization"
                        );
                        None
                    }
                }
            } else {
                // Compatibility fallback for older peers that still push artifacts.
                let mut attempts = 0;
                loop {
                    if let Ok(Some(raw)) = self.store.load_raw_wasm(&sha256) {
                        break Some(raw);
                    }
                    if attempts >= 50 {
                        break None;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    attempts += 1;
                }
            };

            if let Some(raw) = artifact {
                let runtime = self.runtime.clone();
                let store = self.store.clone();
                let app_id_clone = app_id.clone();

                tokio::task::spawn_blocking(move || match runtime.compile(&raw) {
                    Ok(compiled) => {
                        if let Err(e) = store.store_artifact(&app_id_clone, &compiled) {
                            error!(app = %app_id_str, error = %e, "failed to store compiled artifact");
                        } else {
                            info!(app = %app_id_str, "artifact compiled from snapshot");
                        }
                    }
                    Err(e) => {
                        error!(app = %app_id_str, error = %e, "compilation failed");
                    }
                });
            } else {
                warn!(
                    app = app_id_str,
                    sha256, "artifact not yet available, will compile on first request"
                );
            }
        }

        bootstrap_state.applied = true;
        drop(bootstrap_state);

        persist_applied_bootstrap_session(&self.store, &bootstrap_session_id, &bootstrap_nonce)?;

        info!(session = %bootstrap_session_id, "state snapshot import complete");
        Ok(())
    }

    async fn handle_node_upgrade(&self, event: Event) {
        use crate::upgrade::{
            download_and_verify, handle_upgrade_event, verify_upgrade_signature, UpgradeAction,
        };

        // Collect all node IDs in the cluster for rolling upgrade ordering
        let mut cluster_nodes: Vec<String> = self
            .store
            .list_cluster_nodes()
            .unwrap_or_default()
            .into_iter()
            .filter(|node| !node.is_stale(self.cluster_node_stale_after_secs))
            .map(|node| node.node_id)
            .collect();
        if !cluster_nodes.contains(&self.node_id) {
            cluster_nodes.push(self.node_id.clone());
        }

        if let Err(e) = verify_upgrade_signature(&event, self.upgrade_signing_public_key.as_deref())
        {
            error!(error = %e, "upgrade signature verification failed");
            return;
        }

        match handle_upgrade_event(&event, &self.node_id, &cluster_nodes) {
            Ok(UpgradeAction::NotAnUpgradeEvent) => {
                warn!("handle_node_upgrade called with non-upgrade event");
            }
            Ok(UpgradeAction::NotTargeted) => {
                info!("upgrade not targeted at this node");
            }
            Ok(UpgradeAction::WaitForPredecessor { predecessor }) => {
                info!(
                    predecessor,
                    "waiting for predecessor node to complete upgrade"
                );
                // Store upgrade intent and wait for NodeUpgradeComplete event
            }
            Ok(UpgradeAction::IncompatibleVersion) => {
                error!("upgrade version is incompatible with this node's protocol version");
            }
            Ok(UpgradeAction::ProceedWithUpgrade) => {
                if let Event::NodeUpgrade {
                    binary_url,
                    binary_sha256,
                    new_protocol_version,
                    new_binary_version,
                    ..
                } = event
                {
                    info!(
                        url = %binary_url,
                        version = %new_binary_version,
                        protocol = new_protocol_version,
                        "proceeding with upgrade"
                    );

                    // Download and verify the new binary
                    let install_dir = std::path::PathBuf::from("/opt/wasm-cloud");
                    match download_and_verify(&binary_url, &binary_sha256, &install_dir, "node")
                        .await
                    {
                        Ok(new_binary_path) => {
                            info!(path = ?new_binary_path, "new binary downloaded, verified, and activated");

                            info!("release links updated, initiating graceful shutdown");

                            // Publish draining event
                            let drain_event = Event::NodeDraining {
                                node_id: self.node_id.clone(),
                                drain_timeout_secs: 30,
                            };

                            if let Err(e) = self.bus.publish(&drain_event).await {
                                error!(error = %e, "failed to publish draining event");
                            }

                            // Begin graceful shutdown
                            self.begin_graceful_shutdown(30).await;

                            // Publish upgrade complete event
                            let complete_event = Event::NodeUpgradeComplete {
                                node_id: self.node_id.clone(),
                                new_binary_version,
                                new_protocol_version,
                            };

                            if let Err(e) = self.bus.publish(&complete_event).await {
                                error!(error = %e, "failed to publish upgrade complete event");
                            }

                            // Exit process - systemd will restart with new binary
                            info!("exiting for upgrade, expecting systemd restart");
                            std::process::exit(0);
                        }
                        Err(e) => {
                            error!(error = %e, "failed to download or verify new binary");
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "upgrade event handling failed");
            }
        }
    }

    async fn begin_graceful_shutdown(&self, timeout_secs: u64) {
        tracing::info!("Beginning graceful shutdown with {}s timeout", timeout_secs);

        // 1. Stop accepting new connections via backpressure signal
        //    TODO: Add backpressure_signal field to EventDispatcher so we can
        //    call backpressure.set_rejecting() here. For now, the proxy will
        //    continue accepting connections until the process exits.

        // 2. Wait for existing requests to drain
        tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;

        // 3. Kill all running instances
        tracing::info!("drain timeout elapsed, stopping all instances");
        self.supervisor
            .shutdown_all(tokio::time::Duration::from_secs(timeout_secs))
            .await;

        tracing::info!("graceful shutdown complete");
    }
}

/// Fetch an artifact from a URL and verify its SHA-256 hash.
async fn fetch_artifact(
    url: &str,
    requester_node_id: Option<&str>,
    artifact_transfer_manifest: Option<&SignedArtifactTransferManifest>,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(manifest) = artifact_transfer_manifest {
        let header_value = manifest.encode_header_value()?;
        request = request.header(ARTIFACT_TRANSFER_MANIFEST_HEADER, header_value);
        if manifest.manifest.audience.is_some() {
            if let Some(requester_node_id) = requester_node_id {
                request =
                    request.header(ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER, requester_node_id);
            }
        }
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("download failed with HTTP {}", response.status()));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read artifact response body: {e}"))?;
    let actual_sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "artifact hash mismatch: expected {}, got {}",
            expected_sha256, actual_sha256
        ));
    }

    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{
        apply_secret_update, artifact_credentials_app_id, fetch_artifact, ingest_remote_artifact,
        is_loopback_artifact_url, normalized_host_architecture, validate_peer_artifact_url,
        BootstrapSessionState, EventDispatcher,
    };
    use common::{
        artifact_transfer::{
            ArtifactTransferAuthority, ARTIFACT_TRANSFER_MANIFEST_HEADER,
            ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER,
        },
        types::AppId,
    };
    use e2e::NatsContainer;
    use messaging::{events::Event, NatsBus};
    use secrets::{
        crypto::SymmetricKey, encrypt_for_peer, BootstrapKeyPair, LocalSecretProvider,
        SecretProvider, SecretTransportEnvelope,
    };
    use sha2::Digest;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use storage::Store;
    use supervisor::{network::NamespaceRegistry, port_alloc::PortAllocator, Supervisor};
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot, Mutex};

    static NATS_PORT_COUNTER: AtomicU16 = AtomicU16::new(0);

    fn allocate_nats_port() -> u16 {
        let base = 25000 + ((std::process::id() as u16) % 1000);
        base + NATS_PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    async fn start_test_nats() -> Result<NatsContainer, String> {
        NatsContainer::start(allocate_nats_port()).await
    }

    async fn build_test_dispatcher(
        store: Store,
        bootstrap_session: Option<Arc<Mutex<BootstrapSessionState>>>,
        dns_webhook: Option<proxy::dns_webhook::DnsWebhookManager>,
    ) -> EventDispatcher {
        let runtime = runtime::WasmRuntime::new().unwrap();
        let upstream = Arc::new(proxy::upstream::UpstreamRegistry::new());
        let host_router = Arc::new(proxy::router::HostRouter::default());
        let service_registry = Arc::new(NamespaceRegistry::default());
        let port_alloc = Arc::new(PortAllocator::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            20000,
            20010,
        ));
        let (event_tx, _event_rx) = mpsc::channel(8);
        let supervisor = Supervisor::new(
            store.clone(),
            "node-under-test".to_string(),
            runtime.clone(),
            port_alloc,
            upstream.clone(),
            host_router.clone(),
            service_registry,
            0,
            Arc::new(|_, _| Vec::new()),
            event_tx,
            None,
        );
        let nats = start_test_nats().await.unwrap();
        let mut bus = NatsBus::connect(&nats.url).await.unwrap();
        bus.set_node_id("node-under-test".to_string());

        EventDispatcher {
            supervisor,
            upstream,
            host_router,
            store: store.clone(),
            runtime,
            node_id: "node-under-test".to_string(),
            artifact_server_url: "http://node-under-test.internal:9091".to_string(),
            artifact_transfer_authority: ArtifactTransferAuthority::derive(
                "node-under-test",
                &[9u8; 32],
            ),
            upgrade_signing_public_key: None,
            secret_provider: Arc::new(LocalSecretProvider::new(store, SymmetricKey::generate())),
            secret_transport_keypair: Arc::new(BootstrapKeyPair::generate()),
            bootstrap_session,
            bus,
            dns_webhook,
            node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
            cluster_node_stale_after_secs: 120,
            gateway: None,
        }
    }

    #[test]
    fn test_is_loopback_artifact_url_detects_loopback_hosts() {
        assert!(is_loopback_artifact_url("http://127.0.0.1:9091"));
        assert!(is_loopback_artifact_url("http://localhost:9091"));
        assert!(!is_loopback_artifact_url("http://node-1.internal:9091"));
    }

    #[test]
    fn test_validate_peer_artifact_url_rejects_loopback() {
        let err = validate_peer_artifact_url("node-1", "http://127.0.0.1:9091").unwrap_err();
        assert!(err.to_string().contains("loopback artifact URL"));
    }

    #[test]
    fn test_validate_peer_artifact_url_accepts_routable_url() {
        validate_peer_artifact_url("node-1", "http://node-1.internal:9091").unwrap();
    }

    #[tokio::test]
    async fn test_apply_secret_update_uses_secret_provider_bundle_format() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        let app_id = AppId("secret-app:v1".to_string());

        apply_secret_update(
            &provider,
            &BootstrapKeyPair::generate(),
            &app_id,
            "API_KEY",
            &SecretTransportEnvelope::plaintext_utf8("super-secret-value"),
        )
        .await
        .unwrap();

        let plaintext = provider.get(&app_id, "API_KEY").await.unwrap();
        assert_eq!(plaintext, "super-secret-value");

        let raw = store.load_secrets(&app_id).unwrap().unwrap();
        assert_ne!(raw, b"super-secret-value");
    }

    #[tokio::test]
    async fn test_secret_update_event_roundtrip_persists_plaintext_via_secret_provider() {
        let _nats = start_test_nats().await.unwrap();
        let bus = NatsBus::connect(&_nats.url).await.unwrap();
        bus.setup_jetstream().await.unwrap();

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = std::sync::Arc::new(LocalSecretProvider::new(
            store.clone(),
            SymmetricKey::generate(),
        ));
        let app_id = AppId("secret-app:v1".to_string());
        let key = "API_KEY".to_string();
        let expected_value = "super-secret-over-nats".to_string();
        let (tx, rx) = oneshot::channel();
        let provider_for_handler = provider.clone();
        let app_id_for_handler = app_id.clone();
        let key_for_handler = key.clone();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let tx_for_handler = tx.clone();

        bus.subscribe(&format!("secrets.update.{}", app_id.0), move |event| {
            let provider = provider_for_handler.clone();
            let tx = tx_for_handler.clone();
            let expected_app_id = app_id_for_handler.clone();
            let expected_key = key_for_handler.clone();
            async move {
                if let Event::SecretUpdate {
                    app_id,
                    key,
                    target_node_id,
                    secret,
                } = event
                {
                    assert_eq!(app_id, expected_app_id);
                    assert_eq!(key, expected_key);
                    assert!(target_node_id.is_none());
                    apply_secret_update(
                        provider.as_ref(),
                        &BootstrapKeyPair::generate(),
                        &app_id,
                        &key,
                        &secret,
                    )
                    .await
                    .unwrap();
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                }
            }
        })
        .await
        .unwrap();

        bus.publish(&Event::SecretUpdate {
            app_id: app_id.clone(),
            key: key.clone(),
            target_node_id: None,
            secret: SecretTransportEnvelope::plaintext_utf8(expected_value.clone()),
        })
        .await
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for secret update event to be handled")
            .expect("secret update handler dropped before acknowledging");

        let plaintext = provider.get(&app_id, &key).await.unwrap();
        assert_eq!(plaintext, expected_value);
        let raw = store.load_secrets(&app_id).unwrap().unwrap();
        assert_ne!(raw, expected_value.as_bytes());
    }

    #[tokio::test]
    async fn test_secret_update_event_roundtrip_persists_encrypted_targeted_secret_via_secret_provider(
    ) {
        let _nats = start_test_nats().await.unwrap();
        let bus = NatsBus::connect(&_nats.url).await.unwrap();
        bus.setup_jetstream().await.unwrap();

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = std::sync::Arc::new(LocalSecretProvider::new(
            store.clone(),
            SymmetricKey::generate(),
        ));
        let recipient = BootstrapKeyPair::generate();
        let recipient_secret_bytes = recipient.secret_bytes();
        let recipient_public_bytes = recipient.public_bytes();
        let app_id = AppId("secret-app:v1".to_string());
        let key = "API_KEY".to_string();
        let expected_value = "super-secret-over-nats-encrypted".to_string();
        let (tx, rx) = oneshot::channel();
        let provider_for_handler = provider.clone();
        let app_id_for_handler = app_id.clone();
        let key_for_handler = key.clone();
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let tx_for_handler = tx.clone();

        bus.subscribe(
            &format!("secrets.update.{}.node-under-test", app_id.0),
            move |event| {
                let provider = provider_for_handler.clone();
                let tx = tx_for_handler.clone();
                let expected_app_id = app_id_for_handler.clone();
                let expected_key = key_for_handler.clone();
                let recipient = BootstrapKeyPair::from_secret_bytes(recipient_secret_bytes);
                async move {
                    if let Event::SecretUpdate {
                        app_id,
                        key,
                        target_node_id,
                        secret,
                    } = event
                    {
                        assert_eq!(app_id, expected_app_id);
                        assert_eq!(key, expected_key);
                        assert_eq!(target_node_id.as_deref(), Some("node-under-test"));
                        apply_secret_update(provider.as_ref(), &recipient, &app_id, &key, &secret)
                            .await
                            .unwrap();
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                    }
                }
            },
        )
        .await
        .unwrap();

        let ciphertext =
            encrypt_for_peer(&recipient_public_bytes, expected_value.as_bytes()).unwrap();
        bus.publish(&Event::SecretUpdate {
            app_id: app_id.clone(),
            key: key.clone(),
            target_node_id: Some("node-under-test".to_string()),
            secret: SecretTransportEnvelope::node_transport_ciphertext(ciphertext),
        })
        .await
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for encrypted secret update event to be handled")
            .expect("encrypted secret update handler dropped before acknowledging");

        let plaintext = provider.get(&app_id, &key).await.unwrap();
        assert_eq!(plaintext, expected_value);
    }

    #[tokio::test]
    async fn test_fetch_artifact_sends_signed_manifest_header() {
        use axum::{
            extract::Path,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        let wasm_bytes = b"artifact-manifest-header-test".to_vec();
        let expected_sha256 = hex::encode(sha2::Sha256::digest(&wasm_bytes));
        let authority = ArtifactTransferAuthority::derive("node-1", &[5u8; 32]);
        let requester_node_id = "node-2";
        let manifest =
            authority.issue_read_manifest_for_audience(&expected_sha256, requester_node_id);
        let expected_header = manifest.encode_header_value().unwrap();
        let app = Router::new().route(
            "/artifacts/{sha256}",
            get({
                let wasm_bytes = wasm_bytes.clone();
                let expected_header = expected_header.clone();
                move |Path(_sha256): Path<String>, headers: HeaderMap| {
                    let wasm_bytes = wasm_bytes.clone();
                    let expected_header = expected_header.clone();
                    async move {
                        if headers
                            .get(ARTIFACT_TRANSFER_MANIFEST_HEADER)
                            .and_then(|value| value.to_str().ok())
                            == Some(expected_header.as_str())
                            && headers
                                .get(ARTIFACT_TRANSFER_REQUESTER_NODE_HEADER)
                                .and_then(|value| value.to_str().ok())
                                == Some(requester_node_id)
                        {
                            (StatusCode::OK, wasm_bytes)
                        } else {
                            (StatusCode::FORBIDDEN, Vec::new())
                        }
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let fetched = fetch_artifact(
            &format!("http://{addr}/artifacts/{expected_sha256}"),
            Some(requester_node_id),
            Some(&manifest),
            &expected_sha256,
        )
        .await
        .unwrap();
        assert_eq!(fetched, wasm_bytes);
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_fetches_with_stored_authorization_header() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct ArtifactState {
            expected_auth: String,
            wasm_bytes: Vec<u8>,
        }

        let wasm_bytes = b"remote-artifact-ingest-ok".to_vec();
        let sha256 = hex::encode(sha2::Sha256::digest(&wasm_bytes));
        let state = ArtifactState {
            expected_auth: "Bearer super-token".to_string(),
            wasm_bytes: wasm_bytes.clone(),
        };

        let app = Router::new()
            .route(
                "/payload.wasm",
                get(
                    |State(state): State<ArtifactState>, headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        (StatusCode::OK, state.wasm_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store
            .save_cluster_node(&common::types::ClusterNodeRecord::new(
                "node-under-test".to_string(),
                super::now_unix_secs(),
            ))
            .unwrap();
        store
            .save_cluster_node(&common::types::ClusterNodeRecord::new(
                "node-peer".to_string(),
                super::now_unix_secs(),
            ))
            .unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(&artifact_credentials_app_id(), "ghcr-reader", "super-token")
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: sha256.clone(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, sha256);
        assert_eq!(response.size_bytes, wasm_bytes.len() as u64);
        assert_eq!(
            response.artifact_url,
            format!("http://node-under-test.internal:9091/artifacts/{sha256}")
        );
        assert_eq!(response.artifact_transfer_manifests.len(), 1);
        assert!(store.load_raw_wasm(&sha256).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_rejects_hash_mismatch() {
        use axum::{http::StatusCode, routing::get, Router};

        let app = Router::new().route(
            "/payload.wasm",
            get(|| async { (StatusCode::OK, b"wrong-bytes".to_vec()) }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());

        let err = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: "deadbeef".repeat(8),
                credential_ref: None,
                signature: None,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_rejects_oversized_payload() {
        use axum::{http::StatusCode, routing::get, Router};

        let oversized_body = vec![0u8; super::MAX_REMOTE_ARTIFACT_BYTES as usize + 1];
        let app = Router::new().route(
            "/payload.wasm",
            get(move || {
                let oversized_body = oversized_body.clone();
                async move { (StatusCode::OK, oversized_body) }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());

        let err = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: None,
                url: format!("http://{addr}/payload.wasm"),
                sha256: "deadbeef".repeat(8),
                credential_ref: None,
                signature: None,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("maximum size"));
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_resolves_oci_tag_to_blob() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct RegistryState {
            expected_auth: String,
            manifest_body: String,
            blob_bytes: Vec<u8>,
            blob_digest: String,
        }

        let blob_bytes = b"oci-registry-blob".to_vec();
        let blob_hash = hex::encode(sha2::Sha256::digest(&blob_bytes));
        let manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.unknown.config.v1+json",
                "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 2
            },
            "layers": [{
                "mediaType": "application/wasm",
                "digest": format!("sha256:{blob_hash}"),
                "size": blob_bytes.len()
            }]
        })
        .to_string();
        let state = RegistryState {
            expected_auth: "Bearer registry-token".to_string(),
            manifest_body,
            blob_bytes: blob_bytes.clone(),
            blob_digest: blob_hash.clone(),
        };

        let app = Router::new()
            .route(
                "/v2/example-org/hello-api/manifests/v1",
                get(
                    |State(state): State<RegistryState>, headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, String::new());
                        }
                        (StatusCode::OK, state.manifest_body.clone())
                    },
                ),
            )
            .route(
                "/v2/example-org/hello-api/blobs/{digest}",
                get(
                    |State(state): State<RegistryState>,
                     axum::extract::Path(digest): axum::extract::Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        if digest != format!("sha256:{}", state.blob_digest) {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        }
                        (StatusCode::OK, state.blob_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(
                &artifact_credentials_app_id(),
                "ghcr-reader",
                "authorization:Bearer registry-token",
            )
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: Some(format!(
                    "oci://127.0.0.1:{}/example-org/hello-api:v1",
                    addr.port()
                )),
                url: String::new(),
                sha256: String::new(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, blob_hash);
        assert_eq!(
            response.artifact_url,
            format!("http://node-under-test.internal:9091/artifacts/{blob_hash}")
        );
        assert!(store.load_raw_wasm(&blob_hash).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_ingest_remote_artifact_selects_matching_platform_from_oci_index() {
        use axum::{
            extract::{Path, State},
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };

        #[derive(Clone)]
        struct RegistryState {
            expected_auth: String,
            index_body: String,
            matching_manifest_body: String,
            non_matching_manifest_body: String,
            matching_blob_bytes: Vec<u8>,
            matching_blob_digest: String,
            matching_manifest_digest: String,
            non_matching_manifest_digest: String,
        }

        let matching_blob_bytes = b"oci-platform-match".to_vec();
        let matching_blob_hash = hex::encode(sha2::Sha256::digest(&matching_blob_bytes));
        let matching_manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{
                "mediaType": "application/wasm",
                "digest": format!("sha256:{matching_blob_hash}"),
                "size": matching_blob_bytes.len()
            }]
        })
        .to_string();
        let non_matching_manifest_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{
                "mediaType": "application/wasm",
                "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "size": 16
            }]
        })
        .to_string();
        let matching_manifest_digest =
            hex::encode(sha2::Sha256::digest(matching_manifest_body.as_bytes()));
        let non_matching_manifest_digest =
            hex::encode(sha2::Sha256::digest(non_matching_manifest_body.as_bytes()));
        let index_body = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{non_matching_manifest_digest}"),
                    "size": non_matching_manifest_body.len(),
                    "platform": {
                        "os": "linux",
                        "architecture": "arm64"
                    }
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": format!("sha256:{matching_manifest_digest}"),
                    "size": matching_manifest_body.len(),
                    "platform": {
                        "os": std::env::consts::OS,
                        "architecture": normalized_host_architecture()
                    }
                }
            ]
        })
        .to_string();

        let state = RegistryState {
            expected_auth: "Bearer registry-token".to_string(),
            index_body,
            matching_manifest_body,
            non_matching_manifest_body,
            matching_blob_bytes: matching_blob_bytes.clone(),
            matching_blob_digest: matching_blob_hash.clone(),
            matching_manifest_digest: matching_manifest_digest.clone(),
            non_matching_manifest_digest: non_matching_manifest_digest.clone(),
        };

        let app = Router::new()
            .route(
                "/v2/example-org/hello-api/manifests/{reference}",
                get(
                    |State(state): State<RegistryState>,
                     Path(reference): Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, String::new());
                        }
                        let body = if reference == "v1" {
                            state.index_body.clone()
                        } else if reference == format!("sha256:{}", state.matching_manifest_digest)
                        {
                            state.matching_manifest_body.clone()
                        } else if reference
                            == format!("sha256:{}", state.non_matching_manifest_digest)
                        {
                            state.non_matching_manifest_body.clone()
                        } else {
                            return (StatusCode::NOT_FOUND, String::new());
                        };
                        (StatusCode::OK, body)
                    },
                ),
            )
            .route(
                "/v2/example-org/hello-api/blobs/{digest}",
                get(
                    |State(state): State<RegistryState>,
                     Path(digest): Path<String>,
                     headers: HeaderMap| async move {
                        if headers
                            .get(reqwest::header::AUTHORIZATION.as_str())
                            .and_then(|value| value.to_str().ok())
                            != Some(state.expected_auth.as_str())
                        {
                            return (StatusCode::UNAUTHORIZED, Vec::new());
                        }
                        if digest != format!("sha256:{}", state.matching_blob_digest) {
                            return (StatusCode::NOT_FOUND, Vec::new());
                        }
                        (StatusCode::OK, state.matching_blob_bytes.clone())
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        provider
            .set(
                &artifact_credentials_app_id(),
                "ghcr-reader",
                "authorization:Bearer registry-token",
            )
            .await
            .unwrap();

        let response = ingest_remote_artifact(
            &store,
            &provider,
            "http://node-under-test.internal:9091",
            &ArtifactTransferAuthority::derive("node-under-test", &[9u8; 32]),
            "node-under-test",
            120,
            common::deploy::RemoteArtifactSource {
                reference: Some(format!(
                    "oci://127.0.0.1:{}/example-org/hello-api:v1",
                    addr.port()
                )),
                url: String::new(),
                sha256: String::new(),
                credential_ref: Some("ghcr-reader".to_string()),
                signature: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(response.expected_hash, matching_blob_hash);
        assert!(store.load_raw_wasm(&matching_blob_hash).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_apply_secret_update_rejects_bootstrap_payload_for_rotation() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let provider = LocalSecretProvider::new(store.clone(), SymmetricKey::generate());
        let app_id = AppId("secret-app:v1".to_string());

        let err = apply_secret_update(
            &provider,
            &BootstrapKeyPair::generate(),
            &app_id,
            "API_KEY",
            &SecretTransportEnvelope::bootstrap_peer_ciphertext(vec![0xff, 0xfe, 0xfd]),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("unexpected secret payload variant"));
        assert!(store.load_secrets(&app_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn test_handle_state_snapshot_accepts_first_matching_session_only() {
        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let bootstrap_session = Arc::new(Mutex::new(BootstrapSessionState {
            session_id: "session-1".to_string(),
            nonce: "nonce-1".to_string(),
            keypair: secrets::BootstrapKeyPair::generate(),
            applied: false,
        }));
        let dispatcher =
            build_test_dispatcher(store.clone(), Some(bootstrap_session.clone()), None).await;

        let stale_config = common::types::AppConfig {
            id: AppId("stale-app:v1".to_string()),
            fuel_quota: common::types::FuelQuota(1000),
            memory_limit: common::types::MemoryPages(4),
            max_instances: 1,
            idle_timeout_secs: 30,
            wasm_bind_port: 8080,
            env_vars: std::collections::HashMap::new(),
            secret_keys: vec![],
            extended_limits: None,
            health_check_path: None,
            db_max_connections: None,
            rate_limit: None,
            tenant_id: None,
            policy: None,
            namespace: "default".to_string(),
        };
        let accepted_config = common::types::AppConfig {
            id: AppId("accepted-app:v1".to_string()),
            ..stale_config.clone()
        };
        let duplicate_config = common::types::AppConfig {
            id: AppId("duplicate-app:v1".to_string()),
            ..stale_config.clone()
        };

        dispatcher
            .handle_state_snapshot(
                "stale-session".to_string(),
                "stale-nonce".to_string(),
                vec![stale_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&stale_config.id).unwrap().is_none(),
            "mismatched session/nonce snapshot must be ignored"
        );

        dispatcher
            .handle_state_snapshot(
                "session-1".to_string(),
                "nonce-1".to_string(),
                vec![accepted_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&accepted_config.id).unwrap().is_some(),
            "first matching snapshot should be applied"
        );

        dispatcher
            .handle_state_snapshot(
                "session-1".to_string(),
                "nonce-1".to_string(),
                vec![duplicate_config.clone()],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(
            store.load_config(&duplicate_config.id).unwrap().is_none(),
            "duplicate matching snapshot after apply must be ignored"
        );

        let bootstrap_state = bootstrap_session.lock().await;
        assert!(bootstrap_state.applied);
    }

    #[tokio::test]
    async fn test_route_webhook_uses_peer_ips_from_node_load_updates() {
        use axum::{
            extract::{Json, State},
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };
        use proxy::dns_webhook::RouteChangeWebhook;
        use std::sync::Arc as StdArc;
        use tokio::sync::oneshot;

        type WebhookCaptureSender =
            StdArc<std::sync::Mutex<Option<oneshot::Sender<(HeaderMap, RouteChangeWebhook)>>>>;

        #[derive(Clone)]
        struct WebhookState {
            sender: WebhookCaptureSender,
        }

        let (tx, rx) = oneshot::channel();
        let state = WebhookState {
            sender: StdArc::new(std::sync::Mutex::new(Some(tx))),
        };

        let app = Router::new()
            .route(
                "/dns",
                post(
                    |State(state): State<WebhookState>,
                     headers: HeaderMap,
                     Json(payload): Json<RouteChangeWebhook>| async move {
                        if let Some(tx) = state.sender.lock().unwrap().take() {
                            let _ = tx.send((headers, payload));
                        }
                        StatusCode::OK
                    },
                ),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp = NamedTempFile::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let webhook = proxy::dns_webhook::DnsWebhookManager::new(
            Some(format!("http://{addr}/dns")),
            Some("test-token".to_string()),
        )
        .unwrap();
        let dispatcher = build_test_dispatcher(store, None, Some(webhook)).await;

        dispatcher
            .handle(Event::NodeLoad {
                node_id: "node-remote".to_string(),
                cpu_percent: 10.0,
                fuel_budget_used_percent: 25.0,
                active_instances: 2,
                proxy_address: "10.0.0.42:8080".to_string(),
            })
            .await
            .unwrap();

        dispatcher
            .handle(Event::RouteAdd {
                route: common::types::Route {
                    host: "hello.example.com".to_string(),
                    app_id: AppId("hello:v1".to_string()),
                    path_prefix: "/".to_string(),
                    strip_prefix: false,
                    created_at: 1,
                    updated_at: 1,
                },
            })
            .await
            .unwrap();

        let (headers, payload) = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for DNS webhook")
            .expect("DNS webhook sender dropped");

        assert_eq!(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(payload.action, "add");
        assert_eq!(payload.hostname, "hello.example.com");
        assert_eq!(payload.app_id, "hello:v1");
        assert_eq!(payload.node_ips, vec!["10.0.0.42".to_string()]);
    }
}
