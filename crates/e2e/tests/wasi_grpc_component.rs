mod harness;

use std::time::{Duration, Instant};

use common::artifact_transfer::{ArtifactManifestBatchRequest, ArtifactManifestBatchResponse};
use harness::*;
use hyper_util::rt::TokioIo;
use messaging::events::Event;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_stream::iter;
use tonic::codegen::http::Uri;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

pub mod pb {
    tonic::include_proto!("echo");
}

use pb::echo_service_client::EchoServiceClient;
use pb::{EchoReply, EchoRequest};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_ITEM_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_END_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_INSTANCE_PORT: u16 = 10_000;

struct DeployedGrpcApp {
    _nats: NatsContainer,
    _node: NodeProcess,
    proxy_client: EchoServiceClient<Channel>,
}

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_unary_and_server_streaming() {
    let mut app = deploy_grpc_client(
        "wasi-grpc-node-unary-stream",
        4422,
        8380,
        9201,
        9190,
        "wasi-grpc-unary-stream.local",
    )
    .await;

    let started = Instant::now();
    let unary = with_timeout(
        "unary echo",
        RPC_TIMEOUT,
        app.proxy_client.echo(tonic::Request::new(EchoRequest {
            message: "hello over grpc".to_string(),
        })),
    )
    .await
    .expect("unary gRPC Echo request should succeed")
    .into_inner();
    eprintln!("unary_rpc_ms={}", started.elapsed().as_millis());
    assert_eq!(unary.message, "hello over grpc");

    let started = Instant::now();
    let server_stream = with_timeout(
        "server_stream setup",
        RPC_TIMEOUT,
        app.proxy_client
            .server_stream(tonic::Request::new(EchoRequest {
                message: "stream".to_string(),
            })),
    )
    .await
    .expect("server streaming request should succeed")
    .into_inner();
    let server_messages = collect_stream_messages("server_stream", server_stream)
        .await
        .expect("server stream should complete successfully");
    eprintln!("server_stream_ms={}", started.elapsed().as_millis());
    assert_eq!(
        server_messages,
        vec![
            "stream:1".to_string(),
            "stream:2".to_string(),
            "stream:3".to_string(),
        ]
    );
}

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_client_streaming() {
    let mut app = deploy_grpc_client(
        "wasi-grpc-node-client-stream",
        4423,
        8381,
        9202,
        9191,
        "wasi-grpc-client-stream.local",
    )
    .await;

    let started = Instant::now();
    let client_stream = with_timeout(
        "client_stream",
        RPC_TIMEOUT,
        app.proxy_client
            .client_stream(tonic::Request::new(iter(vec![
                EchoRequest {
                    message: "alpha".to_string(),
                },
                EchoRequest {
                    message: "beta".to_string(),
                },
                EchoRequest {
                    message: "gamma".to_string(),
                },
            ]))),
    )
    .await
    .expect("client streaming request should succeed")
    .into_inner();
    eprintln!("client_stream_ms={}", started.elapsed().as_millis());
    assert_eq!(client_stream.message, "count=3;messages=alpha|beta|gamma");
}

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_bidi_streaming() {
    let mut app = deploy_grpc_client(
        "wasi-grpc-node-bidi-stream",
        4424,
        8382,
        9203,
        9192,
        "wasi-grpc-bidi-stream.local",
    )
    .await;

    let started = Instant::now();
    let bidi_stream = with_timeout(
        "bidi_stream setup",
        RPC_TIMEOUT,
        app.proxy_client.bidi_stream(tonic::Request::new(iter(vec![
            EchoRequest {
                message: "first".to_string(),
            },
            EchoRequest {
                message: "second".to_string(),
            },
            EchoRequest {
                message: "third".to_string(),
            },
        ]))),
    )
    .await
    .expect("bidi streaming request should succeed")
    .into_inner();
    let bidi_messages = collect_stream_messages("bidi_stream", bidi_stream)
        .await
        .expect("bidi stream should complete successfully");
    eprintln!("bidi_stream_ms={}", started.elapsed().as_millis());
    assert_eq!(
        bidi_messages,
        vec![
            "1:first".to_string(),
            "2:second".to_string(),
            "3:third".to_string(),
        ]
    );
}

#[tokio::test]
#[ignore = "requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_failure_trailers() {
    let mut app = deploy_grpc_client(
        "wasi-grpc-node-failures",
        4425,
        8383,
        9204,
        9193,
        "wasi-grpc-failures.local",
    )
    .await;

    let unary_error = with_timeout(
        "fail_unary",
        RPC_TIMEOUT,
        app.proxy_client
            .fail_unary(tonic::Request::new(EchoRequest {
                message: "boom".to_string(),
            })),
    )
    .await
    .expect_err("fail_unary should return grpc error");
    assert_eq!(unary_error.code(), tonic::Code::PermissionDenied);
    assert_eq!(unary_error.message(), "forced unary failure");

    let mut failed_stream = with_timeout(
        "fail_server_stream setup",
        RPC_TIMEOUT,
        app.proxy_client
            .fail_server_stream(tonic::Request::new(EchoRequest {
                message: "boom".to_string(),
            })),
    )
    .await
    .expect("fail_server_stream call should establish stream")
    .into_inner();

    let first = with_timeout(
        "fail_server_stream first item",
        STREAM_ITEM_TIMEOUT,
        failed_stream.message(),
    )
    .await
    .expect("first failed-stream poll should not transport-fail")
    .expect("first failed-stream item should exist");
    assert_eq!(first.message, "partial:boom");

    let stream_error = with_timeout(
        "fail_server_stream terminal trailer",
        STREAM_END_TIMEOUT,
        failed_stream.message(),
    )
    .await
    .expect_err("second failed-stream poll should return grpc trailer error");
    assert_eq!(stream_error.code(), tonic::Code::Internal);
    assert_eq!(stream_error.message(), "forced stream failure");
}

async fn deploy_grpc_client(
    node_id: &str,
    nats_port: u16,
    proxy_port: u16,
    artifact_port: u16,
    admin_port: u16,
    host: &str,
) -> DeployedGrpcApp {
    let started = Instant::now();
    let nats = with_timeout(
        "start NATS",
        STARTUP_TIMEOUT,
        NatsContainer::start(nats_port),
    )
    .await
    .expect("Failed to start NATS");
    let bus = with_timeout("connect NATS", STARTUP_TIMEOUT, nats.connect())
        .await
        .expect("Failed to connect to NATS");
    with_timeout("setup JetStream", STARTUP_TIMEOUT, bus.setup_jetstream())
        .await
        .expect("Failed to setup JetStream");
    eprintln!("nats_setup_ms={}", started.elapsed().as_millis());

    let started = Instant::now();
    let node = with_timeout(
        "start node",
        STARTUP_TIMEOUT,
        NodeProcess::start_with_admin(node_id, &nats.url, proxy_port, artifact_port, admin_port),
    )
    .await
    .expect("Failed to start node");
    eprintln!("node_start_ms={}", started.elapsed().as_millis());

    let started = Instant::now();
    let wasm_path = find_wasi_grpc_echo_wasm().expect("wasi-grpc-echo.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path)
        .expect("Failed to get file size")
        .len();
    with_timeout(
        "upload artifact",
        DEPLOY_TIMEOUT,
        upload_artifact(node.artifact_port, &wasm_path, &sha256),
    )
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
    let authorize_response = with_timeout(
        "authorize manifests",
        DEPLOY_TIMEOUT,
        reqwest::Client::new()
            .post(&authorize_url)
            .json(&ArtifactManifestBatchRequest {
                audiences: vec![node.node_id.clone()],
            })
            .send(),
    )
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

    with_timeout(
        "publish DeployApp",
        DEPLOY_TIMEOUT,
        bus.publish(&Event::DeployApp {
            app_id: common::types::AppId(app_id.to_string()),
            config,
            artifact_url,
            artifact_transfer_manifests: manifests,
            expected_hash: Some(sha256),
            size_bytes,
        }),
    )
    .await
    .expect("Failed to publish DeployApp event");

    with_timeout("add route", DEPLOY_TIMEOUT, add_route(&bus, host, app_id))
        .await
        .expect("Failed to add route");

    with_timeout(
        "wait for app ready",
        DEPLOY_TIMEOUT,
        wait_for_app_ready(node.proxy_port, host, 30),
    )
    .await
    .expect("wasi-grpc component did not become ready");
    eprintln!("deploy_ready_ms={}", started.elapsed().as_millis());

    let started = Instant::now();
    let channel = with_timeout(
        "connect tonic client",
        CONNECT_TIMEOUT,
        Endpoint::from_shared(format!("http://{host}:{}", node.proxy_port))
            .expect("gRPC endpoint should be valid")
            .connect_with_connector(service_fn(move |_: Uri| async move {
                TcpStream::connect(("127.0.0.1", node.proxy_port))
                    .await
                    .map(TokioIo::new)
            })),
    )
    .await
    .expect("Failed to connect tonic client to proxy");
    eprintln!("grpc_connect_ms={}", started.elapsed().as_millis());

    DeployedGrpcApp {
        _nats: nats,
        _node: node,
        proxy_client: EchoServiceClient::new(channel),
    }
}

async fn connect_direct_instance(instance_port: u16) -> EchoServiceClient<Channel> {
    let channel = with_timeout(
        "connect direct tonic client",
        CONNECT_TIMEOUT,
        Endpoint::from_shared(format!("http://127.0.0.1:{instance_port}"))
            .expect("direct gRPC endpoint should be valid")
            .connect_with_connector(service_fn(move |_: Uri| async move {
                TcpStream::connect(("127.0.0.1", instance_port))
                    .await
                    .map(TokioIo::new)
            })),
    )
    .await
    .expect("Failed to connect tonic client to direct instance");
    EchoServiceClient::new(channel)
}

#[tokio::test]
#[ignore = "diagnostic: requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_direct_instance_server_streaming() {
    let _app = deploy_grpc_client(
        "wasi-grpc-node-direct-server-stream",
        4426,
        8384,
        9205,
        9194,
        "wasi-grpc-direct-server-stream.local",
    )
    .await;
    let mut direct = connect_direct_instance(FIRST_INSTANCE_PORT).await;
    let stream = with_timeout(
        "direct server_stream setup",
        RPC_TIMEOUT,
        direct.server_stream(tonic::Request::new(EchoRequest {
            message: "stream".to_string(),
        })),
    )
    .await
    .expect("direct server streaming request should succeed")
    .into_inner();
    let messages = collect_stream_messages("direct_server_stream", stream)
        .await
        .expect("direct server stream should complete successfully");
    assert_eq!(
        messages,
        vec![
            "stream:1".to_string(),
            "stream:2".to_string(),
            "stream:3".to_string(),
        ]
    );
}

#[tokio::test]
#[ignore = "diagnostic: requires NATS + wasm-node binary + compiled wasi-grpc-echo.wasm"]
async fn test_wasi_grpc_direct_instance_bidi_streaming() {
    let _app = deploy_grpc_client(
        "wasi-grpc-node-direct-bidi-stream",
        4427,
        8385,
        9206,
        9195,
        "wasi-grpc-direct-bidi-stream.local",
    )
    .await;
    let mut direct = connect_direct_instance(FIRST_INSTANCE_PORT).await;
    let stream = with_timeout(
        "direct bidi_stream setup",
        RPC_TIMEOUT,
        direct.bidi_stream(tonic::Request::new(iter(vec![
            EchoRequest {
                message: "first".to_string(),
            },
            EchoRequest {
                message: "second".to_string(),
            },
            EchoRequest {
                message: "third".to_string(),
            },
        ]))),
    )
    .await
    .expect("direct bidi streaming request should succeed")
    .into_inner();
    let messages = collect_stream_messages("direct_bidi_stream", stream)
        .await
        .expect("direct bidi stream should complete successfully");
    assert_eq!(
        messages,
        vec![
            "1:first".to_string(),
            "2:second".to_string(),
            "3:third".to_string(),
        ]
    );
}

async fn collect_stream_messages(
    label: &str,
    mut stream: tonic::Streaming<EchoReply>,
) -> Result<Vec<String>, tonic::Status> {
    let started = Instant::now();
    let mut messages = Vec::new();
    loop {
        let next = with_timeout(label, STREAM_ITEM_TIMEOUT, stream.message()).await?;
        match next {
            Some(message) => messages.push(message.message),
            None => {
                eprintln!("{label}_complete_ms={}", started.elapsed().as_millis());
                return Ok(messages);
            }
        }
    }
}

async fn with_timeout<T, F>(label: &str, duration: Duration, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    timeout(duration, fut)
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded timeout of {:?}", duration))
}
