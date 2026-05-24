/// Billing E2E Tests
///
/// Tests the billing system's ability to:
/// - Record fuel consumption per instance execution
/// - Maintain hash chain integrity
/// - Support billing reports and exports
mod harness;

use harness::*;
use std::path::Path;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

static BILLING_E2E_LOCK: Mutex<()> = Mutex::const_new(());

async fn collect_billing_records_after_shutdown(
    node: NodeProcess,
    min_records: usize,
    wait_before_extract: Duration,
) -> Result<Vec<common::billing::BillingRecord>, String> {
    sleep(wait_before_extract).await;

    let (db_path, _temp_dir) = node.extract_db();
    let store = storage::Store::open(Path::new(&db_path)).map_err(|e| e.to_string())?;
    let records = store.get_all_billing_records().map_err(|e| e.to_string())?;

    if records.len() < min_records {
        return Err(format!(
            "expected at least {min_records} billing record(s) after shutdown; found {}",
            records.len()
        ));
    }

    Ok(records)
}

/// Test: Billing records are created and stored correctly
#[tokio::test]
async fn test_billing_records_created_on_instance_exit() {
    let _guard = BILLING_E2E_LOCK.lock().await;

    let nats = NatsContainer::start(14250)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start_with_admin("test-billing", &nats.url, 18300, 19120, 19201)
        .await
        .expect("Failed to start node");

    sleep(Duration::from_secs(2)).await;

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

    add_route(&bus, "billing.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "billing.local", 30)
        .await
        .expect("App did not become ready");

    eprintln!("App deployed and ready");

    eprintln!("Readiness probe triggered the first billed request");

    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    let records = collect_billing_records_after_shutdown(node, 1, Duration::from_secs(12))
        .await
        .expect("billing records were not persisted in time");
    eprintln!("Billing records persisted after app removal");

    eprintln!("Found {} billing records", records.len());

    assert!(
        !records.is_empty(),
        "Expected at least 1 billing record, found {}",
        records.len()
    );

    let verified = billing::verify_chain(&records);
    assert!(
        verified.is_ok(),
        "Hash chain should be valid: {:?}",
        verified.err()
    );

    let count = verified.unwrap();
    assert_eq!(
        count as usize,
        records.len(),
        "All records should be in chain"
    );

    eprintln!("Hash chain integrity verified for {} records", count);
}

/// Test: Tampering with a billing record is detected
#[tokio::test]
async fn test_billing_tampering_detected() {
    let _guard = BILLING_E2E_LOCK.lock().await;

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

    add_route(&bus, "tamper.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "tamper.local", 30)
        .await
        .expect("App did not become ready");

    eprintln!("Readiness probe triggered the first billed request");

    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    let mut records = collect_billing_records_after_shutdown(node, 1, Duration::from_secs(12))
        .await
        .expect("billing records were not persisted in time");
    eprintln!("Billing records persisted after app removal");

    assert!(!records.is_empty(), "Should have at least one record");

    let original_fuel = records[0].fuel_consumed;
    records[0].fuel_consumed = 999999999;

    let result = billing::verify_chain(&records);
    assert!(result.is_err(), "Tampering should be detected");

    match result {
        Err(billing::ChainError::TamperedRecord { seq, .. }) => {
            assert_eq!(seq, records[0].seq);
            eprintln!("Tampering detected at record {}", seq);
        }
        _ => panic!("Expected TamperedRecord error"),
    }

    eprintln!(
        "Tampering detection verified (changed fuel from {} to 999999999)",
        original_fuel
    );
}

/// Test: Billing report generation
#[tokio::test]
async fn test_billing_report_generation() {
    let _guard = BILLING_E2E_LOCK.lock().await;

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

    add_route(&bus, "report.local", app_id)
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "report.local", 30)
        .await
        .expect("App did not become ready");

    eprintln!("Readiness probe triggered the first billed request");

    remove_app(&bus, app_id)
        .await
        .expect("Failed to remove app");

    let records = collect_billing_records_after_shutdown(node, 1, Duration::from_secs(12))
        .await
        .expect("billing records were not persisted in time");
    eprintln!("Billing records persisted after app removal");

    let report = billing::report::generate_report(&records, "report-tenant", 0, u64::MAX);

    eprintln!("Generated billing report:");
    eprintln!("   Tenant: {}", report.tenant_id);
    eprintln!("   Total requests: {}", report.total_requests);
    eprintln!("   Total fuel: {}", report.total_fuel_consumed);
    eprintln!("   Per-app: {:?}", report.per_app.len());

    assert_eq!(report.tenant_id, "report-tenant");
    assert!(
        report.total_requests > 0,
        "Should have at least one request"
    );
}
