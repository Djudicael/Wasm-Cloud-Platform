use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put},
    Json, Router,
};
use common::artifact_transfer::{
    ArtifactTransferAuthority, ArtifactTransferMethod, ArtifactUploadAuthorizationResponse,
    SignedArtifactTransferManifest, ARTIFACT_TRANSFER_MANIFEST_HEADER,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Store;

const MAX_ARTIFACT_SIZE: usize = 104_857_600; // 100 MB

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactRequestAction {
    Read,
    Write,
}

impl ArtifactRequestAction {
    fn as_method(self) -> ArtifactTransferMethod {
        match self {
            ArtifactRequestAction::Read => ArtifactTransferMethod::Get,
            ArtifactRequestAction::Write => ArtifactTransferMethod::Put,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactPeerTokenConfig {
    pub token: String,
    pub expires_at_ms: Option<u64>,
    pub allow_read: bool,
    pub allow_write: bool,
}

impl ArtifactPeerTokenConfig {
    pub fn new(
        token: String,
        expires_at_ms: Option<u64>,
        allow_read: bool,
        allow_write: bool,
    ) -> Self {
        ArtifactPeerTokenConfig {
            token,
            expires_at_ms,
            allow_read,
            allow_write,
        }
    }

    fn allows(&self, action: ArtifactRequestAction, now_ms: u64) -> bool {
        if let Some(expires_at_ms) = self.expires_at_ms {
            if now_ms >= expires_at_ms {
                return false;
            }
        }

        match action {
            ArtifactRequestAction::Read => self.allow_read,
            ArtifactRequestAction::Write => self.allow_write,
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn transfer_manifest(headers: &HeaderMap) -> Option<SignedArtifactTransferManifest> {
    headers
        .get(ARTIFACT_TRANSFER_MANIFEST_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| SignedArtifactTransferManifest::decode_header_value(value).ok())
}

fn mark_single_use_manifest_seen(
    used_transfer_ids: &Mutex<HashMap<String, u64>>,
    manifest: &SignedArtifactTransferManifest,
    now_ms: u64,
) -> Result<(), StatusCode> {
    if !manifest.manifest.single_use {
        return Ok(());
    }

    let mut seen = used_transfer_ids.lock().map_err(|_| {
        tracing::error!("artifact transfer replay cache lock poisoned");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    seen.retain(|_, expires_at_ms| *expires_at_ms > now_ms);

    if let Some(existing_expiry) = seen.get(&manifest.manifest.transfer_id) {
        tracing::warn!(
            transfer_id = %manifest.manifest.transfer_id,
            expires_at_ms = *existing_expiry,
            "rejected replayed single-use artifact transfer manifest"
        );
        return Err(StatusCode::FORBIDDEN);
    }

    seen.insert(
        manifest.manifest.transfer_id.clone(),
        manifest.manifest.expires_at_ms,
    );
    Ok(())
}

fn authorize_signed_manifest(
    peer_addr: SocketAddr,
    headers: &HeaderMap,
    manifest_authority: Option<&ArtifactTransferAuthority>,
    used_transfer_ids: &Mutex<HashMap<String, u64>>,
    action: ArtifactRequestAction,
    sha256: &str,
    now_ms: u64,
) -> Result<bool, StatusCode> {
    let Some(authority) = manifest_authority else {
        return Ok(false);
    };
    let Some(manifest) = transfer_manifest(headers) else {
        return Ok(false);
    };

    if let Err(err) = authority.verify_manifest(&manifest, sha256, action.as_method(), now_ms) {
        tracing::warn!(
            peer = %peer_addr,
            action = ?action,
            sha256,
            error = %err,
            "rejected remote artifact request with invalid signed transfer manifest"
        );
        return Ok(false);
    }

    mark_single_use_manifest_seen(used_transfer_ids, &manifest, now_ms)?;
    tracing::info!(
        peer = %peer_addr,
        action = ?action,
        sha256,
        transfer_id = %manifest.manifest.transfer_id,
        issuer = %manifest.manifest.issuer,
        single_use = manifest.manifest.single_use,
        "authorized remote artifact request via signed transfer manifest"
    );
    Ok(true)
}

fn ensure_authorized_peer(
    peer_addr: SocketAddr,
    headers: &HeaderMap,
    peer_tokens: &[ArtifactPeerTokenConfig],
    manifest_authority: Option<&ArtifactTransferAuthority>,
    used_transfer_ids: &Mutex<HashMap<String, u64>>,
    action: ArtifactRequestAction,
    sha256: &str,
) -> Result<(), StatusCode> {
    if peer_addr.ip().is_loopback() {
        return Ok(());
    }

    let now_ms = now_unix_ms();
    if authorize_signed_manifest(
        peer_addr,
        headers,
        manifest_authority,
        used_transfer_ids,
        action,
        sha256,
        now_ms,
    )? {
        return Ok(());
    }

    match bearer_token(headers) {
        Some(provided)
            if peer_tokens
                .iter()
                .any(|cfg| cfg.token == provided && cfg.allows(action, now_ms)) =>
        {
            Ok(())
        }
        Some(_) | None if !peer_tokens.is_empty() => {
            tracing::warn!(peer = %peer_addr, action = ?action, sha256, "rejected non-loopback artifact request with missing/invalid/expired compatible bearer token and no valid signed manifest");
            Err(StatusCode::FORBIDDEN)
        }
        _ => {
            tracing::warn!(peer = %peer_addr, action = ?action, sha256, "rejected non-loopback artifact request because no compatible bearer token or valid signed manifest is configured");
            Err(StatusCode::FORBIDDEN)
        }
    }
}

#[derive(Clone)]
struct ArtifactServerState {
    store: Store,
    peer_tokens: Vec<ArtifactPeerTokenConfig>,
    manifest_authority: Option<ArtifactTransferAuthority>,
    used_transfer_ids: Arc<Mutex<HashMap<String, u64>>>,
}

/// GET /artifacts/:sha256 — serve raw .wasm bytes.
/// Loopback peers are allowed implicitly; remote peers must present either a
/// valid signed transfer manifest or a configured compatibility bearer token.
async fn get_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Bytes, StatusCode> {
    ensure_authorized_peer(
        peer_addr,
        &headers,
        &s.peer_tokens,
        s.manifest_authority.as_ref(),
        &s.used_transfer_ids,
        ArtifactRequestAction::Read,
        &sha256,
    )?;

    match s.store.load_raw_wasm(&sha256) {
        Ok(Some(bytes)) => {
            tracing::debug!(sha256, bytes = bytes.len(), "artifact served");
            Ok(Bytes::from(bytes))
        }
        Ok(None) => {
            tracing::warn!(sha256, "artifact not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            tracing::error!(sha256, error = %e, "failed to load artifact");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// PUT /artifacts/:sha256 — store raw .wasm bytes.
/// Loopback peers are allowed implicitly; remote peers must present either a
/// valid signed transfer manifest or a configured compatibility bearer token.
async fn put_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<ArtifactUploadAuthorizationResponse>), StatusCode> {
    ensure_authorized_peer(
        peer_addr,
        &headers,
        &s.peer_tokens,
        s.manifest_authority.as_ref(),
        &s.used_transfer_ids,
        ArtifactRequestAction::Write,
        &sha256,
    )?;

    if body.len() > MAX_ARTIFACT_SIZE {
        tracing::warn!(
            sha256,
            size = body.len(),
            max = MAX_ARTIFACT_SIZE,
            "artifact too large"
        );
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let actual = hex::encode(Sha256::digest(&body));
    if actual != sha256 {
        tracing::warn!(expected = %sha256, actual, "SHA-256 mismatch on PUT");
        return Err(StatusCode::BAD_REQUEST);
    }

    match s.store.save_raw_wasm(&sha256, &body) {
        Ok(_) => {
            tracing::info!(sha256, bytes = body.len(), "artifact stored");
            let response = ArtifactUploadAuthorizationResponse {
                sha256: sha256.clone(),
                signed_get_manifest: s
                    .manifest_authority
                    .as_ref()
                    .map(|authority| authority.issue_read_manifest(&sha256)),
            };
            Ok((StatusCode::CREATED, Json(response)))
        }
        Err(e) => {
            tracing::error!(sha256, error = %e, "failed to store artifact");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Build the artifact server router.
pub fn artifact_router(
    store: Store,
    peer_tokens: Vec<ArtifactPeerTokenConfig>,
    manifest_authority: Option<ArtifactTransferAuthority>,
) -> Router {
    let state = ArtifactServerState {
        store,
        peer_tokens,
        manifest_authority,
        used_transfer_ids: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/artifacts/{sha256}", get(get_artifact))
        .route("/artifacts/{sha256}", put(put_artifact))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;

    fn authority() -> ArtifactTransferAuthority {
        ArtifactTransferAuthority::derive("node-1", &[9u8; 32])
    }

    #[tokio::test]
    async fn test_artifact_server_put_get() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let app = artifact_router(store, Vec::new(), Some(authority()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let wasm_bytes = b"fake wasm binary for testing";
        let sha256 = hex::encode(Sha256::digest(wasm_bytes));

        let put_url = format!("http://{}/artifacts/{}", addr, sha256);
        let resp = client
            .put(&put_url)
            .body(wasm_bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let upload_response: ArtifactUploadAuthorizationResponse =
            serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
        assert_eq!(upload_response.sha256, sha256);
        assert!(upload_response.signed_get_manifest.is_some());

        let get_url = format!("http://{}/artifacts/{}", addr, sha256);
        let resp = client.get(&get_url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), wasm_bytes);
    }

    #[tokio::test]
    async fn test_artifact_server_wrong_hash() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let app = artifact_router(store, Vec::new(), Some(authority()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let wasm_bytes = b"fake wasm binary";
        let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        let put_url = format!("http://{}/artifacts/{}", addr, wrong_sha256);
        let resp = client
            .put(&put_url)
            .body(wasm_bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_artifact_server_not_found() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let app = artifact_router(store, Vec::new(), Some(authority()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let missing_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let url = format!("http://{}/artifacts/{}", addr, missing_sha256);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_ensure_authorized_peer_accepts_valid_bearer_token() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let mut headers = HeaderMap::new();
        let used_transfer_ids = Mutex::new(HashMap::new());
        let peer_tokens = vec![ArtifactPeerTokenConfig::new(
            "peer-token".to_string(),
            None,
            true,
            true,
        )];

        assert!(ensure_authorized_peer(
            loopback,
            &headers,
            &[],
            None,
            &used_transfer_ids,
            ArtifactRequestAction::Read,
            "abc123"
        )
        .is_ok());
        assert_eq!(
            ensure_authorized_peer(
                remote,
                &headers,
                &peer_tokens,
                None,
                &used_transfer_ids,
                ArtifactRequestAction::Read,
                "abc123"
            )
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );

        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer peer-token".parse().unwrap(),
        );
        assert!(ensure_authorized_peer(
            remote,
            &headers,
            &peer_tokens,
            None,
            &used_transfer_ids,
            ArtifactRequestAction::Read,
            "abc123"
        )
        .is_ok());
    }

    #[test]
    fn test_ensure_authorized_peer_rejects_expired_token() {
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer expired-token".parse().unwrap(),
        );
        let used_transfer_ids = Mutex::new(HashMap::new());
        let peer_tokens = vec![ArtifactPeerTokenConfig::new(
            "expired-token".to_string(),
            Some(now_unix_ms().saturating_sub(1)),
            true,
            true,
        )];

        assert_eq!(
            ensure_authorized_peer(
                remote,
                &headers,
                &peer_tokens,
                None,
                &used_transfer_ids,
                ArtifactRequestAction::Read,
                "abc123"
            )
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn test_ensure_authorized_peer_enforces_scope() {
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer write-only-token".parse().unwrap(),
        );
        let used_transfer_ids = Mutex::new(HashMap::new());
        let peer_tokens = vec![ArtifactPeerTokenConfig::new(
            "write-only-token".to_string(),
            None,
            false,
            true,
        )];

        assert_eq!(
            ensure_authorized_peer(
                remote,
                &headers,
                &peer_tokens,
                None,
                &used_transfer_ids,
                ArtifactRequestAction::Read,
                "abc123"
            )
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert!(ensure_authorized_peer(
            remote,
            &headers,
            &peer_tokens,
            None,
            &used_transfer_ids,
            ArtifactRequestAction::Write,
            "abc123"
        )
        .is_ok());
    }

    #[test]
    fn test_ensure_authorized_peer_accepts_valid_signed_manifest() {
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let authority = authority();
        let manifest = authority.issue_read_manifest("abc123");
        let mut headers = HeaderMap::new();
        headers.insert(
            ARTIFACT_TRANSFER_MANIFEST_HEADER,
            manifest.encode_header_value().unwrap().parse().unwrap(),
        );
        let used_transfer_ids = Mutex::new(HashMap::new());

        assert!(ensure_authorized_peer(
            remote,
            &headers,
            &[],
            Some(&authority),
            &used_transfer_ids,
            ArtifactRequestAction::Read,
            "abc123"
        )
        .is_ok());
    }

    #[test]
    fn test_single_use_put_manifest_rejects_replay() {
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let authority = authority();
        let manifest = authority.issue_write_manifest("abc123");
        let mut headers = HeaderMap::new();
        headers.insert(
            ARTIFACT_TRANSFER_MANIFEST_HEADER,
            manifest.encode_header_value().unwrap().parse().unwrap(),
        );
        let used_transfer_ids = Mutex::new(HashMap::new());

        assert!(ensure_authorized_peer(
            remote,
            &headers,
            &[],
            Some(&authority),
            &used_transfer_ids,
            ArtifactRequestAction::Write,
            "abc123"
        )
        .is_ok());
        assert_eq!(
            ensure_authorized_peer(
                remote,
                &headers,
                &[],
                Some(&authority),
                &used_transfer_ids,
                ArtifactRequestAction::Write,
                "abc123"
            )
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }
}
