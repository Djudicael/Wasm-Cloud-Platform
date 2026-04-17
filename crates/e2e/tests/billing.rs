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

    let node = NodeProcess::start("test-billing", &nats.url, 18300, 19120)
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
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    let mut config = build_app_config(app_id, 100_000_000, 100, 1);
    config.tenant_id = Some("test-tenant".to_string());

    deploy_app(
        &bus,
        app_id,
        artifact_url,
        sha256,
        size_bytes,
        config,
    )
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

    eprintln!("✓ Request sent, billing record should be created");

    // Wait for billing record to be written
    sleep(Duration::from_secs(2)).await;

    // Extract the database path
    let (db_path, _temp_dir) = node.extract_db();

    // Read billing records from storage
    let store = storage::Store::open(std::path::Path::new(&db_path))
        .expect("Failed to open store");
    let records = store.get_all_billing_records().expect("Failed to read billing records");

    eprintln!("✓ Found {} billing records", records.len());

    // We expect at least one billing record (instance cold start/exit)
    assert!(!records.is_empty(), "Expected at least one billing record");

    // Verify the record has correct structure
    let record = &records[0];
    assert_eq!(record.tenant_id, "test-tenant");
    assert_eq!(record.app_id, "billing-test:v1");
    assert_eq!(record.node_id, "test-billing");

    eprintln!("✓ Billing record has correct structure");

    // Verify hash chain is valid
    let verified = billing::verify_chain(&records);
    assert!(verified.is_ok(), "Hash chain should be valid: {:?}", verified.err());

    eprintln!("✓ Hash chain is valid");
}

/// Test: Hash chain integrity is maintained across multiple records
#[tokio::test]
#[ignore]
async fn test_billing_hash_chain_integrity() {
    let nats = NatsContainer::start(14251)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start("test-billing-chain", &nats.url, 18310, 19130)
        .await
        .expect("Failed to start node");

    sleep(Duration::from_secs(2)).await;

    // Deploy multiple apps to generate multiple billing records
    let wasm_path = find_hello_axum_wasm().expect("hello_axum.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path).unwrap().len();

    upload_artifact(node.artifact_port, &wasm_path, &sha256)
        .await
        .expect("Failed to upload artifact");

    // Deploy first app
    let app_id1 = "chain-test1:v1";
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    let mut config1 = build_app_config(app_id1, 100_000_000, 100, 1);
    config1.tenant_id = Some("chain-tenant".to_string());

    deploy_app(&bus, app_id1, artifact_url.clone(), sha256.clone(), size_bytes, config1)
        .await
        .expect("Failed to deploy app1");

    add_route(&bus, "chain1.local", app_id1)
        .await
        .expect("Failed to add route1");

    // Deploy second app
    let app_id2 = "chain-test2:v1";
    let mut config2 = build_app_config(app_id2, 100_000_000, 100, 1);
    config2.tenant_id = Some("chain-tenant".to_string());

    deploy_app(&bus, app_id2, artifact_url.clone(), sha256.clone(), size_bytes, config2)
        .await
        .expect("Failed to deploy app2");

    add_route(&bus, "chain2.local", app_id2)
        .await
        .expect("Failed to add route2");

    // Send requests to both apps
    wait_for_app_ready(node.proxy_port, "chain1.local", 30)
        .await
        .expect("App1 did not become ready");

    let response1 = send_request(node.proxy_port, "chain1.local", "/")
        .await
        .expect("Failed to send request to app1");
    assert_eq!(response1.status(), 200);

    wait_for_app_ready(node.proxy_port, "chain2.local", 30)
        .await
        .expect("App2 did not become ready");

    let response2 = send_request(node.proxy_port, "chain2.local", "/")
        .await
        .expect("Failed to send request to app2");
    assert_eq!(response2.status(), 200);

    eprintln!("✓ Both apps deployed and requests sent");

    // Wait for billing records to be written
    sleep(Duration::from_secs(3)).await;

    // Extract and read billing records
    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(std::path::Path::new(&db_path))
        .expect("Failed to open store");
    let records = store.get_all_billing_records().expect("Failed to read billing records");

    eprintln!("✓ Found {} billing records", records.len());

    // Verify hash chain integrity
    let verified = billing::verify_chain(&records);
    assert!(verified.is_ok(), "Hash chain should be valid: {:?}", verified.err());

    // Verify chain covers all records
    let count = verified.unwrap();
    assert_eq!(count as usize, records.len(), "All records should be in chain");

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

    let node = NodeProcess::start("test-billing-tamper", &nats.url, 18320, 19140)
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
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    let config = build_app_config(app_id, 100_000_000, 100, 1);

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Wait for billing records
    sleep(Duration::from_secs(3)).await;

    // Extract and read billing records
    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(std::path::Path::new(&db_path))
        .expect("Failed to open store");
    let mut records = store.get_all_billing_records().expect("Failed to read billing records");

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

    eprintln!("✓ Tampering detection verified (changed fuel from {} to 999999999)", original_fuel);
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

    let node = NodeProcess::start("test-billing-report", &nats.url, 18330, 19150)
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
    let artifact_url = format!("http://127.0.0.1:{}/artifacts/{}", node.artifact_port, sha256);

    let mut config = build_app_config(app_id, 100_000_000, 100, 1);
    config.tenant_id = Some("report-tenant".to_string());

    deploy_app(&bus, app_id, artifact_url, sha256, size_bytes, config)
        .await
        .expect("Failed to deploy app");

    // Send requests
    for _ in 0..5 {
        let response = send_request(node.proxy_port, app_id, "/")
            .await
            .expect("Failed to send request");
        assert_eq!(response.status(), 200);
        sleep(Duration::from_millis(100)).await;
    }

    eprintln!("✓ Sent 5 requests");

    // Wait for billing records
    sleep(Duration::from_secs(3)).await;

    // Extract and read billing records
    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(std::path::Path::new(&db_path))
        .expect("Failed to open store");
    let records = store.get_all_billing_records().expect("Failed to read billing records");

    // Generate report
    let report = billing::report::generate_report(
        &records,
        "report-tenant",
        0,
        u64::MAX,
    );

    eprintln!("✓ Generated billing report:");
    eprintln!("   Tenant: {}", report.tenant_id);
    eprintln!("   Total requests: {}", report.total_requests);
    eprintln!("   Total fuel: {}", report.total_fuel_consumed);
    eprintln!("   Per-app: {:?}", report.per_app.len());

    // Verify report structure
    assert_eq!(report.tenant_id, "report-tenant");
    assert!(report.total_requests > 0, "Should have at least one request");
}
