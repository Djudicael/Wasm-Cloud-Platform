//! Bootstrap and state transfer for cluster convergence.
//!
//! This module owns node join snapshot export/import, bootstrap session
//! bookkeeping, and the secret transport helpers that support both bootstrap
//! and secret rotation flows.

use std::collections::HashMap;
use std::sync::Arc;

use common::{
    artifact_transfer::{ArtifactTransferAuthority, BootstrapArtifactFetchAuthorization},
    error::PlatformError,
    types::AppId,
};
use messaging::events::Event;
use proxy::{router::HostRouter, upstream::UpstreamRegistry};
use runtime::WasmRuntime;
use secrets::{
    encrypt_for_peer, BootstrapKeyPair, SecretProvider, SecretTransportEntry,
    SecretTransportEnvelope, SecretTransportPayload,
};
use storage::Store;
use supervisor::Supervisor;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::{fetch_artifact, merge_cluster_node_record, now_unix_ms, now_unix_secs};
use crate::handlers::deploy_intent::validate_peer_artifact_url;

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

pub(crate) async fn apply_secret_update<S: SecretProvider + ?Sized>(
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

pub(crate) struct BootstrapContext<'a> {
    pub supervisor: &'a Arc<Supervisor>,
    pub upstream: &'a Arc<UpstreamRegistry>,
    pub host_router: &'a Arc<HostRouter>,
    pub store: &'a Store,
    pub runtime: &'a WasmRuntime,
    pub node_id: &'a str,
    pub artifact_server_url: &'a str,
    pub artifact_transfer_authority: &'a ArtifactTransferAuthority,
    pub secret_provider: &'a Arc<dyn SecretProvider>,
    pub bootstrap_session: Option<&'a Arc<Mutex<BootstrapSessionState>>>,
    pub bus: &'a messaging::NatsBus,
    pub gateway: Option<&'a Arc<proxy::gateway::Gateway>>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_node_joined(
    ctx: BootstrapContext<'_>,
    new_node_id: String,
    bootstrap_session_id: String,
    bootstrap_nonce: String,
    peer_artifact_url: String,
    peer_public_key: Vec<u8>,
    protocol_version: u32,
    binary_version: String,
) -> Result<(), PlatformError> {
    let _ = (&ctx.supervisor, &ctx.upstream);
    info!(
        new_node = %new_node_id,
        protocol = protocol_version,
        version = %binary_version,
        "node joined cluster"
    );

    if new_node_id == ctx.node_id {
        tracing::debug!(new_node = %new_node_id, "ignoring our own NodeJoined event");
        return Ok(());
    }

    validate_peer_artifact_url(&new_node_id, &peer_artifact_url)?;

    let mut cluster_node =
        merge_cluster_node_record(ctx.store.load_cluster_node(&new_node_id)?, &new_node_id);
    let now_secs = now_unix_secs();
    cluster_node.last_seen_unix_secs = now_secs;
    cluster_node.joined_at_unix_secs = Some(cluster_node.joined_at_unix_secs.unwrap_or(now_secs));
    cluster_node.artifact_server_url = Some(peer_artifact_url.clone());
    cluster_node.protocol_version = Some(protocol_version);
    cluster_node.binary_version = Some(binary_version.clone());
    ctx.store.save_cluster_node(&cluster_node)?;

    info!(
        new_node = %new_node_id,
        our_node = %ctx.node_id,
        "sending state snapshot to new node"
    );

    let app_ids = ctx.store.list_apps()?;
    let mut configs = Vec::with_capacity(app_ids.len());
    for id in &app_ids {
        if let Some(config) = ctx.store.load_config(id)? {
            configs.push(config);
        }
    }

    let routes = ctx.store.list_routes()?;

    let mut encrypted_secrets: Vec<SecretTransportEntry> = Vec::new();
    for config in &configs {
        let keys = ctx.secret_provider.list_keys(&config.id).await?;
        for key in keys {
            let value = ctx.secret_provider.get(&config.id, &key).await?;
            let encrypted = encrypt_for_peer(&peer_public_key, value.as_bytes())?;
            encrypted_secrets.push(SecretTransportEntry {
                app_id: config.id.0.clone(),
                key,
                envelope: SecretTransportEnvelope::bootstrap_peer_ciphertext(encrypted),
            });
        }
    }

    let gateway_configs = ctx.store.list_gateway_configs()?;
    let mut api_keys = Vec::new();
    let mut artifact_hashes = Vec::new();
    let mut artifact_fetches = Vec::new();
    for config in &configs {
        if let Some(hash) = ctx.store.get_artifact_sha256(&config.id)? {
            artifact_hashes.push((config.id.0.clone(), hash.clone()));
            artifact_fetches.push(BootstrapArtifactFetchAuthorization {
                app_id: config.id.0.clone(),
                sha256: hash.clone(),
                artifact_url: format!("{}/artifacts/{}", ctx.artifact_server_url, hash),
                artifact_transfer_manifest: Some(
                    ctx.artifact_transfer_authority
                        .issue_read_manifest_for_audience(&hash, &new_node_id),
                ),
            });
        }
        let keys = ctx.store.load_api_keys(&config.id.0)?;
        if !keys.is_empty() {
            api_keys.push((config.id.0.clone(), keys));
        }
    }

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

    ctx.bus.publish(&event).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_state_snapshot(
    ctx: BootstrapContext<'_>,
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
    let _ = (&ctx.supervisor, &ctx.upstream);
    info!(
        session = %bootstrap_session_id,
        apps = configs.len(),
        routes = routes.len(),
        secrets = encrypted_secrets.len(),
        "received state snapshot"
    );

    let Some(bootstrap_session) = ctx.bootstrap_session else {
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

    for config in &configs {
        ctx.store.save_config(config)?;
    }

    for route in &routes {
        ctx.store.save_route(route)?;
        ctx.host_router
            .add_route(
                route.host.clone(),
                route.path_prefix.clone(),
                route.app_id.clone(),
                route.strip_prefix,
            )
            .await;
    }

    for (app_id, config) in gateway_configs {
        ctx.store.save_gateway_config(&app_id, &config)?;
        if let Some(gw) = ctx.gateway {
            gw.set_route_config(&app_id, config).await;
        }
    }

    for (app_id, keys) in api_keys {
        ctx.store.save_api_keys(&app_id, &keys)?;
        if !keys.is_empty() {
            let validator = proxy::gateway::api_key::ApiKeyValidator::new(keys);
            if let Some(gw) = ctx.gateway {
                gw.set_api_key_validator(&app_id, validator).await;
            }
        }
    }

    for SecretTransportEntry {
        app_id: app_id_str,
        key,
        envelope,
    } in encrypted_secrets
    {
        let app_id = AppId(app_id_str.clone());
        let plaintext = decrypt_bootstrap_secret(&bootstrap_state.keypair, &envelope)?;
        ctx.secret_provider.set(&app_id, &key, &plaintext).await?;
        info!(app = app_id_str, key, "secret decrypted and stored");
    }

    for (app_id_str, sha256) in &artifact_hashes {
        let app_id = AppId(app_id_str.clone());
        ctx.store.save_artifact_hash(&app_id, sha256)?;
    }

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
                Some(ctx.node_id),
                fetch.artifact_transfer_manifest.as_ref(),
                &sha256,
            )
            .await
            {
                Ok(raw) => {
                    ctx.store.save_raw_wasm(&sha256, &raw)?;
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
            let mut attempts = 0;
            loop {
                if let Ok(Some(raw)) = ctx.store.load_raw_wasm(&sha256) {
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
            let runtime = ctx.runtime.clone();
            let store = ctx.store.clone();
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

    persist_applied_bootstrap_session(ctx.store, &bootstrap_session_id, &bootstrap_nonce)?;

    info!(session = %bootstrap_session_id, "state snapshot import complete");
    Ok(())
}
