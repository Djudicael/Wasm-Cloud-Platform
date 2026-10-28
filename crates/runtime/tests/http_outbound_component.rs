//! Real WASI outgoing-handler regression. Build the fixture explicitly before
//! running this ignored test; no service, container, or microVM is required.

use bytes::Bytes;
use common::policy::{NetworkPolicyConfig, PolicyConfig};
use common::types::{AppConfig, AppId};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::executor::SocketAddrCheckFn;
use runtime::WasmRuntime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
#[ignore = "build http-hello-component for wasm32-wasip2 and set WCP_HTTP_PROBE_COMPONENT"]
async fn real_component_outgoing_http_obeys_network_and_supervisor_policy() {
    let path = std::env::var("WCP_HTTP_PROBE_COMPONENT").expect("set WCP_HTTP_PROBE_COMPONENT");
    let wasm = std::fs::read(path).unwrap();
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&wasm).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let backend_task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                headers.push(byte[0]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
    });
    for (tcp, denied, extra_denial, profile, connection_limit) in [
        (false, false, false, None, None),
        (true, false, false, None, None),
        (true, true, false, None, None),
        (true, false, true, None, None),
        (true, false, false, None, Some(0)),
        (
            false,
            false,
            false,
            Some(common::policy::NetworkProfile::Isolated),
            None,
        ),
        (
            true,
            false,
            false,
            Some(common::policy::NetworkProfile::MeshOnly),
            None,
        ),
        (
            true,
            false,
            false,
            Some(common::policy::NetworkProfile::EgressAllowlist),
            None,
        ),
    ] {
        let mut config = AppConfig::default_for(AppId::new("http-egress-probe", "v1"));
        config.extended_limits =
            connection_limit.map(|limit| common::types::ExtendedLimitsConfig {
                max_outbound_connections: Some(limit),
                max_net_egress_bytes: None,
                max_open_fds: None,
                max_fs_write_bytes: None,
                max_table_elements: None,
            });
        config.policy = Some(PolicyConfig {
            network: Some(NetworkPolicyConfig {
                profile,
                allowed_http_hosts: (profile
                    == Some(common::policy::NetworkProfile::EgressAllowlist))
                .then(|| vec!["models.example.com".into()]),
                allow_outbound_tcp: Some(tcp),
                allow_dns: Some(false),
                allowed_cidrs: Some(vec!["127.0.0.0/8".into()]),
                denied_cidrs: Some(if denied {
                    vec!["127.0.0.0/8".into()]
                } else {
                    vec![]
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let prepared = runtime.prepare(&artifact, config).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let deny: SocketAddrCheckFn = Arc::new(|_, _| Box::pin(async { false }));
        let extra = extra_denial.then_some(deny);
        let instance = prepared
            .spawn_http_server(
                vec![(
                    "RESOURCE_PROBE_HTTP_TARGET".into(),
                    format!("http://{backend}/"),
                )],
                addr,
                extra,
                None,
            )
            .unwrap();
        let stream = tokio::time::timeout(Duration::from_secs(3), async {
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
        let request = hyper::Request::builder()
            .uri("http://probe/probe/http/get")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), sender.send_request(request))
            .await
            .unwrap()
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        if tcp && !denied && !extra_denial && profile.is_none() && connection_limit != Some(0) {
            assert_eq!(body, "status:200");
        } else {
            assert!(body.starts_with("denied:"), "{body}");
        }
        drop(sender);
        connection.abort();
        let _ = instance.shutdown_tx.send(());
        let stats = tokio::time::timeout(Duration::from_secs(3), instance.task)
            .await
            .unwrap()
            .unwrap();
        assert!(stats.trap.is_none(), "{:?}", stats.trap);
    }
    assert_eq!(
        received.load(Ordering::Relaxed),
        1,
        "denied guests must never connect"
    );
    backend_task.abort();
}
