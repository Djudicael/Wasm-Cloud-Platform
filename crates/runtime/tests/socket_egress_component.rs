//! Exercise the actual guest socket imports on the async HTTP linker.
use bytes::Bytes;
use common::policy::{NetworkPolicyConfig, PolicyConfig};
use common::types::{AppConfig, AppId, ExtendedLimitsConfig};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::WasmRuntime;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
#[ignore = "build http-hello-component for wasm32-wasip2 and set WCP_HTTP_PROBE_COMPONENT"]
async fn guest_tcp_and_udp_share_policy_and_extended_byte_ceilings() {
    let path = std::env::var("WCP_HTTP_PROBE_COMPONENT").expect("set WCP_HTTP_PROBE_COMPONENT");
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&std::fs::read(path).unwrap()).unwrap();
    for udp in [false, true] {
        for limited in [false, true] {
            let expected = if limited { 4 } else { 12 };
            let (backend, backend_task) = if udp {
                let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let addr = socket.local_addr().unwrap();
                let task = tokio::spawn(async move {
                    let mut received = Vec::new();
                    let mut buffer = [0; 128];
                    while received.len() < expected {
                        let count =
                            tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buffer))
                                .await
                                .unwrap()
                                .unwrap();
                        received.extend_from_slice(&buffer[..count]);
                    }
                    assert!(
                        tokio::time::timeout(Duration::from_millis(100), socket.recv(&mut buffer))
                            .await
                            .is_err(),
                        "quota must prevent extra datagrams"
                    );
                    received
                });
                (addr, task)
            } else {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let task = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut received = Vec::new();
                    stream.read_to_end(&mut received).await.unwrap();
                    received
                });
                (addr, task)
            };
            let mut config = AppConfig::default_for(AppId::new("socket-meter-probe", "v1"));
            config.policy = Some(PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_udp: Some(udp),
                    max_egress_bytes: Some(if limited { 10 } else { 0 }),
                    ..Default::default()
                }),
                ..Default::default()
            });
            if limited {
                config.extended_limits = Some(ExtendedLimitsConfig {
                    max_net_egress_bytes: Some(6),
                    max_outbound_connections: None,
                    max_open_fds: None,
                    max_fs_write_bytes: None,
                    max_table_elements: None,
                });
            }
            let prepared = runtime.prepare(&artifact, config).unwrap();
            let addr = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let instance = prepared
                .spawn_http_server(
                    vec![("RESOURCE_PROBE_TCP_TARGET".into(), backend.to_string())],
                    addr,
                    None,
                    None,
                )
                .unwrap();
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
            let (mut sender, connection) =
                hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                    .handshake(TokioIo::new(stream))
                    .await
                    .unwrap();
            let connection = tokio::spawn(connection);
            let protocol = if udp { "udp" } else { "tcp" };
            let request = hyper::Request::builder()
                .uri(format!("http://probe/probe/{protocol}/write"))
                .body(Empty::<Bytes>::new())
                .unwrap();
            let response =
                tokio::time::timeout(Duration::from_secs(5), sender.send_request(request))
                    .await
                    .unwrap()
                    .unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                body.as_ref(),
                format!("written:{expected};denied:{limited}").as_bytes()
            );
            drop(sender);
            connection.abort();
            let _ = instance.shutdown_tx.send(());
            let stats = tokio::time::timeout(Duration::from_secs(5), instance.task)
                .await
                .unwrap()
                .unwrap();
            assert!(stats.trap.is_none(), "{:?}", stats.trap);
            assert_eq!(stats.io_stats.net_egress_bytes, expected as u64);
            let received = tokio::time::timeout(Duration::from_secs(5), backend_task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, b"test".repeat(expected / 4));
        }
    }
}

#[tokio::test]
#[ignore = "build echo-service for wasm32-wasip2 and set WCP_ECHO_PROBE_COMPONENT"]
async fn cli_accepted_tcp_streams_are_metered_by_sync_linker() {
    use std::sync::atomic::Ordering;
    let path = std::env::var("WCP_ECHO_PROBE_COMPONENT").expect("set WCP_ECHO_PROBE_COMPONENT");
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&std::fs::read(path).unwrap()).unwrap();
    for limit in [0, 1] {
        let mut config = AppConfig::default_for(AppId::new("accepted-meter-probe", "v1"));
        config.policy = Some(PolicyConfig {
            network: Some(NetworkPolicyConfig {
                max_egress_bytes: Some(limit),
                ..Default::default()
            }),
            ..Default::default()
        });
        let prepared = runtime.prepare(&artifact, config).unwrap();
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut instance = prepared
            .spawn_instance(
                vec![("RESOURCE_PROBE_MAX_REQUESTS".into(), "1".into())],
                addr.port(),
                None,
            )
            .unwrap();
        let counters = instance.policy_counters();
        let task = tokio::task::spawn_blocking(move || instance.run());
        let mut stream = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let stats = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(stats.trap.is_none(), "{:?}", stats.trap);
        if limit == 0 {
            assert!(response.starts_with(b"HTTP/1.1 200 OK"));
            assert_eq!(stats.io_stats.net_egress_bytes, response.len() as u64);
        } else {
            assert!(response.is_empty());
            assert_eq!(stats.io_stats.net_egress_bytes, 0);
            assert_eq!(counters.egress_denied_total.load(Ordering::Acquire), 1);
        }
    }
}
