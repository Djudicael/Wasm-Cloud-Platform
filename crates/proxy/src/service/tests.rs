use super::*;
use crate::rate_limiter::RateLimiter;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn strip_uri_prefix_handles_path_and_query_independently() {
    let cases = [
        ("/api", "/api/health", None, Some("/health")),
        ("/api", "/api/health", Some("x=1"), Some("/health?x=1")),
        (
            "/api",
            "/api/health",
            Some("x=1&y=2"),
            Some("/health?x=1&y=2"),
        ),
        ("/api", "/api", None, Some("")),
        ("/api", "/api", Some("x=1"), Some("?x=1")),
        ("/api", "/other", Some("x=1"), None),
    ];

    for (prefix, path, query, expected) in cases {
        let actual = strip_uri_prefix(path, query, prefix);
        assert_eq!(actual.as_deref(), expected);
    }
}

#[tokio::test]
async fn test_unknown_host_returns_502_behavior() {
    let router = Arc::new(HostRouter::default());
    let resolved = router.resolve("unknown.com", "/").await;
    assert!(
        resolved.is_none(),
        "Unknown host should not resolve, leading to 502"
    );
}

#[test]
fn test_canonical_host_strips_port_suffix() {
    assert_eq!(canonical_host("wasi-grpc.local:8380"), "wasi-grpc.local");
    assert_eq!(canonical_host("example.com"), "example.com");
    assert_eq!(canonical_host("[::1]:8443"), "::1");
}

#[test]
fn test_extract_request_host_prefers_host_header_value() {
    use pingora::http::RequestHeader;

    let mut req = RequestHeader::build("GET", b"/", None).unwrap();
    req.insert_header("host", "wasi-grpc.local:8380").unwrap();
    assert_eq!(
        canonical_host(
            req.headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap()
        ),
        "wasi-grpc.local"
    );
}

#[test]
fn untrusted_platform_identity_and_trace_headers_are_removed() {
    use pingora::http::RequestHeader;

    let mut request = RequestHeader::build("GET", b"/", None).unwrap();
    for (name, value) in [
        ("x-app-id", "attacker-app"),
        ("x-trace-id", "attacker-trace"),
        (
            "traceparent",
            "00-11111111111111111111111111111111-2222222222222222-01",
        ),
        ("tracestate", "vendor=attacker"),
        ("authorization", "Bearer must-remain-forwardable"),
        ("cookie", "session=must-remain-forwardable"),
    ] {
        request.insert_header(name, value).unwrap();
    }

    super::forwarding::remove_untrusted_platform_headers(&mut request);

    for name in ["x-app-id", "x-trace-id", "traceparent", "tracestate"] {
        assert!(
            request.headers.get(name).is_none(),
            "{name} must be removed"
        );
    }
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        "Bearer must-remain-forwardable"
    );
    assert_eq!(
        request.headers.get("cookie").unwrap(),
        "session=must-remain-forwardable"
    );
}

#[tokio::test]
async fn test_wasm_proxy_cold_start() {
    use crate::rate_limiter::RateLimitConfig;

    let router = Arc::new(HostRouter::default());
    let upstream = Arc::new(UpstreamRegistry::default());
    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));

    let cold_start_triggered = Arc::new(AtomicBool::new(false));
    let cold_start_triggered_clone = cold_start_triggered.clone();

    let cold_start = Arc::new(move |_app_id: AppId| {
        let trigger = cold_start_triggered_clone.clone();
        Box::pin(async move {
            trigger.store(true, Ordering::SeqCst);
            Some("127.0.0.1:8080".parse().unwrap())
        }) as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    let proxy = WasmProxy {
        router,
        upstream,
        rate_limiter,
        backpressure: crate::backpressure::BackpressureSignal::new(),
        node_table: Arc::new(crate::node_table::NodeLoadTable::default()),
        local_node_id: "node-0".to_string(),
        metrics: None,
        gateway: Arc::new(Gateway::new(None)),
        cold_start,
        max_body_size_bytes: super::DEFAULT_MAX_BODY_SIZE_BYTES,
    };

    let app_id = AppId("test-app".to_string());

    let addr = proxy.upstream.next(&app_id).await;
    assert!(addr.is_none());

    let result = (proxy.cold_start)(app_id.clone()).await;
    assert!(result.is_some());
    assert!(
        cold_start_triggered.load(Ordering::SeqCst),
        "Cold start should be triggered when the pool is empty"
    );
}

#[tokio::test]
async fn test_select_upstream_prefers_remote_proxy_when_local_node_is_overloaded() {
    use crate::node_table::NodeEntry;
    use crate::rate_limiter::RateLimitConfig;
    use std::time::{SystemTime, UNIX_EPOCH};

    let router = Arc::new(HostRouter::default());
    let upstream = Arc::new(UpstreamRegistry::default());
    let rate_limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
    let node_table = Arc::new(crate::node_table::NodeLoadTable::default());
    let cold_start_triggered = Arc::new(AtomicBool::new(false));
    let cold_start_triggered_clone = cold_start_triggered.clone();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    node_table
        .update(NodeEntry {
            node_id: "node-local".to_string(),
            proxy_address: "127.0.0.1:18080".to_string(),
            fuel_used_percent: 95.0,
            active_instances: 0,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        })
        .await;
    node_table
        .update(NodeEntry {
            node_id: "node-remote".to_string(),
            proxy_address: "127.0.0.1:28080".to_string(),
            fuel_used_percent: 10.0,
            active_instances: 0,
            last_seen: now,
            health_status: common::health::NodeHealthStatus::Healthy,
        })
        .await;

    let cold_start = Arc::new(move |_app_id: AppId| {
        let trigger = cold_start_triggered_clone.clone();
        Box::pin(async move {
            trigger.store(true, Ordering::SeqCst);
            Some("127.0.0.1:8080".parse().unwrap())
        }) as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    let proxy = WasmProxy {
        router,
        upstream,
        rate_limiter,
        backpressure: crate::backpressure::BackpressureSignal::new(),
        node_table,
        local_node_id: "node-local".to_string(),
        metrics: None,
        gateway: Arc::new(Gateway::new(None)),
        cold_start,
        max_body_size_bytes: super::DEFAULT_MAX_BODY_SIZE_BYTES,
    };

    let app_id = AppId("test-app".to_string());
    let endpoint = proxy.select_upstream(&app_id).await.unwrap();
    assert_eq!(endpoint.addr, "127.0.0.1:28080".parse().unwrap());
    assert!(!endpoint.h2c);
    assert!(
        !cold_start_triggered.load(Ordering::SeqCst),
        "local cold start should not run when an eligible remote node exists"
    );
}
