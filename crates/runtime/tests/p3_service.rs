use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::WasmRuntime;
use std::sync::atomic::Ordering;

async fn request(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let stream = loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };
    let (mut client, connection) =
        hyper::client::conn::http2::handshake::<_, _, Empty<bytes::Bytes>>(
            TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let request = hyper::Request::builder()
        .uri(format!("http://localhost{path}"))
        .header("x-trigger-id", "forged")
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let response = client.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    drop(client);
    task.abort();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Requires `cargo build --target wasm32-wasip2 -p http-p3-probe` first.
#[tokio::test]
#[ignore = "requires the explicitly built P3 WASI probe"]
async fn p3_http_policy_and_identity_boundary() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "../../target".into());
        let path = std::path::Path::new(&target).join("wasm32-wasip2/debug/http_p3_probe.wasm");
        let runtime = WasmRuntime::new().unwrap();
        let artifact = runtime.compile(&std::fs::read(path).unwrap()).unwrap();
        let mut config = AppConfig::default_for(AppId("test/p3:v1".into()));
        config.fuel_quota = FuelQuota(2_000_000_000);
        config.memory_limit = MemoryPages(2048);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let prepared = runtime.prepare(&artifact, config).unwrap();
        let server = prepared
            .spawn_http_server(
                vec![("PROBE_HTTP_TARGET".into(), "http://127.0.0.1:9".into())],
                addr,
                Some(std::sync::Arc::new(|_, _| Box::pin(async { false }))),
                None,
            )
            .unwrap();
        assert_eq!(
            request(addr, "/health").await,
            (200, "platform-p3-probe".into())
        );
        assert_eq!(
            request(addr, "/probe/trigger-identity").await,
            (200, "none".into())
        );
        assert_eq!(request(addr, "/_platform/triggers/sweep").await.0, 403);
        assert_eq!(request(addr, "/probe/http").await, (200, "denied".into()));
        assert!(
            server
                .policy_counters
                .connection_denied_total
                .load(Ordering::Relaxed)
                > 0
        );
        let _ = server.shutdown_tx.send(());
        assert!(server.task.await.unwrap().trap.is_none());
    })
    .await
    .expect("P3 hosting test timed out");
}

#[tokio::test]
#[ignore = "requires the explicitly built P3 WASI probe"]
async fn p3_tcp_releases_slots_and_counts_connections() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "../../target".into());
        let runtime = WasmRuntime::new().unwrap();
        let artifact = runtime
            .compile(
                &std::fs::read(
                    std::path::Path::new(&target).join("wasm32-wasip2/debug/http_p3_probe.wasm"),
                )
                .unwrap(),
            )
            .unwrap();
        let mut config = AppConfig::default_for(AppId("test/p3-tcp:v1".into()));
        config.fuel_quota = FuelQuota(2_000_000_000);
        config.memory_limit = MemoryPages(2048);
        config.policy = Some(common::policy::PolicyConfig {
            network: Some(common::policy::NetworkPolicyConfig {
                max_outbound_connections: Some(1),
                ..Default::default()
            }),
            filesystem: None,
        });
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let prepared = runtime.prepare(&artifact, config).unwrap();
        let server = prepared
            .spawn_http_server(
                vec![("PROBE_TCP_TARGET".into(), peer_addr.to_string())],
                addr,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            request(addr, "/probe/tcp").await,
            (200, "connected:3".into())
        );
        assert_eq!(
            server
                .policy_counters
                .outbound_connections_total
                .load(Ordering::Relaxed),
            3
        );
        assert_eq!(
            server
                .policy_counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
        let _ = server.shutdown_tx.send(());
        assert!(server.task.await.unwrap().trap.is_none());
    })
    .await
    .expect("P3 TCP test timed out");
}

/// Uses an explicitly selected disposable database; never prints its URL.
#[tokio::test]
#[ignore = "requires P3 probe and WCP_P3_DATABASE_FILE pointing to a disposable database URL"]
async fn p3_postgres_streams_and_egress_quota() {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        let file =
            std::env::var("WCP_P3_DATABASE_FILE").expect("explicit disposable database file");
        let url = std::fs::read_to_string(file).unwrap().trim().to_owned();
        let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "../../target".into());
        let runtime = WasmRuntime::new().unwrap();
        let artifact = runtime
            .compile(
                &std::fs::read(
                    std::path::Path::new(&target).join("wasm32-wasip2/debug/http_p3_probe.wasm"),
                )
                .unwrap(),
            )
            .unwrap();
        for quota in [None, Some(1)] {
            let mut config = AppConfig::default_for(AppId("test/p3-db:v1".into()));
            config.fuel_quota = FuelQuota(2_000_000_000);
            config.memory_limit = MemoryPages(2048);
            config.policy = Some(common::policy::PolicyConfig {
                network: Some(common::policy::NetworkPolicyConfig {
                    max_egress_bytes: quota,
                    ..Default::default()
                }),
                filesystem: None,
            });
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            let prepared = runtime.prepare(&artifact, config).unwrap();
            let server = prepared
                .spawn_http_server(vec![("DATABASE_URL".into(), url.clone())], addr, None, None)
                .unwrap();
            let result = request(addr, "/probe/postgres").await;
            assert_eq!(
                result,
                (
                    200,
                    if quota.is_some() {
                        "denied"
                    } else {
                        "postgres:42"
                    }
                    .into()
                )
            );
            if quota.is_some() {
                assert!(
                    server
                        .policy_counters
                        .egress_denied_total
                        .load(Ordering::Relaxed)
                        > 0
                );
            } else {
                assert!(server.policy_counters.egress_bytes.load(Ordering::Relaxed) > 0);
            }
            assert_eq!(
                server
                    .policy_counters
                    .outbound_connections_active
                    .load(Ordering::Relaxed),
                0
            );
            let _ = server.shutdown_tx.send(());
            assert!(server.task.await.unwrap().trap.is_none());
        }
    })
    .await
    .expect("P3 database test timed out");
}
