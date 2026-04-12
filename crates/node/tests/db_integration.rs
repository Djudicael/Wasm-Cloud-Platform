/// Integration test: Verify pgBouncer health check works
#[tokio::test]
async fn test_pgbouncer_health_check_basic() {
    use tokio::net::TcpListener;

    // Start a dummy server to simulate pgBouncer
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if listener.accept().await.is_ok() {
                // Just accept and close to simulate pgBouncer responding
            }
        }
    });

    // Health check should succeed when server is available
    let available = supervisor::db_proxy::check_pgbouncer(&addr.to_string()).await;
    assert!(available, "pgBouncer should be detected as available");

    // Health check to non-existent port should fail
    let unavailable = supervisor::db_proxy::check_pgbouncer("127.0.0.1:60000").await;
    assert!(
        !unavailable,
        "Non-existent pgBouncer should be detected as unavailable"
    );

    println!("✓ pgBouncer health check integration test passed");
}

/// Test: Verify ConnectionProxy can be created
#[test]
fn test_connection_proxy_instantiation() {
    let proxy = supervisor::db_proxy::ConnectionProxy::new(10, "localhost:5432".to_string());
    drop(proxy);
    println!("✓ ConnectionProxy can be instantiated");
}
