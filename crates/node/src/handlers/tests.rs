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
    types::{AppConfig, AppId, AppRateLimitConfig},
};
use e2e::NatsContainer;
use messaging::{events::Event, NatsBus};
use secrets::{
    crypto::SymmetricKey, encrypt_for_peer, BootstrapKeyPair, LocalSecretProvider, SecretProvider,
    SecretTransportEnvelope,
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

#[tokio::test]
async fn test_app_rate_limit_override_is_applied_and_removed() {
    let temp = NamedTempFile::new().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let dispatcher = build_test_dispatcher(store, None, None).await;
    let app_id = AppId("default/limited:v1".to_string());
    let mut config = AppConfig::default_for(app_id.clone());
    config.rate_limit = Some(AppRateLimitConfig {
        requests_per_second: 5_000,
        burst_capacity: 10_000,
        per_ip_limit: 4_000,
    });

    dispatcher.apply_rate_limit(&app_id, &config);
    let applied = dispatcher.rate_limiter.get_app_config(&app_id.0);
    assert_eq!(applied.requests_per_second, 5_000);
    assert_eq!(applied.burst_capacity, 10_000);
    assert_eq!(applied.per_ip_limit, 4_000);

    config.rate_limit = None;
    dispatcher.apply_rate_limit(&app_id, &config);
    let restored = dispatcher.rate_limiter.get_app_config(&app_id.0);
    assert_eq!(restored.requests_per_second, 1_000);
    assert_eq!(restored.burst_capacity, 50);
    assert_eq!(restored.per_ip_limit, 100);
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
        rate_limiter: Arc::new(proxy::rate_limiter::RateLimiter::new(
            proxy::rate_limiter::RateLimitConfig::default(),
        )),
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
async fn test_secret_update_event_roundtrip_persists_encrypted_targeted_secret_via_secret_provider()
{
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

    let ciphertext = encrypt_for_peer(&recipient_public_bytes, expected_value.as_bytes()).unwrap();
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
    let manifest = authority.issue_read_manifest_for_audience(&expected_sha256, requester_node_id);
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
                    } else if reference == format!("sha256:{}", state.matching_manifest_digest) {
                        state.matching_manifest_body.clone()
                    } else if reference == format!("sha256:{}", state.non_matching_manifest_digest)
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
