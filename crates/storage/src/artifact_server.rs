// crates/storage/src/artifact_server.rs
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, put},
    Router,
};
use sha2::{Digest, Sha256};

use crate::Store;

const MAX_ARTIFACT_SIZE: usize = 104_857_600; // 100 MB

#[derive(Clone)]
struct ArtifactServerState {
    store: Store,
}

/// GET /artifacts/:sha256 — serve raw .wasm bytes
async fn get_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
) -> Result<Bytes, StatusCode> {
    // We use sha256 as the lookup key for raw .wasm (pre-compilation)
    // This is separate from the compiled artifact stored under AppId
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

/// PUT /artifacts/:sha256 — store raw .wasm bytes (localhost only)
async fn put_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    body: Bytes,
) -> StatusCode {
    // Check size limit
    if body.len() > MAX_ARTIFACT_SIZE {
        tracing::warn!(
            sha256,
            size = body.len(),
            max = MAX_ARTIFACT_SIZE,
            "artifact too large"
        );
        return StatusCode::PAYLOAD_TOO_LARGE;
    }

    // Verify hash before storing
    let actual = format!("{:x}", Sha256::digest(&body));
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
pub fn artifact_router(store: Store) -> Router {
    let state = ArtifactServerState { store };
    Router::new()
        .route("/artifacts/:sha256", get(get_artifact))
        .route("/artifacts/:sha256", put(put_artifact))
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

        let app = artifact_router(store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let wasm_bytes = b"fake wasm binary for testing";
        let sha256 = format!("{:x}", Sha256::digest(wasm_bytes));

        // 1. PUT artifact
        let put_url = format!("http://{}/artifacts/{}", addr, sha256);
        let resp = client
            .put(&put_url)
            .body(wasm_bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // 2. GET artifact
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

        let app = artifact_router(store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let wasm_bytes = b"fake wasm binary";
        let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        // PUT with wrong hash should fail
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

        let app = artifact_router(store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let unknown_sha256 = "1111111111111111111111111111111111111111111111111111111111111111";

        // GET unknown artifact should return 404
        let get_url = format!("http://{}/artifacts/{}", addr, unknown_sha256);
        let resp = client.get(&get_url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
