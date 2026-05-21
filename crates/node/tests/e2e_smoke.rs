use common::types::{AppConfig, AppId};
use messaging::events::Event;
use messaging::NatsBus;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use testcontainers::{core::ContainerPort, runners::AsyncRunner, GenericImage, ImageExt};
use tokio::time::sleep;

#[tokio::test]
#[ignore = "full-platform smoke test; requires long-running artifact hosting and acceptance-style deployment flow"]
async fn test_full_platform_e2e() {
    // 1. Build the Wasm app
    println!("Building hello-axum to wasm32-wasi...");
    let mut build_cmd = Command::new("cargo");
    build_cmd
        .env("RUSTFLAGS", "--cfg tokio_unstable")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            "hello-axum",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = build_cmd.status().expect("Failed to build hello-axum");
    assert!(status.success(), "Wasm build failed");

    // Load the wasm bytes
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read compiled wasm");
    let wasm_bytes_clone = wasm_bytes.clone();
    println!("Wasm bytes loaded: {} bytes", wasm_bytes.len());

    // Start a temporary HTTP server to host the wasm file
    let app = axum::Router::new().route(
        "/hello_axum.wasm",
        axum::routing::get(|| async move { wasm_bytes_clone }),
    );
    let artifact_port = 9091; // random unused port
    let artifact_addr = SocketAddr::from(([127, 0, 0, 1], artifact_port));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    // Build the node binary just in case it isn't built
    println!("Building wasm-node binary...");
    Command::new("cargo")
        .args(["build", "--release", "-p", "node"])
        .status()
        .expect("Failed to build node");

    let node_bin = workspace_root.join("target/release/wasm-node");

    // 2. Start NATS via testcontainers
    println!("Starting NATS container...");
    let image = GenericImage::new("nats", "latest")
        .with_mapped_port(4222, ContainerPort::Tcp(4222))
        .with_cmd(vec!["-js"]);

    let _nats_container = image.start().await.expect("Failed to start NATS container");
    sleep(Duration::from_secs(2)).await; // wait for NATS to boot

    let nats_url = "nats://127.0.0.1:4222".to_string();

    // 3. Set up paths and ports for the node
    let db_path = env::temp_dir().join(format!("wasm-node-{}.redb", uuid::Uuid::new_v4()));
    let key_path = env::temp_dir().join(format!("master-{}.key", uuid::Uuid::new_v4()));

    let proxy_port = 8080;
    let admin_port = 9090;
    let node_artifact_port = 19091; // Node's artifact server (different from test's)

    println!("Starting wasm-node process in background...");
    let node_process: Child = Command::new(node_bin)
        .env("RUST_BACKTRACE", "1")
        .arg("--db-path")
        .arg(&db_path)
        .arg("--nats-url")
        .arg(&nats_url)
        .arg("--proxy-port")
        .arg(proxy_port.to_string())
        .arg("--proxy-https-port")
        .arg("0") // Disable HTTPS for tests
        .arg("--admin-port")
        .arg(admin_port.to_string())
        .arg("--artifact-port")
        .arg(node_artifact_port.to_string())
        .arg("--database-url")
        .arg("postgres://localhost:5432/postgres")
        .arg("--key-source")
        .arg("generate")
        .arg("--key-file")
        .arg(&key_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start wasm-node");

    // Wait for the admin API to be up
    println!("Waiting for node to become ready...");
    let mut node_process = node_process;
    let client = Client::new();
    let mut ready = false;
    for _ in 0..15 {
        if let Ok(res) = client
            .get(format!("http://127.0.0.1:{}/health", admin_port))
            .send()
            .await
        {
            if res.status().is_success() {
                ready = true;
                break;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    if !ready {
        let _ = node_process.kill().ok();
        let output = node_process
            .wait_with_output()
            .expect("Failed to read node output");
        println!("--- Node STDOUT ---");
        println!("{}", String::from_utf8_lossy(&output.stdout));
        println!("--- Node STDERR ---");
        println!("{}", String::from_utf8_lossy(&output.stderr));
        panic!("wasm-node did not become ready in time");
    }

    // 4. Send Event::DeployApp via NATS
    println!("Connecting to NATS to deploy app...");
    let bus = NatsBus::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup jetstream");

    let app_id = AppId("hello-axum".to_string());

    // Hash for security step
    let mut hasher = Sha256::new();
    hasher.update(&wasm_bytes);
    let expected_hash = hex::encode(hasher.finalize());

    let deploy_event = Event::DeployApp {
        app_id: app_id.clone(),
        config: AppConfig::default_for(app_id.clone()),
        artifact_url: format!("http://127.0.0.1:{}/hello_axum.wasm", artifact_port),
        artifact_transfer_manifests: vec![],
        expected_hash: Some(expected_hash),
        size_bytes: wasm_bytes.len() as u64,
    };

    println!("Publishing deploy event...");
    bus.publish(&deploy_event)
        .await
        .expect("Failed to publish deploy event");
    println!(
        "Deploy event published to subject: {}",
        deploy_event.subject()
    );

    let route_event = Event::RouteAdd {
        route: common::types::Route {
            host: "hello-axum".to_string(),
            app_id: app_id.clone(),
            path_prefix: "/".to_string(),
            strip_prefix: false,
            created_at: 0,
            updated_at: 0,
        },
    };
    println!("Publishing route add event...");
    bus.publish(&route_event)
        .await
        .expect("Failed to publish route event");
    println!(
        "Route add event published to subject: {}",
        route_event.subject()
    );

    // Give it a few seconds to compile Cranelift AOT and spin up
    println!("Waiting for app to be deployed and ready...");
    sleep(Duration::from_secs(5)).await;

    // 5. Reqwest -> http://127.0.0.1:8080 with Host header
    println!("Sending HTTP request to proxy...");
    let mut success = false;
    for _ in 0..5 {
        let res = client
            .get(format!("http://127.0.0.1:{}/", proxy_port))
            .header("Host", "hello-axum")
            .send()
            .await;

        if let Ok(response) = res {
            if response.status().is_success() {
                println!("Got successful response!");
                // Read body
                let body = response.text().await.unwrap_or_default();
                println!("Response body: {}", body);
                success = true;
                break;
            } else {
                println!("Request failed with status: {}", response.status());
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    // 6. Assert response and kill process
    println!("Cleaning up...");
    let _ = node_process.kill();
    let output = node_process
        .wait_with_output()
        .expect("Failed to read node output");

    println!("--- Node STDOUT ---");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    println!("--- Node STDERR ---");
    println!("{}", String::from_utf8_lossy(&output.stderr));

    // Clean up temporary files
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&key_path);

    assert!(
        success,
        "Failed to get a 200 OK from the deployed app through Pingora proxy!"
    );
}
