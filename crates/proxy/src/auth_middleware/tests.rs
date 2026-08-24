use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use common::auth::{AuthConfig, Permission};
use prometheus::Registry;
use tokio::sync::RwLock;
use tower::util::ServiceExt;

use super::*;

#[test]
fn test_public_endpoints() {
    assert!(is_public_endpoint("/health"));
    assert!(is_public_endpoint("/healthz"));
    assert!(is_public_endpoint("/readyz"));
    assert!(is_public_endpoint("/livez"));
    assert!(is_public_endpoint("/_platform/health"));
    assert!(is_public_endpoint("/favicon.ico"));
}

#[test]
fn test_non_public_endpoints() {
    assert!(!is_public_endpoint("/admin/config"));
    assert!(!is_public_endpoint("/admin/rebuild"));
    assert!(!is_public_endpoint("/admin/gc/force"));
    assert!(!is_public_endpoint("/admin/auth/rotate-token"));
    assert!(!is_public_endpoint("/status/pgbouncer"));
    assert!(!is_public_endpoint("/metrics"));
    assert!(!is_public_endpoint("/status/metrics"));
    assert!(!is_public_endpoint("/logs/my-app"));
    assert!(!is_public_endpoint("/upstreams"));
}

#[test]
fn test_required_permission_get() {
    let get = axum::http::Method::GET;
    assert_eq!(required_permission(&get, "/admin/config"), Permission::Read);
    assert_eq!(required_permission(&get, "/metrics"), Permission::Read);
}

#[test]
fn test_required_permission_post() {
    let post = axum::http::Method::POST;
    assert_eq!(
        required_permission(&post, "/admin/rebuild"),
        Permission::Write
    );
}

#[test]
fn test_required_permission_patch() {
    let patch = axum::http::Method::PATCH;
    assert_eq!(
        required_permission(&patch, "/admin/config"),
        Permission::Write
    );
}

#[test]
fn test_required_permission_delete() {
    let delete = axum::http::Method::DELETE;
    assert_eq!(
        required_permission(&delete, "/admin/config"),
        Permission::Write
    );
}

#[test]
fn test_required_permission_head() {
    let head = axum::http::Method::HEAD;
    assert_eq!(required_permission(&head, "/health"), Permission::Read);
}

#[test]
fn test_required_permission_options() {
    let options = axum::http::Method::OPTIONS;
    assert_eq!(
        required_permission(&options, "/admin/config"),
        Permission::Read
    );
}

#[test]
fn test_extract_client_ip_xff() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Forwarded-For", "192.168.1.1, 10.0.0.1".parse().unwrap());
    let ip = extract_client_ip(
        &headers,
        Some("127.0.0.1:1234".parse().unwrap()),
        &["127.0.0.1/32".parse().unwrap()],
    );
    assert_eq!(ip, Some("192.168.1.1".parse::<IpAddr>().unwrap()));
}

#[test]
fn test_extract_client_ip_x_real_ip() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
    let ip = extract_client_ip(
        &headers,
        Some("127.0.0.1:1234".parse().unwrap()),
        &["127.0.0.1/32".parse().unwrap()],
    );
    assert_eq!(ip, Some("10.0.0.1".parse::<IpAddr>().unwrap()));
}

#[test]
fn test_extract_client_ip_xff_priority() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Forwarded-For", "192.168.1.1".parse().unwrap());
    headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
    let ip = extract_client_ip(
        &headers,
        Some("127.0.0.1:1234".parse().unwrap()),
        &["127.0.0.1/32".parse().unwrap()],
    );
    assert_eq!(ip, Some("192.168.1.1".parse::<IpAddr>().unwrap()));
}

#[test]
fn test_extract_client_ip_none() {
    let headers = HeaderMap::new();
    let ip = extract_client_ip(&headers, None, &[]);
    assert!(ip.is_none());
}

#[test]
fn test_extract_client_ip_ipv6() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Real-IP", "::1".parse().unwrap());
    let ip = extract_client_ip(
        &headers,
        Some("[::1]:1234".parse().unwrap()),
        &["::1/128".parse().unwrap()],
    );
    assert_eq!(ip, Some("::1".parse::<IpAddr>().unwrap()));
}

#[test]
fn test_extract_client_ip_untrusted_peer_ignores_forwarded_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Forwarded-For", "192.168.1.1".parse().unwrap());
    headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
    let ip = extract_client_ip(
        &headers,
        Some("172.16.0.9:1234".parse().unwrap()),
        &["127.0.0.1/32".parse().unwrap()],
    );
    assert_eq!(ip, Some("172.16.0.9".parse::<IpAddr>().unwrap()));
}

#[test]
fn test_rate_limiter_allows_within_burst() {
    let limiter = AdminRateLimiter::new(10, 5);
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    for _ in 0..5 {
        assert!(limiter.allow(Some(ip)));
    }
}

#[test]
fn test_rate_limiter_blocks_excess() {
    let limiter = AdminRateLimiter::new(10, 3);
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    assert!(limiter.allow(Some(ip)));
    assert!(limiter.allow(Some(ip)));
    assert!(limiter.allow(Some(ip)));
    assert!(!limiter.allow(Some(ip)));
}

#[test]
fn test_rate_limiter_no_ip_allowed() {
    let limiter = AdminRateLimiter::new(10, 3);
    assert!(limiter.allow(None));
}

#[test]
fn test_rate_limiter_different_ips_independent() {
    let limiter = AdminRateLimiter::new(10, 1);
    let ip1: IpAddr = "127.0.0.1".parse().unwrap();
    let ip2: IpAddr = "127.0.0.2".parse().unwrap();

    assert!(limiter.allow(Some(ip1)));
    assert!(!limiter.allow(Some(ip1)));
    assert!(limiter.allow(Some(ip2)));
}

#[test]
fn test_rate_limiter_disabled() {
    let limiter = AdminRateLimiter::disabled();
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    for _ in 0..100 {
        assert!(limiter.allow(Some(ip)));
    }
}

#[test]
fn test_rate_limiter_prune_stale() {
    let limiter = AdminRateLimiter::new(10, 5);
    let ip1: IpAddr = "127.0.0.1".parse().unwrap();
    let ip2: IpAddr = "127.0.0.2".parse().unwrap();

    limiter.allow(Some(ip1));
    limiter.allow(Some(ip2));

    limiter.prune_stale(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(2));
    limiter.prune_stale(Duration::from_millis(1));

    assert!(limiter.allow(Some(ip1)));
    assert!(limiter.allow(Some(ip2)));
}

#[test]
fn test_validate_rotation_read() {
    let req = RotateTokenRequest {
        token_type: "read".to_string(),
        new_token: None,
    };
    let result = validate_rotation_request(&req);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 64);
}

#[test]
fn test_validate_rotation_write() {
    let req = RotateTokenRequest {
        token_type: "write".to_string(),
        new_token: Some("new_write_token_1234567890".to_string()),
    };
    let result = validate_rotation_request(&req);
    assert_eq!(result.unwrap(), "new_write_token_1234567890");
}

#[test]
fn test_validate_rotation_invalid_type() {
    let req = RotateTokenRequest {
        token_type: "admin".to_string(),
        new_token: None,
    };
    assert!(validate_rotation_request(&req).is_err());
}

#[test]
fn test_validate_rotation_short_token() {
    let req = RotateTokenRequest {
        token_type: "read".to_string(),
        new_token: Some("short".to_string()),
    };
    assert!(validate_rotation_request(&req).is_err());
}

#[test]
fn test_tls_check_auth_disabled() {
    let config = AuthConfig::default();
    assert!(check_admin_tls_requirement(&config, false).is_ok());
}

#[test]
fn test_tls_check_no_requirement() {
    let config = AuthConfig {
        enabled: true,
        require_tls: false,
        write_token: Some("a_valid_write_token_5678".to_string()),
        ..Default::default()
    };
    assert!(check_admin_tls_requirement(&config, false).is_ok());
}

#[test]
fn test_tls_check_required_but_not_configured() {
    let config = AuthConfig {
        enabled: true,
        require_tls: true,
        write_token: Some("a_valid_write_token_5678".to_string()),
        ..Default::default()
    };
    assert!(check_admin_tls_requirement(&config, false).is_err());
}

#[test]
fn test_tls_check_required_and_configured() {
    let config = AuthConfig {
        enabled: true,
        require_tls: true,
        write_token: Some("a_valid_write_token_5678".to_string()),
        ..Default::default()
    };
    assert!(check_admin_tls_requirement(&config, true).is_ok());
}

#[test]
fn test_auth_metrics_unregistered() {
    let metrics = AuthMetrics::new_unregistered();
    metrics.auth_successes_total.inc();
    metrics.auth_failures_total.inc();
    metrics.rate_limited_total.inc();

    assert_eq!(metrics.auth_successes_total.get(), 1);
    assert_eq!(metrics.auth_failures_total.get(), 1);
    assert_eq!(metrics.rate_limited_total.get(), 1);
}

#[test]
fn test_auth_metrics_registered() {
    let registry = Registry::new();
    let metrics = AuthMetrics::new(&registry);
    metrics.auth_successes_total.inc();
    metrics.auth_failures_total.inc_by(5);
    metrics.rate_limited_total.inc_by(3);

    assert_eq!(metrics.auth_successes_total.get(), 1);
    assert_eq!(metrics.auth_failures_total.get(), 5);
    assert_eq!(metrics.rate_limited_total.get(), 3);

    let families = registry.gather();
    let names: Vec<&str> = families.iter().map(|f| f.name()).collect();
    assert!(names.contains(&"wasm_admin_auth_successes_total"));
    assert!(names.contains(&"wasm_admin_auth_failures_total"));
    assert!(names.contains(&"wasm_admin_rate_limited_total"));
}

fn test_auth_router(config: AuthConfig) -> axum::Router {
    let state = AuthState {
        config: Arc::new(RwLock::new(config)),
        metrics: Arc::new(AuthMetrics::new_unregistered()),
        rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
        trusted_proxies: Arc::new(Vec::new()),
        audit_fn: None,
        node_id: "test-node".to_string(),
    };

    axum::Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/livez", get(|| async { "ok" }))
        .route("/metrics", get(|| async { "# metrics\n" }))
        .route("/status/pgbouncer", get(|| async { "pgbouncer ok" }))
        .route("/admin/config", get(|| async { "config ok" }))
        .route("/admin/config", post(|| async { "config updated" }))
        .route("/admin/rebuild", post(|| async { "rebuild ok" }))
        .route("/admin/gc/force", post(|| async { "gc ok" }))
        .route("/admin/auth/rotate-token", post(|| async { "rotated" }))
        .route("/logs/test-app", get(|| async { "log line" }))
        .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
}

#[tokio::test]
async fn test_integration_auth_disabled_allows_all() {
    let config = AuthConfig::default();
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = test_auth_router(AuthConfig::default());
    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/rebuild")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_public_endpoints_no_auth() {
    for path in &["/health", "/healthz", "/readyz", "/livez"] {
        let app = test_auth_router(AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        });
        let req = axum::extract::Request::builder()
            .uri(*path)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "public endpoint {} should not require auth",
            path
        );
    }
}

#[tokio::test]
async fn test_integration_metrics_requires_read_auth() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };

    let request = axum::extract::Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = test_auth_router(config.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let request = axum::extract::Request::builder()
        .uri("/metrics")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let response = test_auth_router(config).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_missing_token_returns_401() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_integration_invalid_token_returns_401() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer wrong_token_value")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_integration_read_token_on_get_returns_200() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_read_token_on_post_returns_403() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/rebuild")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_integration_write_token_on_get_returns_200() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer write_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_write_token_on_post_returns_200() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/rebuild")
        .header("Authorization", "Bearer write_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_write_only_config() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: None,
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer write_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = test_auth_router(AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: None,
        ..Default::default()
    });
    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer some_other_token")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_integration_rate_limit_returns_429() {
    let shared_limiter = Arc::new(AdminRateLimiter::new(1, 2));

    let make_state = || AuthState {
        config: Arc::new(RwLock::new(AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            ..Default::default()
        })),
        metrics: Arc::new(AuthMetrics::new_unregistered()),
        rate_limiter: shared_limiter.clone(),
        trusted_proxies: Arc::new(vec!["127.0.0.1/32".parse().unwrap()]),
        audit_fn: None,
        node_id: "test-node".to_string(),
    };

    for _ in 0..2 {
        let app: axum::Router = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                make_state(),
                auth_middleware,
            ));
        let req = axum::extract::Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer write_token_1234567890")
            .header("X-Real-IP", "10.0.0.1")
            .body(Body::empty())
            .unwrap();
        let mut req = req;
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:4321".parse::<SocketAddr>().unwrap()));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let app: axum::Router = axum::Router::new()
        .route("/admin/config", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            make_state(),
            auth_middleware,
        ));
    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer write_token_1234567890")
        .header("X-Real-IP", "10.0.0.1")
        .body(Body::empty())
        .unwrap();
    let mut req = req;
    req.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:4321".parse::<SocketAddr>().unwrap()));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_integration_token_rotation_updates_config() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("old_write_token_12345".to_string()),
        read_token: Some("old_read_token_12345".to_string()),
        ..Default::default()
    };

    let shared_config = Arc::new(RwLock::new(config));
    let state = AuthState {
        config: shared_config.clone(),
        metrics: Arc::new(AuthMetrics::new_unregistered()),
        rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
        trusted_proxies: Arc::new(Vec::new()),
        audit_fn: None,
        node_id: "test-node".to_string(),
    };

    let app = axum::Router::new()
        .route("/admin/config", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer old_write_token_12345")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    {
        let mut cfg = shared_config.write().await;
        cfg.write_token = Some("new_write_token_67890".to_string());
    }

    let app = axum::Router::new()
        .route("/admin/config", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            AuthState {
                config: shared_config.clone(),
                metrics: Arc::new(AuthMetrics::new_unregistered()),
                rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                trusted_proxies: Arc::new(Vec::new()),
                audit_fn: None,
                node_id: "test-node".to_string(),
            },
            auth_middleware,
        ));
    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer old_write_token_12345")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let app = axum::Router::new()
        .route("/admin/config", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            AuthState {
                config: shared_config.clone(),
                metrics: Arc::new(AuthMetrics::new_unregistered()),
                rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                trusted_proxies: Arc::new(Vec::new()),
                audit_fn: None,
                node_id: "test-node".to_string(),
            },
            auth_middleware,
        ));
    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer new_write_token_67890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let app = axum::Router::new()
        .route("/admin/config", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            AuthState {
                config: shared_config,
                metrics: Arc::new(AuthMetrics::new_unregistered()),
                rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                trusted_proxies: Arc::new(Vec::new()),
                audit_fn: None,
                node_id: "test-node".to_string(),
            },
            auth_middleware,
        ));
    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Bearer old_read_token_12345")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_integration_all_mutation_endpoints_require_write() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };

    let app = test_auth_router(config.clone());
    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/rebuild")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let app = test_auth_router(config.clone());
    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/gc/force")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let app = test_auth_router(config);
    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/auth/rotate-token")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_integration_malformed_auth_header() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_integration_response_body_contains_error_json() {
    let config = AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    };
    let app = test_auth_router(config);

    let req = axum::extract::Request::builder()
        .uri("/admin/config")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "unauthorized");

    let app = test_auth_router(AuthConfig {
        enabled: true,
        write_token: Some("write_token_1234567890".to_string()),
        read_token: Some("read_token_1234567890".to_string()),
        ..Default::default()
    });
    let req = axum::extract::Request::builder()
        .method("POST")
        .uri("/admin/rebuild")
        .header("Authorization", "Bearer read_token_1234567890")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "forbidden");
}
