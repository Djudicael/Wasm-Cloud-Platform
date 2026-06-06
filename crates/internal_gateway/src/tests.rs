use super::*;
use axum::body::Body;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use http::HeaderValue;
use hyper::service::service_fn;
use jsonwebtoken::{DecodingKey, EncodingKey};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::rand_core::OsRng;
use rsa::{traits::PublicKeyParts, RsaPrivateKey, RsaPublicKey};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::net::TcpListener;

async fn setup_gateway() -> (Arc<InternalGateway>, common::types::AppId) {
    let registry = Arc::new(supervisor::network::NamespaceRegistry::default());
    let app_id = common::types::AppId::new_namespaced("default", "target", "v1");

    registry
        .register(
            &app_id,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 10101),
        )
        .await;

    let gw = Arc::new(InternalGateway::new(
        registry,
        Arc::new(proxy::rate_limiter::RateLimiter::new(
            proxy::rate_limiter::RateLimitConfig::default(),
        )),
        Arc::new(proxy::gateway::circuit_breaker::CircuitBreakerManager::new()),
        Arc::new(proxy::gateway::Gateway::new(None)),
    ));

    (gw, app_id)
}

async fn setup_gateway_with(
    gateway_config: Arc<proxy::gateway::Gateway>,
    endpoint: supervisor::network::RegisteredEndpoint,
) -> (Arc<InternalGateway>, common::types::AppId) {
    let registry = Arc::new(supervisor::network::NamespaceRegistry::default());
    let app_id = common::types::AppId::new_namespaced("default", "target", "v1");
    let namespace_map = Arc::new(ebpf_monitor::NamespaceMap::new_fallback());

    registry.register_endpoint(&app_id, endpoint).await;
    namespace_map
        .register_tid(
            4242,
            ebpf_monitor::common::TidIdentity::new("default", "caller:v1"),
        )
        .unwrap();
    namespace_map.bind_port(54321, 4242);

    let gw = Arc::new(
        InternalGateway::new(
            registry,
            Arc::new(proxy::rate_limiter::RateLimiter::new(
                proxy::rate_limiter::RateLimitConfig::default(),
            )),
            Arc::new(proxy::gateway::circuit_breaker::CircuitBreakerManager::new()),
            gateway_config,
        )
        .with_namespace_map(namespace_map)
        .with_ebpf_active(true),
    );

    (gw, app_id)
}

async fn test_gateway_with_provider() -> (Arc<proxy::gateway::Gateway>, String, String) {
    let mut rng = OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let public_key = RsaPublicKey::from(&private_key);

    let private_pkcs1 = private_key.to_pkcs1_der().unwrap();
    let encoding_key = EncodingKey::from_rsa_der(private_pkcs1.as_bytes());

    let n_b64 = URL_SAFE_NO_PAD.encode(public_key.n().to_bytes_be());
    let e_b64 = URL_SAFE_NO_PAD.encode(public_key.e().to_bytes_be());

    let provider = Arc::new(proxy::gateway::oidc::OidcProvider::new(
        common::types::OidcConfig {
            issuer_url: "https://test-issuer.example.com".to_string(),
            audience: "test-audience".to_string(),
            jwks_refresh_secs: 3600,
            clock_skew_secs: 30,
        },
    ));

    provider
        .inject_jwks_key(
            "test-key-1".to_string(),
            DecodingKey::from_rsa_components(&n_b64, &e_b64).unwrap(),
        )
        .await;

    let gateway = Arc::new(proxy::gateway::Gateway::new(Some(provider)));

    let with_scope = create_test_jwt(
        &serde_json::json!({
            "sub": "user-123",
            "iss": "https://test-issuer.example.com",
            "aud": "test-audience",
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            "iat": chrono::Utc::now().timestamp(),
            "realm_access": { "roles": ["admin"] },
            "scope": "admin:users read:users",
        }),
        "test-key-1",
        &encoding_key,
    );

    let missing_scope = create_test_jwt(
        &serde_json::json!({
            "sub": "user-123",
            "iss": "https://test-issuer.example.com",
            "aud": "test-audience",
            "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            "iat": chrono::Utc::now().timestamp(),
            "realm_access": { "roles": ["admin"] },
            "scope": "read:users",
        }),
        "test-key-1",
        &encoding_key,
    );

    (gateway, with_scope, missing_scope)
}

fn create_test_jwt(claims: &serde_json::Value, kid: &str, encoding_key: &EncodingKey) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, claims, encoding_key).unwrap()
}

async fn spawn_h2c_test_server() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let service = service_fn(|req: Request<Incoming>| async move {
            assert_eq!(
                req.headers()
                    .get("x-user-id")
                    .and_then(|value| value.to_str().ok()),
                Some("user-123")
            );
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(axum::body::Body::from("ok"))
                    .unwrap(),
            )
        });

        hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(io, service)
            .await
            .unwrap();
    });

    addr
}

async fn spawn_h1_test_server(max_requests: usize) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        for _ in 0..max_requests {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let service = service_fn(|_req: Request<Incoming>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(axum::body::Body::from("ok"))
                        .unwrap(),
                )
            });

            tokio::spawn(async move {
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await
                    .unwrap();
            });
        }
    });

    addr
}

#[test]
fn test_parse_internal_host() {
    assert_eq!(
        parse_internal_host("echo-service.default.internal"),
        Some(("echo-service", "default"))
    );
    assert_eq!(
        parse_internal_host("api.production.internal"),
        Some(("api", "production"))
    );
    assert_eq!(
        parse_internal_host("bare-app.internal"),
        Some(("bare-app", "default"))
    );
    assert_eq!(parse_internal_host("invalid"), None);

    // With port number
    assert_eq!(
        parse_internal_host("echo-service.default.internal:9080"),
        Some(("echo-service", "default"))
    );
    assert_eq!(
        parse_internal_host("api.production.internal:9082"),
        Some(("api", "production"))
    );

    // App names with dots
    assert_eq!(
        parse_internal_host("my.api-service.production.internal"),
        Some(("my.api-service", "production"))
    );
    assert_eq!(
        parse_internal_host("a.b.c.staging.internal:9080"),
        Some(("a.b.c", "staging"))
    );
}

#[tokio::test]
async fn test_internal_gateway_creation() {
    let (gw, _app_id) = setup_gateway().await;
    assert!(!gw.circuit_breaker.is_circuit_open("test"));
}

#[test]
fn test_strip_internal_identity_headers_removes_forged_values() {
    let mut headers = HeaderMap::new();
    headers.insert("x-namespace", HeaderValue::from_static("forged"));
    headers.insert("x-source-app", HeaderValue::from_static("evil-app"));
    headers.insert("x-source-tid", HeaderValue::from_static("999"));
    headers.insert("host", HeaderValue::from_static("target.default.internal"));

    strip_internal_identity_headers(&mut headers);

    assert!(headers.get("x-namespace").is_none());
    assert!(headers.get("x-source-app").is_none());
    assert!(headers.get("x-source-tid").is_none());
    assert_eq!(
        headers.get("host").and_then(|v| v.to_str().ok()),
        Some("target.default.internal")
    );
}

#[tokio::test]
async fn test_proxy_handler_rejects_forged_internal_identity_headers() {
    let (base_gw, _app_id) = setup_gateway().await;
    let namespace_map = Arc::new(ebpf_monitor::NamespaceMap::new_fallback());
    namespace_map
        .register_tid(
            4242,
            ebpf_monitor::common::TidIdentity::new("staging", "caller:v1"),
        )
        .unwrap();
    namespace_map.bind_port(54321, 4242);

    let gw = Arc::new(
        InternalGateway::new(
            base_gw.registry.clone(),
            base_gw.rate_limiter.clone(),
            base_gw.circuit_breaker.clone(),
            base_gw.gateway_config.clone(),
        )
        .with_namespace_map(namespace_map)
        .with_ebpf_active(true),
    );

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("host", "target.default.internal")
        .header("x-namespace", "default")
        .header("x-source-app", "forged:v1")
        .header("x-source-tid", "99999")
        .body(Body::empty())
        .unwrap();

    let result = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req,
    )
    .await;

    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

#[test]
fn test_endpoint_auth_policy_inherits_route_default() {
    assert_eq!(
        endpoint_auth_policy(
            &common::types::AuthPolicy::Authenticated,
            &common::types::EndpointAuth::Inherit,
        ),
        Some(common::types::AuthPolicy::Authenticated)
    );
}

#[tokio::test]
async fn test_internal_gateway_rejects_missing_bearer_token_for_route_auth() {
    let gateway = Arc::new(proxy::gateway::Gateway::new(None));
    gateway
        .set_route_config(
            "default/target:v1",
            common::types::GatewayRouteConfig {
                auth: common::types::AuthPolicy::Authenticated,
                ..Default::default()
            },
        )
        .await;

    let (gw, _app_id) = setup_gateway_with(
        gateway,
        supervisor::network::RegisteredEndpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10101),
            h2c: false,
        },
    )
    .await;

    let req = Request::builder()
        .method("GET")
        .uri("/users")
        .header("host", "target.default.internal")
        .body(Body::empty())
        .unwrap();

    let result = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req,
    )
    .await;

    assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_internal_gateway_enforces_endpoint_scopes() {
    let (gateway, _good_token, missing_scope_token) = test_gateway_with_provider().await;
    gateway
        .set_route_config(
            "default/target:v1",
            common::types::GatewayRouteConfig {
                endpoints: vec![common::types::EndpointRule {
                    path: "/api/admin".to_string(),
                    methods: vec!["POST".to_string()],
                    auth: common::types::EndpointAuth::Roles {
                        allowed_roles: vec!["admin".to_string()],
                        client_id: None,
                    },
                    required_scopes: vec!["admin:users".to_string()],
                    rate_limit: None,
                }],
                ..Default::default()
            },
        )
        .await;

    let (gw, _app_id) = setup_gateway_with(
        gateway,
        supervisor::network::RegisteredEndpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 10101),
            h2c: false,
        },
    )
    .await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/admin")
        .header("host", "target.default.internal")
        .header("authorization", format!("Bearer {missing_scope_token}"))
        .body(Body::empty())
        .unwrap();

    let result = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req,
    )
    .await;

    assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_internal_gateway_forwards_h2c_with_identity_headers() {
    let upstream_addr = spawn_h2c_test_server().await;
    let (gateway, good_token, _missing_scope_token) = test_gateway_with_provider().await;
    gateway
        .set_route_config(
            "default/target:v1",
            common::types::GatewayRouteConfig {
                auth: common::types::AuthPolicy::Authenticated,
                ..Default::default()
            },
        )
        .await;

    let (gw, _app_id) = setup_gateway_with(
        gateway,
        supervisor::network::RegisteredEndpoint {
            addr: upstream_addr,
            h2c: true,
        },
    )
    .await;

    assert_eq!(
        endpoint_auth_policy(
            &common::types::AuthPolicy::Authenticated,
            &common::types::EndpointAuth::ApiKey,
        ),
        None
    );

    let req = Request::builder()
        .method("GET")
        .uri("/grpc")
        .header("host", "target.default.internal")
        .header("authorization", format!("Bearer {good_token}"))
        .body(Body::empty())
        .unwrap();

    let resp = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req,
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn test_internal_gateway_rejects_oversized_request_body_from_content_length() {
    let (gw, _app_id) = setup_gateway().await;
    let namespace_map = Arc::new(ebpf_monitor::NamespaceMap::new_fallback());
    namespace_map
        .register_tid(
            4242,
            ebpf_monitor::common::TidIdentity::new("default", "caller:v1"),
        )
        .unwrap();
    namespace_map.bind_port(54321, 4242);
    let gw = Arc::new(
        InternalGateway::new(
            gw.registry.clone(),
            gw.rate_limiter.clone(),
            gw.circuit_breaker.clone(),
            gw.gateway_config.clone(),
        )
        .with_namespace_map(namespace_map)
        .with_ebpf_active(true)
        .with_max_body_size_bytes(16),
    );

    let req = Request::builder()
        .method("POST")
        .uri("/upload")
        .header("host", "target.default.internal")
        .header("content-length", "32")
        .body(Body::empty())
        .unwrap();

    let result = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req,
    )
    .await;

    assert_eq!(result.unwrap_err(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_internal_gateway_enforces_endpoint_rate_limit() {
    let upstream_addr = spawn_h1_test_server(2).await;
    let gateway = Arc::new(proxy::gateway::Gateway::new(None));
    gateway
        .set_route_config(
            "default/target:v1",
            common::types::GatewayRouteConfig {
                endpoints: vec![common::types::EndpointRule {
                    path: "/echo".to_string(),
                    methods: vec!["GET".to_string()],
                    auth: common::types::EndpointAuth::None,
                    required_scopes: vec![],
                    rate_limit: Some(common::types::RouteRateLimit {
                        requests_per_second: 1,
                        burst_capacity: 1,
                        distributed: false,
                    }),
                }],
                ..Default::default()
            },
        )
        .await;

    let (gw, _app_id) = setup_gateway_with(
        gateway,
        supervisor::network::RegisteredEndpoint {
            addr: upstream_addr,
            h2c: false,
        },
    )
    .await;

    let req1 = Request::builder()
        .method("GET")
        .uri("/echo")
        .header("host", "target.default.internal")
        .body(Body::empty())
        .unwrap();
    let resp1 = proxy_handler(
        State(gw.clone()),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req1,
    )
    .await
    .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    let req2 = Request::builder()
        .method("GET")
        .uri("/echo")
        .header("host", "target.default.internal")
        .body(Body::empty())
        .unwrap();
    let resp2 = proxy_handler(
        State(gw),
        ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 54321)),
        req2,
    )
    .await;

    assert_eq!(resp2.unwrap_err(), StatusCode::TOO_MANY_REQUESTS);
}
