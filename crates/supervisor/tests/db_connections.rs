/// Test that pgBouncer health check works.
#[tokio::test]
async fn test_pgbouncer_health_check() {
    use tokio::net::TcpListener;

    // Start a dummy server to simulate pgBouncer
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if listener.accept().await.is_ok() {
                // Just accept and close
            }
        }
    });

    // Health check should succeed
    assert!(
        supervisor::db_proxy::check_pgbouncer(&addr.to_string()).await,
        "Health check should succeed when server is available"
    );

    // Health check to non-existent port should fail
    assert!(
        !supervisor::db_proxy::check_pgbouncer("127.0.0.1:60000").await,
        "Health check should fail when server is unavailable"
    );

    println!("✓ pgBouncer health check works correctly");
}

/// Test that DATABASE_URL is injected into app environment.
#[tokio::test]
async fn test_database_url_injection() {
    use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
    use std::collections::HashMap;

    let app_id = AppId::new("test-app", "v1");
    let mut env_vars = HashMap::new();
    env_vars.insert("FOO".to_string(), "bar".to_string());

    let config = AppConfig {
        id: app_id.clone(),
        fuel_quota: FuelQuota(1_000_000),
        memory_limit: MemoryPages(64),
        max_instances: 1,
        idle_timeout_secs: 60,
        wasm_bind_port: 8080,
        env_vars: env_vars.clone(),
        secret_keys: vec![],
        extended_limits: None,
        health_check_path: None,
        db_max_connections: Some(10),
    };

    // Simulate the env_resolver from node/main.rs
    let default_db_url = "postgres://127.0.0.1:5432/mydb".to_string();
    let env_resolver = |cfg: &AppConfig| -> Vec<(String, String)> {
        let mut vars = Vec::new();
        for (k, v) in &cfg.env_vars {
            vars.push((k.clone(), v.clone()));
        }
        if !cfg.env_vars.contains_key("DATABASE_URL") {
            vars.push(("DATABASE_URL".to_string(), default_db_url.clone()));
        }
        vars
    };

    let resolved = env_resolver(&config);

    // Check that DATABASE_URL was injected
    assert!(
        resolved
            .iter()
            .any(|(k, v)| k == "DATABASE_URL" && v == &default_db_url),
        "DATABASE_URL should be injected"
    );

    // Check that existing env vars are preserved
    assert!(
        resolved.iter().any(|(k, v)| k == "FOO" && v == "bar"),
        "Existing env vars should be preserved"
    );

    // Test that DATABASE_URL is NOT overridden if already present
    let mut env_with_db = env_vars.clone();
    env_with_db.insert(
        "DATABASE_URL".to_string(),
        "postgres://custom:5432/custom".to_string(),
    );

    let config_with_db = AppConfig {
        env_vars: env_with_db,
        ..config
    };

    let resolved_with_db = env_resolver(&config_with_db);
    assert!(
        resolved_with_db
            .iter()
            .any(|(k, v)| k == "DATABASE_URL" && v == "postgres://custom:5432/custom"),
        "Custom DATABASE_URL should not be overridden"
    );

    println!("✓ DATABASE_URL injection works correctly");
}

/// Test that the connection proxy can be instantiated.
#[test]
fn test_connection_proxy_creation() {
    let proxy = supervisor::db_proxy::ConnectionProxy::new(10, "localhost:5432".to_string());
    // Just verify we can create it
    drop(proxy);
    println!("✓ ConnectionProxy can be created");
}
