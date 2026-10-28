//! Real guest checks for both P2 linkers. No container or microVM is needed.
use bytes::Bytes;
use common::policy::{NetworkPolicyConfig, PolicyConfig};
use common::types::{AppConfig, AppId, FuelQuota};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::executor::{SocketAddrCheckFn, SocketAddrUse};
use runtime::WasmRuntime;
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn h2(
    addr: std::net::SocketAddr,
) -> (
    hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    let stream = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    (sender, tokio::spawn(connection))
}
fn request(path: &str) -> hyper::Request<Empty<Bytes>> {
    hyper::Request::builder()
        .uri(format!("http://probe{path}"))
        .body(Empty::new())
        .unwrap()
}
fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn stalled(mode: &'static str, cli: bool) {
    let fixture = std::env::var(if cli {
        "WCP_ECHO_PROBE_COMPONENT"
    } else {
        "WCP_HTTP_PROBE_COMPONENT"
    })
    .expect("set fixture paths");
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&std::fs::read(fixture).unwrap()).unwrap();
    let mut config = AppConfig::default_for(AppId::new("deadline-probe", "v1"));
    config.fuel_quota = FuelQuota(2_000_000_000);
    config.policy = Some(PolicyConfig {
        network: Some(NetworkPolicyConfig {
            allow_outbound_udp: Some(mode == "udp-read"),
            max_egress_bytes: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    });
    let prepared = runtime.prepare(&artifact, config).unwrap();
    // Keep the receive window small so the write case stalls deterministically.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
    let backend = listener.local_addr().unwrap();
    let extra: Option<SocketAddrCheckFn> = (mode == "connect").then(|| {
        Arc::new(|_, kind| {
            Box::pin(async move {
                if kind == SocketAddrUse::TcpConnect {
                    // The fallback bounds the test even if a regression removes
                    // the platform deadline. It must lose to the 10-second cap.
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    false
                } else {
                    true
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + Sync>>
        }) as SocketAddrCheckFn
    });
    let started = tokio::time::Instant::now();
    if cli {
        let mut instance = prepared
            .spawn_instance(
                vec![
                    ("RESOURCE_PROBE_IO_MODE".into(), mode.into()),
                    ("RESOURCE_PROBE_TCP_TARGET".into(), backend.to_string()),
                ],
                free_addr().port(),
                extra,
            )
            .unwrap();
        let counters = instance.policy_counters();
        let task = tokio::task::spawn_blocking(move || instance.run());
        let peer = if mode == "connect" || mode == "udp-read" {
            None
        } else {
            Some(
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap()
                    .0,
            )
        };
        let stats = tokio::time::timeout(Duration::from_secs(35), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            stats.trap.as_deref().unwrap().contains(&format!(
                "WASI socket host {} deadline exceeded",
                if mode == "udp-read" { "read" } else { mode }
            )),
            "{:?}",
            stats.trap
        );
        assert_eq!(
            counters.outbound_connections_active.load(Ordering::Acquire),
            0
        );
        assert_counts(&counters, mode);
        assert_closed(peer).await;
    } else {
        let addr = free_addr();
        let instance = prepared
            .spawn_http_server(
                vec![("RESOURCE_PROBE_TCP_TARGET".into(), backend.to_string())],
                addr,
                extra,
                None,
            )
            .unwrap();
        let counters = instance.policy_counters.clone();
        let (mut sender, connection) = h2(addr).await;
        let path = if mode == "udp-read" {
            "/probe/udp/read"
        } else if mode == "write" {
            "/probe/tcp/stall-write"
        } else if mode == "read" {
            "/probe/tcp/read"
        } else {
            "/probe/tcp/connect"
        };
        let task = tokio::spawn(async move { sender.send_request(request(path)).await });
        let peer = if mode == "connect" || mode == "udp-read" {
            None
        } else {
            Some(
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap()
                    .0,
            )
        };
        assert!(tokio::time::timeout(Duration::from_secs(35), task)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_counts(&counters, mode);
        assert_eq!(
            counters.outbound_connections_active.load(Ordering::Acquire),
            0
        );
        assert_closed(peer).await;
        connection.abort();
        // One request store times out; another request on the adapter still works.
        let (mut sender, connection) = h2(addr).await;
        let response = sender.send_request(request("/health")).await.unwrap();
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            b"healthy\n"
        );
        drop(sender);
        connection.abort();
        let _ = instance.shutdown_tx.send(());
        let stats = tokio::time::timeout(Duration::from_secs(3), instance.task)
            .await
            .unwrap()
            .unwrap();
        assert!(stats.trap.is_none());
    }
    let cap = if mode == "connect" { 10 } else { 30 };
    assert!(started.elapsed() >= Duration::from_secs(cap));
    assert!(started.elapsed() < Duration::from_secs(cap + 5));
    if mode == "connect" {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "the pending supervisor gate must not contact the backend"
        );
    }
}
fn assert_counts(counters: &runtime::policy_tracker::PolicyCounters, mode: &str) {
    assert_eq!(
        counters.io_connect_timeouts.load(Ordering::Acquire),
        u64::from(mode == "connect")
    );
    assert_eq!(
        counters.io_read_timeouts.load(Ordering::Acquire),
        u64::from(mode == "read" || mode == "udp-read")
    );
    assert_eq!(
        counters.io_write_timeouts.load(Ordering::Acquire),
        u64::from(mode == "write")
    );
}
async fn assert_closed(peer: Option<tokio::net::TcpStream>) {
    if let Some(mut peer) = peer {
        let mut bytes = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut bytes))
            .await
            .unwrap();
        if let Err(error) = result {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
    }
}

#[tokio::test]
#[ignore = "set WCP_HTTP_PROBE_COMPONENT and WCP_ECHO_PROBE_COMPONENT to built WASI fixtures"]
async fn raw_connect_deadline_covers_pending_policy_gate_in_both_linkers() {
    tokio::join!(stalled("connect", true), stalled("connect", false));
}
#[tokio::test]
#[ignore = "set WCP_HTTP_PROBE_COMPONENT and WCP_ECHO_PROBE_COMPONENT to built WASI fixtures"]
async fn raw_read_deadline_covers_stalled_peer_in_both_linkers() {
    tokio::join!(stalled("read", true), stalled("read", false));
}
#[tokio::test]
#[ignore = "set WCP_HTTP_PROBE_COMPONENT and WCP_ECHO_PROBE_COMPONENT to built WASI fixtures"]
async fn raw_write_deadline_covers_backpressure_in_both_linkers() {
    tokio::join!(stalled("write", true), stalled("write", false));
}

#[tokio::test]
#[ignore = "set WCP_HTTP_PROBE_COMPONENT and WCP_ECHO_PROBE_COMPONENT to built WASI fixtures"]
async fn raw_udp_read_deadline_covers_stalled_receive_in_both_linkers() {
    tokio::join!(stalled("udp-read", true), stalled("udp-read", false));
}

#[tokio::test]
#[ignore = "set WCP_HTTP_PROBE_COMPONENT to built WASI fixture"]
async fn closing_raw_connections_returns_slots_without_kernel_observation() {
    let runtime = WasmRuntime::new().unwrap();
    let fixture = std::env::var("WCP_HTTP_PROBE_COMPONENT").unwrap();
    let artifact = runtime.compile(&std::fs::read(fixture).unwrap()).unwrap();
    let mut config = AppConfig::default_for(AppId::new("reconnect", "v1"));
    config.policy = Some(PolicyConfig {
        network: Some(NetworkPolicyConfig {
            max_outbound_connections: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for _ in 0..3 {
            let _ = listener.accept().await.unwrap();
        }
    });
    let addr = free_addr();
    let instance = runtime
        .prepare(&artifact, config)
        .unwrap()
        .spawn_http_server(
            vec![(
                "RESOURCE_PROBE_TCP_TARGET".into(),
                format!("localhost:{}", backend.port()),
            )],
            addr,
            None,
            None,
        )
        .unwrap();
    let (mut sender, connection) = h2(addr).await;
    let response = sender
        .send_request(request("/probe/tcp/reconnect"))
        .await
        .unwrap();
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"connected:3"
    );
    assert_eq!(
        instance
            .policy_counters
            .outbound_connections_active
            .load(Ordering::Acquire),
        0
    );
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    drop(sender);
    connection.abort();
    let _ = instance.shutdown_tx.send(());
    assert!(instance.task.await.unwrap().trap.is_none());
}

#[tokio::test]
#[ignore = "set WCP_ECHO_PROBE_COMPONENT to built WASI fixture"]
async fn idle_cli_listener_survives_longer_than_the_socket_io_deadline() {
    let runtime = WasmRuntime::new().unwrap();
    let fixture = std::env::var("WCP_ECHO_PROBE_COMPONENT").unwrap();
    let artifact = runtime.compile(&std::fs::read(fixture).unwrap()).unwrap();
    let config = AppConfig::default_for(AppId::new("idle-listener", "v1"));
    let prepared = runtime.prepare(&artifact, config).unwrap();
    let addr = free_addr();
    let mut instance = prepared
        .spawn_instance(
            vec![("RESOURCE_PROBE_MAX_REQUESTS".into(), "1".into())],
            addr.port(),
            None,
        )
        .unwrap();
    let counters = instance.policy_counters();
    let task = tokio::task::spawn_blocking(move || instance.run());
    tokio::time::sleep(runtime::limits::RAW_SOCKET_IO_TIMEOUT + Duration::from_secs(1)).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    assert!(tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .trap
        .is_none());
    assert_eq!(counters.io_connect_timeouts.load(Ordering::Acquire), 0);
    assert_eq!(counters.io_read_timeouts.load(Ordering::Acquire), 0);
    assert_eq!(counters.io_write_timeouts.load(Ordering::Acquire), 0);
}
