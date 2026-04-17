/// Billing E2E Tests
///
/// Tests the billing system's ability to:
/// - Record fuel consumption per instance execution
/// - Maintain hash chain integrity
/// - Support billing reports and exports
mod harness;

use harness::*;
use std::time::Duration;
use tokio::time::sleep;

/// Test: Billing records are created and stored correctly
#[tokio::test]
#[ignore]
async fn test_billing_records_created_on_instance_exit() {
    let nats = NatsContainer::start(14250)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    // Use unique admin port for this test
    let node = NodeProcess::start_with_admin("test-billing", &nats.url, 18300, 19120, 19201)
        .await
        .expect("Failed to start node");

    sleep(Duration::from_secs(2)).await;

    // Deploy app with tenant_id for billing attribution
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "billing-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    let mut config = build_app_config(app_id, 100_000_000, 100, 1);
    config.tenant_id = Some("test-tenant".to_string());

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Add route
    add_route(&bus, "billing.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "billing.local", 30)
        .await
        .expect("App did not become ready");

    eprintln!("✓ App deployed and ready");

    // Send a request to trigger instance activity
    let response = send_request(node.proxy_port, "billing.local", "/")
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 200);

    eprintln!("✓ Request sent to trigger instance spawn");

    // Remove app to trigger instance shutdown and billing record creation
    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    // Wait for shutdown and billing record to be written
    sleep(Duration::from_secs(5)).await;
    eprintln!("✓ App removed, instances shutdown, billing records created");

    // Extract the database path
    let (db_path, _temp_dir) = node.extract_db();

    // Read billing records from storage
    let store = storage::Store::open(std::path::Path::new(&db_path)).expect("Failed to open store");
    let records = store
        .get_all_billing_records()
        .expect("Failed to read billing records");

    eprintln!("✓ Found {} billing records", records.len());

    // We expect at least 1 billing record (one per app removal)
    assert!(
        !records.is_empty(),
        "Expected at least 1 billing record, found {}",
        records.len()
    );

    // Verify hash chain integrity
    let verified = billing::verify_chain(&records);
    assert!(
        verified.is_ok(),
        "Hash chain should be valid: {:?}",
        verified.err()
    );

    // Verify chain covers all records
    let count = verified.unwrap();
    assert_eq!(
        count as usize,
        records.len(),
        "All records should be in chain"
    );

    eprintln!("✓ Hash chain integrity verified for {} records", count);
}

/// Test: Tampering with a billing record is detected
#[tokio::test]
#[ignore]
async fn test_billing_tampering_detected() {
    let nats = NatsContainer::start(14252)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start_with_admin("test-billing-tamper", &nats.url, 18320, 19140, 19202)
        .await
        .expect("Failed to start node");

    sleep(Duration::from_secs(2)).await;

    // Deploy an app
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "tamper-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    let config = build_app_config(app_id, 100_000_000, 100, 1);

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Add route
    add_route(&bus, "tamper.local", app_id)
        .await
        .expect("Failed to add route");

    // Wait for app to be ready
    wait_for_app_ready(node.proxy_port, "tamper.local", 30)
        .await
        .expect("App did not become ready");

    // Send a request to trigger instance activity
    let response = send_request(node.proxy_port, "tamper.local", "/")
        .await
        .expect("Failed to send request");
    assert_eq!(response.status(), 200);

    // Remove app to trigger instance shutdown and billing record creation
    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    // Wait for shutdown and billing record to be written
    sleep(Duration::from_secs(5)).await;
    eprintln!("✓ App removed, instances shutdown, billing records created");

    // Extract and read billing records
    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(std::path::Path::new(&db_path)).expect("Failed to open store");
    let mut records = store
        .get_all_billing_records()
        .expect("Failed to read billing records");

    assert!(!records.is_empty(), "Should have at least one record");

    // Tamper with a record (change fuel_consumed)
    let original_fuel = records[0].fuel_consumed;
    records[0].fuel_consumed = 999999999;

    // Verify tampering is detected
    let result = billing::verify_chain(&records);
    assert!(result.is_err(), "Tampering should be detected");

    match result {
        Err(billing::ChainError::TamperedRecord { seq, .. }) => {
            assert_eq!(seq, records[0].seq);
            eprintln!("✓ Tampering detected at record {}", seq);
        }
        _ => panic!("Expected TamperedRecord error"),
    }

    eprintln!(
        "✓ Tampering detection verified (changed fuel from {} to 999999999)",
        original_fuel
    );
}

/// Test: Billing report generation
#[tokio::test]
#[ignore]
async fn test_billing_report_generation() {
    let nats = NatsContainer::start(14253)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start_with_admin("test-billing-report", &nats.url, 18330, 19150, 19203)
        .await
        .expect("Failed to start node");

    sleep(Duration::from_secs(2)).await;

    // Deploy app with tenant
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    let app_id = "report-test:v1";
    let artifact_url = format!(
        "http://127.0.0.1:{}/artifacts/{}",
        node.artifact_port, sha256
    );

    let mut config = build_app_config(app_id, 100_000_000, 100, 1);
    config.tenant_id = Some("report-tenant".to_string());

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Add route
    add_route(&bus, "report.local", app_id)
        .await
        .expect("Failed to add route");

    // Wait for app to be ready
    wait_for_app_ready(node.proxy_port, "report.local", 30)
        .await
        .expect("App did not become ready");

    // Send requests
    for _ in 0..5 {
        let response = send_request(node.proxy_port, "report.local", "/")
            .await
            .expect("Failed to send request");
        assert_eq!(response.status(), 200);
        sleep(Duration::from_millis(100)).await;
    }

    eprintln!("✓ Sent 5 requests");

    // Remove app to trigger instance shutdown and billing record creation
    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    // Wait for shutdown and billing record to be written
    sleep(Duration::from_secs(5)).await;
    eprintln!("✓ App removed, instances shutdown, billing records created");

    // Extract and read billing records
    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(std::path::Path::new(&db_path)).expect("Failed to open store");
    let records = store
        .get_all_billing_records()
        .expect("Failed to read billing records");

    // Generate report
    let report = billing::report::generate_report(&records, "report-tenant", 0, u64::MAX);

    eprintln!("✓ Generated billing report:");
    eprintln!("   Tenant: {}", report.tenant_id);
    eprintln!("   Total requests: {}", report.total_requests);
    eprintln!("   Total fuel: {}", report.total_fuel_consumed);
    eprintln!("   Per-app: {:?}", report.per_app.len());

    // Verify report structure
    assert_eq!(report.tenant_id, "report-tenant");
    assert!(
        report.total_requests > 0,
        "Should have at least one request"
    );
}
