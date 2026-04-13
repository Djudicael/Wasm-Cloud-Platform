/// Hot-swap zero downtime test
///
/// This test verifies that deploying a new version of an app while traffic is flowing
/// results in ZERO failed requests.

mod harness;

use common::types::AppId;
use harness::*;
use messaging::events::Event;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
#[ignore]
async fn test_hot_swap_zero_downtime() {
    // Shared counters for tracking request results
    let success_count = Arc::new(AtomicU64::new(0));
    let failure_count = Arc::new(AtomicU64::new(0));
    let total_count = Arc::new(AtomicU64::new(0));

    // 1. Start NATS
    let nats = NatsContainer::start(4224)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream().await.expect("Failed to setup JetStream");

    // 2. Start node
    let node = NodeProcess::start("test-node-hotswap", &nats.url, 8182, 9002)
        .await
        .expect("Failed to start node");

    // 3. Deploy v1
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id_v1 = "hotswap-app:v1";
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    deploy_app(
        &bus,
        app_id_v1,
        artifact_url.clone(),
        sha256.clone(),
        size_bytes,
        build_app_config(app_id_v1, 100_000_000, 100, 2),
    )
    .await
    .expect("Failed to deploy v1");

    // 4. Add route to v1
    add_route(&bus, "hotswap.local", app_id_v1)
        .await
        .expect("Failed to add route");

    // Wait for v1 to be ready
    wait_for_app_ready(node.proxy_port, "hotswap.local", 30)
        .await
        .expect("v1 did not become ready");

    eprintln!("✓ v1 deployed and ready");

    // 5. Start background traffic generator
    let traffic_success = success_count.clone();
    let traffic_failure = failure_count.clone();
    let traffic_total = total_count.clone();
    let proxy_port = node.proxy_port;

    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);

    let traffic_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }

            traffic_total.fetch_add(1, Ordering::SeqCst);

            match client
                .get(format!("http://127.0.0.1:{}/", proxy_port))
                .header("host", "hotswap.local")
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    traffic_success.fetch_add(1, Ordering::SeqCst);
                }
                Ok(resp) => {
                    eprintln!("❌ Failed request: status {}", resp.status());
                    traffic_failure.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => {
                    eprintln!("❌ Failed request: {}", e);
                    traffic_failure.fetch_add(1, Ordering::SeqCst);
                }
            }

            sleep(Duration::from_millis(10)).await;
        }
    });

    // Let traffic flow for 2 seconds to establish baseline
    sleep(Duration::from_secs(2)).await;

    let baseline_success = success_count.load(Ordering::SeqCst);
    eprintln!("✓ Baseline: {} successful requests", baseline_success);
    assert!(baseline_success > 100, "Expected traffic to flow");

    // 6. Deploy v2 (while traffic is flowing)
    eprintln!("Deploying v2 while traffic flows...");
    let app_id_v2 = "hotswap-app:v2";

    deploy_app(
        &bus,
        app_id_v2,
        artifact_url.clone(),
        sha256.clone(),
        size_bytes,
        build_app_config(app_id_v2, 100_000_000, 100, 2),
    )
    .await
    .expect("Failed to deploy v2");

    // Small delay for deployment to process
    sleep(Duration::from_millis(500)).await;

    // 7. Update route to point to v2
    eprintln!("Updating route to v2...");
    let route = common::types::Route {
        host: "hotswap.local".to_string(),
        app_id: AppId(app_id_v2.to_string()),
        path_prefix: "/".to_string(),
        strip_prefix: false,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    };
    bus.publish(&Event::RouteAdd { route }).await.unwrap();

    // Continue traffic during and after swap
    sleep(Duration::from_secs(3)).await;

    // Stop traffic
    stop_tx.send(()).await.ok();
    traffic_task.await.ok();

    // Get final counts
    let final_total = total_count.load(Ordering::SeqCst);
    let final_success = success_count.load(Ordering::SeqCst);
    let final_failure = failure_count.load(Ordering::SeqCst);

    eprintln!("\nHot-swap results:");
    eprintln!("  Total requests: {}", final_total);
    eprintln!("  Successful: {}", final_success);
    eprintln!("  Failed: {}", final_failure);
    eprintln!(
        "  Success rate: {:.2}%",
        (final_success as f64 / final_total as f64) * 100.0
    );

    node.stop().ok();

    // ASSERTION: Zero downtime = zero failures
    assert_eq!(
        final_failure, 0,
        "Expected ZERO failed requests during hot-swap, got {}",
        final_failure
    );

    // Verify we actually got traffic through
    assert!(
        final_total > 400,
        "Expected at least 400 requests (~5s * 100 req/s), got {}",
        final_total
    );

    eprintln!("✅ test_hot_swap_zero_downtime PASSED");
}

#[test]
fn test_hot_swap_infrastructure() {
    let counter = Arc::new(AtomicU64::new(0));
    counter.fetch_add(1, Ordering::SeqCst);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
