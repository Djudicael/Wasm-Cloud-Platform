use super::args::DeployArgs;
use anyhow::Result;
use colored::Colorize;
use common::artifact_transfer::{
    ArtifactManifestBatchRequest, ArtifactManifestBatchResponse,
    ArtifactUploadAuthorizationResponse,
};
use common::deploy::{
    ArtifactSignature, DeployIntentRequest, DeployIntentResponse, RemoteArtifactIngressResponse,
    RemoteArtifactSource,
};
use common::types::ClusterNodeRecord;
use indicatif::{ProgressBar, ProgressStyle};

pub(super) enum ArtifactInput {
    LocalPath(String),
    Remote(Box<RemoteArtifactSource>),
}

pub(super) fn build_artifact_signature(args: &DeployArgs) -> Result<Option<ArtifactSignature>> {
    match (&args.artifact_public_key, &args.artifact_signature) {
        (None, None) => Ok(None),
        (Some(public_key), Some(signature)) => {
            let algorithm = args.artifact_signature_algorithm.trim().to_lowercase();
            if algorithm != "ed25519"
                && algorithm != "cosign-ed25519"
                && algorithm != "sigstore-bundle"
            {
                anyhow::bail!(
                    "--artifact-signature-algorithm must be one of ed25519, cosign-ed25519, or sigstore-bundle"
                );
            }
            if (algorithm == "cosign-ed25519" || algorithm == "sigstore-bundle")
                && args.artifact_signature_payload.is_none()
            {
                anyhow::bail!(
                    "--artifact-signature-payload is required when --artifact-signature-algorithm is cosign-ed25519 or sigstore-bundle"
                );
            }
            if algorithm == "sigstore-bundle" && args.artifact_identity.is_none() {
                anyhow::bail!(
                    "--artifact-identity is required when --artifact-signature-algorithm=sigstore-bundle"
                );
            }
            Ok(Some(ArtifactSignature {
                algorithm,
                public_key: public_key.clone(),
                signature: signature.clone(),
                payload: args.artifact_signature_payload.clone(),
                issuer: args.artifact_issuer.clone(),
                identity: args.artifact_identity.clone(),
                repository: args.artifact_repository.clone(),
                namespace: args.artifact_namespace.clone(),
            }))
        }
        _ => anyhow::bail!(
            "--artifact-public-key and --artifact-signature must be provided together"
        ),
    }
}

pub(super) fn remote_source_from_oci_reference(
    reference: &str,
    credential_ref: Option<String>,
    signature: Option<ArtifactSignature>,
) -> Result<RemoteArtifactSource> {
    let without_scheme = reference
        .strip_prefix("oci://")
        .ok_or_else(|| anyhow::anyhow!("OCI artifact reference must start with oci://"))?;
    let slash_index = without_scheme.find('/').ok_or_else(|| {
        anyhow::anyhow!("OCI artifact reference must include a registry and repository path")
    })?;
    let registry = &without_scheme[..slash_index];
    let repo_and_ref = &without_scheme[slash_index + 1..];
    let last_slash = repo_and_ref.rfind('/').unwrap_or(0);
    let has_digest = repo_and_ref.rfind('@').is_some();
    let has_tag = repo_and_ref
        .rfind(':')
        .map(|idx| idx > last_slash)
        .unwrap_or(false);
    if registry.trim().is_empty() || repo_and_ref.trim().is_empty() || (!has_digest && !has_tag) {
        anyhow::bail!("OCI artifact reference must include a non-empty registry, repository, and tag or digest");
    }

    Ok(RemoteArtifactSource {
        reference: Some(reference.to_string()),
        url: String::new(),
        sha256: String::new(),
        credential_ref,
        signature,
    })
}

#[derive(serde::Deserialize)]
pub(super) struct ClusterNodeRegistryResponse {
    pub nodes: Vec<ClusterNodeRecord>,
    #[serde(default = "default_cluster_node_staleness_secs")]
    pub active_staleness_secs: u64,
}

fn default_cluster_node_staleness_secs() -> u64 {
    120
}

pub(super) async fn load_cluster_node_registry(
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

pub(super) fn select_target_node_ids(
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

pub(super) async fn request_per_node_manifests(
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

pub(super) fn resolve_artifact_input(
    args: &DeployArgs,
    manifest: Option<&super::super::manifest::DeployManifest>,
) -> Result<ArtifactInput> {
    let cli_signature = build_artifact_signature(args)?;
    let manifest_remote = match manifest.and_then(|m| m.artifact.as_ref()) {
        Some(artifact) if artifact.reference.is_some() && !artifact.url.trim().is_empty() => {
            anyhow::bail!("manifest artifact section cannot specify both reference and url");
        }
        Some(artifact) if artifact.reference.is_some() => Some(remote_source_from_oci_reference(
            artifact.reference.as_deref().unwrap_or_default(),
            artifact.credential_ref.clone(),
            artifact.signature.clone(),
        )?),
        Some(artifact) if !artifact.url.trim().is_empty() => Some(RemoteArtifactSource {
            reference: None,
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
            credential_ref: artifact.credential_ref.clone(),
            signature: artifact.signature.clone(),
        }),
        Some(_) => None,
        None => None,
    };

    if args.artifact_url.is_some() && args.artifact_ref.is_some() {
        anyhow::bail!(
            "remote artifact input cannot specify both --artifact-url and --artifact-ref"
        );
    }

    let cli_remote = if let Some(reference) = args.artifact_ref.as_deref() {
        Some(remote_source_from_oci_reference(
            reference,
            args.artifact_credential.clone(),
            cli_signature.clone(),
        )?)
    } else {
        args.artifact_url.as_ref().map(|url| RemoteArtifactSource {
            reference: None,
            url: url.clone(),
            sha256: args.sha256.clone().unwrap_or_default(),
            credential_ref: args.artifact_credential.clone(),
            signature: cli_signature,
        })
    };

    if args.wasm.is_some()
        && (args.artifact_url.is_some() || args.artifact_ref.is_some() || manifest_remote.is_some())
    {
        anyhow::bail!("local --wasm and remote artifact deploy inputs are mutually exclusive");
    }

    if (args.artifact_url.is_some() || args.artifact_ref.is_some()) && manifest_remote.is_some() {
        anyhow::bail!("remote artifact input cannot be specified in both CLI flags and manifest");
    }

    if let Some(remote) = cli_remote.or(manifest_remote) {
        if remote.reference.is_none() && remote.url.trim().is_empty() {
            anyhow::bail!("remote artifact URL cannot be empty");
        }
        if remote.reference.is_none() && remote.sha256.trim().is_empty() {
            anyhow::bail!("remote artifact deploy requires --sha256 or manifest artifact.sha256");
        }
        return Ok(ArtifactInput::Remote(Box::new(remote)));
    }

    let wasm_path = args
        .wasm
        .clone()
        .or_else(|| manifest.map(|m| m.app.wasm_artifact.clone()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("--wasm is required when no remote artifact is provided"))?;

    Ok(ArtifactInput::LocalPath(wasm_path))
}

#[allow(dead_code)]
pub(super) async fn upload_local_artifact(
    http: &reqwest::Client,
    node_api: &str,
    sha256: &str,
    wasm_bytes: Vec<u8>,
) -> Result<RemoteArtifactIngressResponse> {
    let size_bytes = wasm_bytes.len() as u64;
    let upload_url = format!("{}/artifacts/{}", node_api.trim_end_matches('/'), sha256);

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

    let response = http.put(&upload_url).body(wasm_bytes).send().await?;
    pb.finish_with_message("uploaded");

    if !response.status().is_success() {
        anyhow::bail!("Artifact upload failed: {}", response.status());
    }

    let upload_authorization = response
        .json::<ArtifactUploadAuthorizationResponse>()
        .await
        .ok();
    println!("{} Artifact uploaded to {}", "✓".green(), upload_url);

    Ok(RemoteArtifactIngressResponse {
        source_node_id: upload_authorization
            .as_ref()
            .and_then(|authorization| authorization.signed_get_manifest.as_ref())
            .map(|manifest| manifest.manifest.issuer.clone())
            .unwrap_or_default(),
        artifact_url: upload_url,
        expected_hash: sha256.to_string(),
        size_bytes,
        artifact_transfer_manifests: Vec::new(),
    })
}

pub(super) async fn submit_deploy_intent(
    http: &reqwest::Client,
    deploy_api: &str,
    request: DeployIntentRequest,
) -> Result<DeployIntentResponse> {
    let intent_url = format!("{}/deploy/intent", deploy_api.trim_end_matches('/'));
    let response = http.post(&intent_url).json(&request).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "deploy intent failed: HTTP {} from {}{}",
            status,
            intent_url,
            if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        );
    }

    Ok(response.json::<DeployIntentResponse>().await?)
}
