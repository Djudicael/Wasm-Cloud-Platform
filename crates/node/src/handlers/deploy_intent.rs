use common::{
    artifact_transfer::{ArtifactManifestAudienceBinding, ArtifactTransferAuthority},
    deploy::{
        DeployIntentRequest, DeployIntentResponse, RemoteArtifactIngressResponse,
        RemoteArtifactSource,
    },
    error::PlatformError,
    types::AppId,
};
use messaging::events::Event;
use reqwest::Url;
use secrets::SecretProvider;
use sha2::Digest;
use std::net::IpAddr;
use storage::Store;

const ARTIFACT_CREDENTIALS_APP_ID: &str = "_platform/artifact-credentials:v1";
const OCI_ACCEPT_HEADER: &str = concat!(
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.artifact.manifest.v1+json"
);
pub(super) const MAX_REMOTE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn is_loopback_artifact_url(url: &str) -> bool {
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

pub(super) fn validate_peer_artifact_url(
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

pub(crate) fn artifact_credentials_app_id() -> AppId {
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

pub(super) fn normalized_host_architecture() -> &'static str {
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
