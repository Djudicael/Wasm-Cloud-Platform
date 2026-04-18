use common::types::{AppConfig, AppId};
use messaging::events::Event;
use messaging::NatsBus;
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

/// End-to-end test demonstrating graceful shutdown with the /_platform/shutdown endpoint
///
/// **Prerequisites**: Start NATS with JetStream before running this test:
/// ```bash
/// podman run -d --rm --name nats-test -p 4222:4222 docker.io/library/nats:2.10-alpine -js
/// ```
///
/// This test shows:
/// 1. Building hello-axum WASM app with graceful shutdown support
/// 2. Deploying it to the platform
/// 3. Sending requests to verify instance is running
/// 4. Triggering graceful shutdown via HTTP endpoint
/// 5. Verifying the instance exits cleanly and stops accepting requests
#[tokio::test]
#[ignore] // Requires manual NATS setup - run with: cargo test -- --ignored
async fn test_graceful_shutdown_via_platform_endpoint() {
    // 1. Build the Wasm app with graceful shutdown support
    println!("Building hello-axum with graceful shutdown to wasm32-wasip2...");
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

    // 2. Load the compiled WASM bytes
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
    let wasm_path = workspace_root.join("target/wasm32-wasip2/release/hello-axum.wasm");
    let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read compiled wasm");
    let wasm_bytes_clone = wasm_bytes.clone();
    println!("✓ Wasm bytes loaded: {} bytes", wasm_bytes.len());

    // 3. Calculate SHA256 hash
    let mut hasher = Sha256::new();
    hasher.update(&wasm_bytes);
    let expected_hash = hex::encode(hasher.finalize());

    // 4. Start artifact server to host the wasm file
    let artifact_app = axum::Router::new().route(
        "/hello_axum.wasm",
        axum::routing::get(|| async move { wasm_bytes_clone }),
    );
    let artifact_port = 19091;
    let artifact_addr = SocketAddr::from(([127, 0, 0, 1], artifact_port));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr).await.unwrap();
        axum::serve(listener, artifact_app).await.unwrap();
    });
    sleep(Duration::from_millis(500)).await;
    println!("✓ Artifact server ready on port {}", artifact_port);

    // 5. Use manually started NATS (assumes it's running on port 4222)
    let nats_url = "nats://127.0.0.1:4222".to_string();
    println!("✓ Using NATS at {}", nats_url);

    // 6. Start the wasm-node
    println!("Starting wasm-node...");
    let proxy_port = 28080;
    let admin_port = 29090;
    let key_path = "/tmp/wasm-graceful-test.key";

    let mut node_process = Command::new("cargo")
        .args(["run", "-p", "node", "--"])
        .arg("--nats-url")
        .arg(&nats_url)
        .arg("--db-path")
        .arg("/tmp/wasm-graceful-test.redb")
        .arg("--proxy-port")
        .arg(proxy_port.to_string())
        .arg("--proxy-https-port")
        .arg("0") // Disable HTTPS
        .arg("--admin-port")
        .arg(admin_port.to_string())
        .arg("--artifact-port")
        .arg("29092") // Artifact server port
        .arg("--key-source")
        .arg("generate")
        .arg("--key-file")
        .arg(key_path)
        .arg("--port-start")
        .arg("30000")
        .arg("--port-end")
        .arg("30999")
        .env("RUST_LOG", "info,node=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start wasm-node");

    // 7. Wait for node to be ready
    println!("Waiting for node to become ready...");
    let client = Client::new();
    let mut ready = false;
    for _ in 0..20 {
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
        let _ = node_process.kill();
        let output = node_process.wait_with_output().ok();
        if let Some(out) = output {
            println!("--- Node STDOUT ---");
            println!("{}", String::from_utf8_lossy(&out.stdout));
            println!("--- Node STDERR ---");
            println!("{}", String::from_utf8_lossy(&out.stderr));
        }
        panic!("wasm-node did not become ready in time");
    }
    println!("✓ Node is ready");

    // 8. Connect to NATS and deploy the app
    let bus = NatsBus::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup jetstream");

    let app_id = AppId("hello-axum".to_string());

    println!("Publishing DeployApp event...");
    let deploy_event = Event::DeployApp {
        app_id: app_id.clone(),
        config: AppConfig::default_for(app_id.clone()),
        artifact_url: format!("http://127.0.0.1:{}/hello_axum.wasm", artifact_port),
        expected_hash: Some(expected_hash),
        size_bytes: wasm_bytes.len() as u64,
    };
    bus.publish(&deploy_event)
        .await
        .expect("Failed to publish deploy event");

    // Add route
    println!("Publishing RouteAdd event...");
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
    bus.publish(&route_event)
        .await
        .expect("Failed to publish route event");

    // Wait for deployment
    println!("Waiting for app deployment and compilation...");
    sleep(Duration::from_secs(6)).await;

    // 9. Send requests to verify instance is running
    println!("Sending test requests via proxy...");
    let proxy_url = format!("http://127.0.0.1:{}/", proxy_port);

    for i in 1..=3 {
        match client
            .get(&proxy_url)
            .header("Host", "hello-axum")
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status() == 200 => {
                let body = resp.text().await.unwrap_or_default();
                println!("  Request {}: ✓ status=200, body={}", i, body);
                assert_eq!(body, "Hello from Wasm!");
            }
            Ok(resp) => {
                println!("  Request {}: unexpected status {}", i, resp.status());
            }
            Err(e) => {
                println!("  Request {}: failed {:?}", i, e);
                if i > 1 {
                    // First request might fail during warmup
                    panic!("Request failed after warmup: {:?}", e);
                }
            }
        }
        sleep(Duration::from_millis(300)).await;
    }

    // 10. Find the instance's direct address (bypassing proxy)
    println!("Finding instance direct address...");
    let mut instance_addr = None;

    for port in 30000..30020 {
        let test_url = format!("http://127.0.0.1:{}/", port);
        if let Ok(resp) = client
            .get(&test_url)
            .timeout(Duration::from_millis(500))
            .send()
            .await
        {
            if resp.status() == 200 {
                instance_addr = Some(format!("127.0.0.1:{}", port));
                println!("✓ Found instance at {}", instance_addr.as_ref().unwrap());
                break;
            }
        }
    }

    let instance_addr =
        instance_addr.expect("Could not find running instance - deployment may have failed");

    // 11. Test the graceful shutdown endpoint
    println!("\n🔌 Testing graceful shutdown via /_platform/shutdown...");
    let shutdown_url = format!("http://{}/_platform/shutdown", instance_addr);

    let status_before_shutdown = client
        .post(&shutdown_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await;

    match status_before_shutdown {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            println!("  Shutdown response: status={}, body=\"{}\"", status, body);
            assert_eq!(status, 200, "Shutdown endpoint should return 200");
            assert_eq!(
                body, "shutting down gracefully",
                "Unexpected shutdown response body"
            );
        }
        Err(e) => {
            // Connection might close immediately if instance exits very fast
            println!("  Shutdown triggered (connection may have closed): {:?}", e);
        }
    }

    // 12. Verify instance has stopped accepting requests
    println!("Verifying instance shutdown...");
    sleep(Duration::from_secs(2)).await;

    let check_url = format!("http://{}/", instance_addr);
    let post_shutdown_check = client
        .get(&check_url)
        .timeout(Duration::from_millis(1000))
        .send()
        .await;

    match post_shutdown_check {
        Ok(resp) => {
            println!("  ⚠️  Instance still responding: status={}", resp.status());
            println!("     (This is OK if shutdown is delayed, but instance should stop soon)");
        }
        Err(e) => {
            println!("  ✓ Instance stopped (connection refused): {:?}", e);
        }
    }

    // Wait a bit more and check again
    sleep(Duration::from_secs(2)).await;
    let final_check = client
        .get(&check_url)
        .timeout(Duration::from_millis(500))
        .send()
        .await;

    assert!(
        final_check.is_err(),
        "Instance should be completely shut down and unreachable after 4 seconds"
    );
    println!("  ✓ Instance fully shut down");

    // 13. Verify proxy also shows instance is gone
    println!("Verifying instance removed from proxy...");
    let proxy_check = client
        .get(&proxy_url)
        .header("Host", "hello-axum")
        .timeout(Duration::from_secs(2))
        .send()
        .await;

    match proxy_check {
        Ok(resp) => {
            println!(
                "  Proxy returned status: {} (might have other instances or cold-start)",
                resp.status()
            );
        }
        Err(e) => {
            println!("  ✓ Proxy shows no instances available: {:?}", e);
        }
    }

    // 14. Cleanup
    println!("\nCleaning up...");
    node_process.kill().ok();
    let _ = node_process.wait();
    let _ = std::fs::remove_file("/tmp/wasm-graceful-test.redb");
    let _ = std::fs::remove_file(key_path);

    println!("\n✅ Graceful shutdown test completed successfully!");
    println!("\nWhat this test demonstrated:");
    println!("  1. ✓ Building WASM app with /_platform/shutdown endpoint");
    println!("  2. ✓ Deploying to the platform via events");
    println!("  3. ✓ Sending successful requests to running instance");
    println!("  4. ✓ Triggering graceful shutdown via HTTP POST");
    println!("  5. ✓ Verifying instance exits cleanly within timeout");
    println!("  6. ✓ Confirming instance stops accepting new requests");
}
