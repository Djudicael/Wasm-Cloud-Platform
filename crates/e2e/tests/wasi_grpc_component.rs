mod harness;

use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use harness::*;
use hyper_util::rt::TokioIo;
use messaging::events::Event;
use tokio::net::TcpStream;
use tonic::codegen::http::Uri;
use tonic::transport::Endpoint;
use tower::service_fn;

pub mod pb {
    tonic::include_proto!("echo");
}

use pb::echo_service_client::EchoServiceClient;
use pb::EchoRequest;

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_deploy_and_serve_wasi_grpc_component() {
    let nats = NatsContainer::start(4422)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start("wasi-grpc-node-0", &nats.url, 8380, 9201)
        .await
        .expect("Failed to start node");

    let wasm_path = find_wasi_grpc_echo_wasm().expect("wasi-grpc-echo.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).expect("Failed to get file size").len();
    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact to node artifact server");
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    let authorize_url = format!(
        "http://127.0.0.1:{}/artifacts/{}/authorize",
        node.artifact_port, sha256
    );
    let authorize_response = reqwest::Client::new()
        .post(&authorize_url)
        .json(&ArtifactManifestBatchRequest {
            audiences: vec![node.node_id.clone()],
        })
        .send()
        .await
        .expect("Failed to authorize artifact manifests");
    assert!(
        authorize_response.status().is_success(),
        "artifact authorize failed with status {}",
        authorize_response.status()
    );
    let manifests = authorize_response
        .json::<ArtifactManifestBatchResponse>()
        .await
        .expect("Failed to parse artifact manifest batch response")
        .manifests;

    let app_id = "wasi-grpc-echo:v1";
    let config = build_app_config(app_id, 100_000_000, 100, 2);

    bus.publish(&Event::DeployApp {
        app_id: common::types::AppId(app_id.to_string()),
        config,
        artifact_url,
        artifact_transfer_manifests: manifests,
        expected_hash: Some(sha256),
        size_bytes,
    })
    .await
    .expect("Failed to publish DeployApp event");

    add_route(&bus, "wasi-grpc.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "wasi-grpc.local", 30)
        .await
        .expect("wasi-grpc component did not become ready");

    let channel = Endpoint::from_shared(format!("http://wasi-grpc.local:{}", node.proxy_port))
        .expect("gRPC endpoint should be valid")
        .connect_with_connector(service_fn(move |_: Uri| async move {
            TcpStream::connect(("127.0.0.1", node.proxy_port))
                .await
                .map(TokioIo::new)
        }))
        .await
        .expect("Failed to connect tonic client to proxy");

    let mut client = EchoServiceClient::new(channel);
    let response = client
        .echo(tonic::Request::new(EchoRequest {
            message: "hello over grpc".to_string(),
        }))
        .await
        .expect("gRPC Echo request should succeed")
        .into_inner();

    assert_eq!(response.message, "hello over grpc");
}
