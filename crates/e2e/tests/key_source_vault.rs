mod harness;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use harness::{reserve_test_port, NatsContainer, NodeProcess};
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
struct MockVaultState {
    expected_token: Arc<str>,
    kv_field: Arc<str>,
    kv_value: Arc<str>,
    expected_transit_input: Arc<str>,
    transit_hmac_hex: Arc<str>,
}

struct MockVaultServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockVaultServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_mock_vault_server(
    expected_token: &str,
    kv_field: &str,
    kv_value: &str,
    expected_transit_input: &str,
    transit_hmac_hex: &str,
) -> MockVaultServer {
    let state = MockVaultState {
        expected_token: Arc::from(expected_token),
        kv_field: Arc::from(kv_field),
        kv_value: Arc::from(kv_value),
        expected_transit_input: Arc::from(expected_transit_input),
        transit_hmac_hex: Arc::from(transit_hmac_hex),
    };

    let app = Router::new()
        .route("/v1/secret/data/{*path}", get(handle_vault_kv))
        .route(
            "/v1/transit/hmac/{key}/sha2-256",
            post(handle_vault_transit),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind mock vault");
    let addr = listener.local_addr().expect("mock vault addr");
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock vault serve");
    });

    MockVaultServer {
        url: format!("http://{}", addr),
        task,
    }
}

async fn handle_vault_kv(
    State(state): State<MockVaultState>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    let authorized = headers.get("x-vault-token").and_then(|v| v.to_str().ok())
        == Some(state.expected_token.as_ref());

    if !authorized {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "errors": ["forbidden"] })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "data": {
                "data": {
                    state.kv_field.as_ref(): state.kv_value.as_ref()
                }
            }
        })),
    )
}

async fn handle_vault_transit(
    State(state): State<MockVaultState>,
    headers: HeaderMap,
    Path(_key): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let authorized = headers.get("x-vault-token").and_then(|v| v.to_str().ok())
        == Some(state.expected_token.as_ref());
    let input_matches = payload.get("input").and_then(|v| v.as_str())
        == Some(state.expected_transit_input.as_ref());

    if !authorized || !input_matches {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "errors": ["forbidden"] })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "data": {
                "hmac": format!("vault:v1:{}", state.transit_hmac_hex)
            }
        })),
    )
}

#[tokio::test]
async fn test_node_starts_and_restarts_with_vault_kv_key_source() {
    let nats = NatsContainer::start(reserve_test_port().expect("reserve nats port"))
        .await
        .expect("start nats");
    let admin_port = reserve_test_port().expect("reserve admin port");
    let proxy_port = reserve_test_port().expect("reserve proxy port");
    let artifact_port = reserve_test_port().expect("reserve artifact port");

    let token_env = format!("WASM_NODE_E2E_VAULT_KV_TOKEN_{}", admin_port);
    let token_value = "vault-kv-token-e2e";
    let _vault = start_mock_vault_server(
        token_value,
        "key",
        "3333333333333333333333333333333333333333333333333333333333333333",
        "",
        "",
    )
    .await;

    let extra_args = vec![
        "--key-source",
        "vault-kv",
        "--key-vault-url",
        _vault.url.as_str(),
        "--key-vault-token-env",
        token_env.as_str(),
        "--key-vault-mount",
        "secret",
        "--key-vault-path",
        "wasm-node/seal-key",
        "--key-vault-field",
        "key",
    ];
    let extra_env = vec![(token_env.as_str(), token_value)];

    let node = NodeProcess::start_with_admin_and_options(
        "vault-kv-e2e",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
        &extra_args,
        &extra_env,
    )
    .await
    .expect("node should start with vault-kv key source");

    let (db_path, temp_dir) = node.extract_db();

    let restarted = NodeProcess::start_with_db_and_admin_and_options(
        "vault-kv-e2e",
        &nats.url,
        reserve_test_port().expect("reserve restart proxy port"),
        reserve_test_port().expect("reserve restart artifact port"),
        reserve_test_port().expect("reserve restart admin port"),
        db_path,
        temp_dir,
        &extra_args,
        &extra_env,
    )
    .await
    .expect("node should restart with vault-kv key source");

    restarted.stop().expect("stop restarted node");
}

#[tokio::test]
async fn test_node_starts_and_restarts_with_vault_transit_key_source() {
    let nats = NatsContainer::start(reserve_test_port().expect("reserve nats port"))
        .await
        .expect("start nats");
    let admin_port = reserve_test_port().expect("reserve admin port");
    let proxy_port = reserve_test_port().expect("reserve proxy port");
    let artifact_port = reserve_test_port().expect("reserve artifact port");

    let token_env = format!("WASM_NODE_E2E_VAULT_TRANSIT_TOKEN_{}", admin_port);
    let token_value = "vault-transit-token-e2e";
    let _vault = start_mock_vault_server(
        token_value,
        "ignored",
        "ignored",
        "cHJvZC1ub2RlLTA=",
        "5555555555555555555555555555555555555555555555555555555555555555",
    )
    .await;

    let extra_args = vec![
        "--key-source",
        "vault-transit",
        "--key-vault-url",
        _vault.url.as_str(),
        "--key-vault-token-env",
        token_env.as_str(),
        "--key-vault-transit-mount",
        "transit",
        "--key-vault-transit-key",
        "wasm-node-seal",
        "--key-vault-transit-context",
        "prod-node-0",
    ];
    let extra_env = vec![(token_env.as_str(), token_value)];

    let node = NodeProcess::start_with_admin_and_options(
        "vault-transit-e2e",
        &nats.url,
        proxy_port,
        artifact_port,
        admin_port,
        &extra_args,
        &extra_env,
    )
    .await
    .expect("node should start with vault-transit key source");

    let (db_path, temp_dir) = node.extract_db();

    let restarted = NodeProcess::start_with_db_and_admin_and_options(
        "vault-transit-e2e",
        &nats.url,
        reserve_test_port().expect("reserve restart proxy port"),
        reserve_test_port().expect("reserve restart artifact port"),
        reserve_test_port().expect("reserve restart admin port"),
        db_path,
        temp_dir,
        &extra_args,
        &extra_env,
    )
    .await
    .expect("node should restart with vault-transit key source");

    restarted.stop().expect("stop restarted node");
}
