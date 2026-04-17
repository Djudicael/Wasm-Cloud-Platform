/// End-to-end test: Deploy postgres-app and connect to PostgreSQL
///
/// This test:
/// 1. Starts a NATS container
/// 2. Starts a PostgreSQL container
/// 3. Starts a wasm-node process with DATABASE_URL pointing to PostgreSQL
/// 4. Uploads postgres-app.wasm to the artifact server
/// 5. Publishes a DeployApp event
/// 6. Adds a route
/// 7. Sends an HTTP request to /query endpoint
/// 8. Verifies the PostgreSQL query response
///
/// To run:
/// ```
/// # Build prerequisites
/// cargo build --bin wasm-node
/// RUSTFLAGS='--cfg tokio_unstable' cargo build --manifest-path apps/postgres-app/Cargo.toml --target wasm32-wasip2 --release
///
/// # Run test
/// cargo test -p e2e test_postgres_connection -- --ignored --nocapture
/// ```
mod harness;

use harness::*;

#[tokio::test]
#[ignore] // Requires built binaries, NATS, and PostgreSQL containers
async fn test_postgres_connection() {
    let postgres_pwd = "testpassword123";

    let pg = PostgresContainer::start(5432, postgres_pwd)
        .await
        .expect("Failed to start PostgreSQL");
    eprintln!("✓ PostgreSQL ready at {}", pg.url);

    let nats = NatsContainer::start(4222)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");

    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");

    let node = NodeProcess::start("test-node-postgres", &nats.url, 8181, 9001)
        .await
        .expect("Failed to start node");

    let wasm_path = find_postgres_app_wasm().expect("postgres-app.wasm not found");
    let sha256 = compute_sha256(&wasm_path).expect("Failed to compute SHA-256");
    let size_bytes = std::fs::metadata(&wasm_path)
        .expect("Failed to get file size")
        .len();

    let file_server = FileServer::start(9101, &wasm_path)
        .await
        .expect("Failed to start file server");

    let wasm_filename = wasm_path.file_name().unwrap().to_str().unwrap();
    let artifact_url = file_server.wasm_url(wasm_filename);

    let mut config = build_app_config("postgres-app:v1", 100_000_000, 100, 2);
    let db_url = format!(
        "postgres://postgres:{}@127.0.0.1:5432/postgres",
        postgres_pwd
    );
    config.env_vars.insert("DATABASE_URL".to_string(), db_url);

    deploy_app(
        &bus,
        "postgres-app:v1",
        artifact_url,
        sha256.clone(),
        size_bytes,
        config,
    )
    .await
    .expect("Failed to deploy app");

    add_route(&bus, "postgres-app.local", "postgres-app:v1")
        .await
        .expect("Failed to add route");

    wait_for_app_ready(node.proxy_port, "postgres-app.local", 30)
        .await
        .expect("App did not become ready");

    let response = send_request(node.proxy_port, "postgres-app.local", "/query")
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "Expected 200 OK, got {}",
        response.status()
    );

    let body = response.text().await.expect("Failed to read response body");
    eprintln!("✓ Response: {}", body);

    assert!(
        body.contains("status") || body.contains("ok"),
        "Expected response to contain 'status' or 'ok', got: {}",
        body
    );

    node.stop().ok();

    eprintln!("✅ test_postgres_connection PASSED");
}

#[test]
fn test_e2e_postgres_infrastructure() {
    assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).exists());
}
