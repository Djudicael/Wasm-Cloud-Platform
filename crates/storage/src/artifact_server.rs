// crates/storage/src/artifact_server.rs
use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put},
    Router,
};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;

use crate::Store;

const MAX_ARTIFACT_SIZE: usize = 104_857_600; // 100 MB

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn ensure_authorized_peer(
    peer_addr: SocketAddr,
    headers: &HeaderMap,
    peer_tokens: &[String],
) -> Result<(), StatusCode> {
    if peer_addr.ip().is_loopback() {
        return Ok(());
    }

    match bearer_token(headers) {
        Some(provided) if peer_tokens.iter().any(|expected| expected == provided) => Ok(()),
        Some(_) | None if !peer_tokens.is_empty() => {
            tracing::warn!(peer = %peer_addr, "rejected non-loopback artifact request with missing/invalid bearer token");
            Err(StatusCode::FORBIDDEN)
        }
        _ => {
            tracing::warn!(peer = %peer_addr, "rejected non-loopback artifact request because no peer artifact token is configured");
            Err(StatusCode::FORBIDDEN)
        }
    }
}

#[derive(Clone)]
struct ArtifactServerState {
    store: Store,
    peer_tokens: Vec<String>,
}

/// GET /artifacts/:sha256 — serve raw .wasm bytes.
/// Loopback peers are allowed implicitly; remote peers must present the configured bearer token.
async fn get_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Bytes, StatusCode> {
    ensure_authorized_peer(peer_addr, &headers, &s.peer_tokens)?;

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
/// Loopback peers are allowed implicitly; remote peers must present the configured bearer token.
async fn put_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if let Err(code) = ensure_authorized_peer(peer_addr, &headers, &s.peer_tokens) {
        return code;
    }

    if body.len() > MAX_ARTIFACT_SIZE {
        tracing::warn!(
            sha256,
            size = body.len(),
            max = MAX_ARTIFACT_SIZE,
            "artifact too large"
        );
        return StatusCode::PAYLOAD_TOO_LARGE;
    }

    let actual = hex::encode(Sha256::digest(&body));
    if actual != sha256 {
        tracing::warn!(expected = %sha256, actual, "SHA-256 mismatch on PUT");
        return StatusCode::BAD_REQUEST;
    }

    match s.store.save_raw_wasm(&sha256, &body) {
        Ok(_) => {
            tracing::info!(sha256, bytes = body.len(), "artifact stored");
            StatusCode::CREATED
        }
        Err(e) => {
            tracing::error!(sha256, error = %e, "failed to store artifact");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Build the artifact server router.
pub fn artifact_router(store: Store, peer_tokens: Vec<String>) -> Router {
    let state = ArtifactServerState { store, peer_tokens };
    Router::new()
        .route("/artifacts/{sha256}", get(get_artifact))
        .route("/artifacts/{sha256}", put(put_artifact))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use sha2::{Digest, Sha256};
    use tempfile::NamedTempFile;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_artifact_server_put_get() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let app = artifact_router(store, Vec::new());
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

        let app = artifact_router(store, Vec::new());
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

        let app = artifact_router(store, Vec::new());
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
    fn test_ensure_authorized_peer() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let remote: SocketAddr = "10.0.0.5:8080".parse().unwrap();
        let mut headers = HeaderMap::new();
        let peer_tokens = vec!["peer-token".to_string()];

        assert!(ensure_authorized_peer(loopback, &headers, &[]).is_ok());
        assert_eq!(
            ensure_authorized_peer(remote, &headers, &peer_tokens).unwrap_err(),
            StatusCode::FORBIDDEN
        );

        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer peer-token".parse().unwrap(),
        );
        assert!(ensure_authorized_peer(remote, &headers, &peer_tokens).is_ok());
    }
}
