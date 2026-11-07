use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn endpoint(budget: Option<u32>) -> supervisor::network::RegisteredEndpoint {
    supervisor::network::RegisteredEndpoint {
        addr: "127.0.0.1:9".parse().unwrap(),
        h2c: false,
        request_timeout_ms: budget,
    }
}

#[test]
fn registered_target_budget_is_bounded_and_legacy_defaults_to_thirty_seconds() {
    assert_eq!(
        forwarding_budget(&endpoint(None)).unwrap(),
        Duration::from_secs(30)
    );
    for ms in [1000, 180000, 300000] {
        assert_eq!(
            forwarding_budget(&endpoint(Some(ms))).unwrap(),
            Duration::from_millis(u64::from(ms))
        );
    }
    for ms in [0, 999, 300001, u32::MAX] {
        assert_eq!(
            forwarding_budget(&endpoint(Some(ms))),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
    }
}

#[tokio::test]
async fn invalid_target_metadata_fails_before_connect_even_with_forged_budget_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("x-request-timeout-ms", "300000".parse().unwrap());
    let result = forward_request(
        endpoint(Some(0)),
        http::Method::GET,
        "/".into(),
        headers,
        axum::body::Body::empty(),
        "target",
    )
    .await;
    assert_eq!(result.unwrap_err(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn cold_start_and_reregistration_preserve_only_the_trusted_target_metadata() {
    let registry = supervisor::network::NamespaceRegistry::default();
    let app = common::types::AppId::new_namespaced("default", "target", "v1");
    let mut selected = endpoint(Some(180000));
    assert_eq!(
        registered_after_cold_start(&registry, "default", "target", selected.addr).await,
        Err(StatusCode::SERVICE_UNAVAILABLE)
    );
    registry.register_endpoint(&app, selected).await;
    assert_eq!(
        registered_after_cold_start(&registry, "default", "target", selected.addr)
            .await
            .unwrap(),
        selected
    );
    registry.deregister(&app, &selected.addr).await;
    assert!(registry
        .resolve_endpoint("default", "target")
        .await
        .is_none());
    selected.request_timeout_ms = Some(150000);
    registry.register_endpoint(&app, selected).await;
    assert_eq!(
        registered_after_cold_start(&registry, "default", "target", selected.addr)
            .await
            .unwrap(),
        selected
    );
    assert_eq!(
        registered_after_cold_start(
            &registry,
            "default",
            "target",
            "127.0.0.1:10".parse().unwrap()
        )
        .await,
        Err(StatusCode::SERVICE_UNAVAILABLE)
    );
    registry.deregister(&app, &selected.addr).await;
    registry.register(&app, selected.addr).await;
    assert_eq!(
        registry
            .resolve_endpoint("default", "target")
            .await
            .unwrap()
            .request_timeout_ms,
        None
    );
}

#[tokio::test]
async fn one_short_deadline_covers_http1_and_http2_headers_and_cancels_the_driver() {
    for h2c in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let mut server = PendingForwardConnection(Some(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Never send headers/settings: include handshake waiting as well.
            let mut input = Vec::new();
            let _ = stream.read_to_end(&mut input).await;
            let _ = closed_tx.send(!input.is_empty());
        })));
        let mut selected = endpoint(Some(180000));
        selected.addr = addr;
        selected.h2c = h2c;
        let mut headers = HeaderMap::new();
        headers.insert("x-request-timeout-ms", "300000".parse().unwrap());
        headers.insert("x-forwarding-timeout", "300000".parse().unwrap());
        let started = Instant::now();
        let result = forward_with_budget(
            selected,
            http::Method::GET,
            "/".into(),
            headers,
            axum::body::Body::empty(),
            "target",
            Duration::from_millis(40),
        )
        .await;
        assert_eq!(result.unwrap_err(), StatusCode::GATEWAY_TIMEOUT);
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .unwrap()
            .unwrap());
        server.0.take().unwrap().await.unwrap();
    }
}

#[tokio::test]
async fn receiving_headers_keeps_the_existing_streamed_body_contract() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut server = PendingForwardConnection(Some(tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            headers.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        stream.write_all(b"ok").await.unwrap();
    })));
    let mut selected = endpoint(Some(180000));
    selected.addr = addr;
    let response = forward_with_budget(
        selected,
        http::Method::GET,
        "/".into(),
        HeaderMap::new(),
        axum::body::Body::empty(),
        "target",
        Duration::from_millis(40),
    )
    .await
    .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"ok");
    server.0.take().unwrap().await.unwrap();
}
