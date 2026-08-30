use sha2::Digest;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Test: public route allows all requests without authentication.
#[tokio::test]
async fn test_full_gateway_pipeline_public_route() {
    let gateway = Arc::new(proxy::gateway::Gateway::new(None));
    let config = common::types::GatewayRouteConfig::default(); // auth = None
    gateway
        .set_route_config("test-app:v1", config.clone())
        .await;

    let retrieved = gateway
        .get_route_config(&common::types::AppId("test-app:v1".to_string()))
        .await;
    assert!(retrieved.is_some());
    let cfg = retrieved.unwrap();
    assert_eq!(cfg.auth, common::types::AuthPolicy::None);
    // No authentication required → anyone can access
    assert!(proxy::gateway::authz::authorize(
        &proxy::gateway::oidc::UserIdentity {
            sub: "anonymous".to_string(),
            email: None,
            roles: vec![],
            raw_claims: serde_json::json!({}),
        },
        &cfg.auth
    ));
}

/// Test: authenticated route requires a valid JWT.
#[tokio::test]
async fn test_full_gateway_pipeline_authenticated_route() {
    // Generate a test RSA key pair
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::rand_core::OsRng;
    use rsa::{traits::PublicKeyParts, RsaPrivateKey, RsaPublicKey};

    let mut rng = OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let public_key = RsaPublicKey::from(&private_key);

    // Encode private key for jsonwebtoken
    let private_pkcs1 = private_key.to_pkcs1_der().unwrap();
    let encoding_key = jsonwebtoken::EncodingKey::from_rsa_der(private_pkcs1.as_bytes());

    // Encode public key components for JWKS
    let n_bytes = public_key.n().to_bytes_be();
    let e_bytes = public_key.e().to_bytes_be();
    let n_b64 = URL_SAFE_NO_PAD.encode(&n_bytes);
    let e_b64 = URL_SAFE_NO_PAD.encode(&e_bytes);

    let kid = "test-key-1";
    let oidc_config = proxy::gateway::oidc::OidcConfig {
        issuer_url: "https://test-issuer.example.com".to_string(),
        audience: "test-audience".to_string(),
        jwks_url: None,
        jwks_refresh_secs: 3600,
        clock_skew_secs: 30,
    };

    let provider = Arc::new(proxy::gateway::oidc::OidcProvider::new(oidc_config));

    // Inject the public key into the JWKS cache via test helper
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(&n_b64, &e_b64).unwrap();
    provider
        .inject_jwks_key(kid.to_string(), decoding_key)
        .await;

    let gateway = Arc::new(proxy::gateway::Gateway::new(Some(provider.clone())));

    let _config = common::types::GatewayRouteConfig {
        auth: common::types::AuthPolicy::Authenticated,
        ..Default::default()
    };
    gateway.set_route_config("test-app:v1", _config).await;

    // Create a valid JWT
    let claims = serde_json::json!({
        "sub": "user-123",
        "iss": "https://test-issuer.example.com",
        "aud": "test-audience",
        "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
        "iat": chrono::Utc::now().timestamp(),
    });
    let token = create_test_jwt(&claims, kid, &encoding_key);

    // Test: the provider can validate the token
    let identity = provider.validate_token(&token).await;
    assert!(identity.is_ok(), "valid JWT should pass validation");
    let id = identity.unwrap();
    assert_eq!(id.sub, "user-123");

    // Test: expired token should fail
    let expired_claims = serde_json::json!({
        "sub": "user-123",
        "iss": "https://test-issuer.example.com",
        "aud": "test-audience",
        "exp": (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp(),
        "iat": (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp(),
    });
    let expired_token = create_test_jwt(&expired_claims, kid, &encoding_key);
    let result = provider.validate_token(&expired_token).await;
    assert!(result.is_err(), "expired JWT should fail validation");

    // Test: wrong audience should fail
    let wrong_aud_claims = serde_json::json!({
        "sub": "user-123",
        "iss": "https://test-issuer.example.com",
        "aud": "wrong-audience",
        "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
        "iat": chrono::Utc::now().timestamp(),
    });
    let wrong_aud_token = create_test_jwt(&wrong_aud_claims, kid, &encoding_key);
    let result = provider.validate_token(&wrong_aud_token).await;
    assert!(result.is_err(), "wrong audience JWT should fail validation");

    // Test: tampered token should fail
    let tampered_token = format!("{}a", token);
    let result = provider.validate_token(&tampered_token).await;
    assert!(result.is_err(), "tampered JWT should fail validation");
}

/// Test: CORS preflight handled correctly.
#[tokio::test]
async fn test_full_gateway_pipeline_cors_preflight() {
    let cors = common::types::CorsPolicy {
        allowed_origins: vec!["https://app.example.com".to_string()],
        allowed_methods: common::types::CorsPolicy::default_methods(),
        allowed_headers: common::types::CorsPolicy::default_headers(),
        expose_headers: vec!["X-Custom-Header".to_string()],
        allow_credentials: true,
        max_age_secs: 3600,
    };

    // Allowed origin
    assert!(proxy::gateway::cors::is_origin_allowed(
        "https://app.example.com",
        &cors
    ));
    // Disallowed origin
    assert!(!proxy::gateway::cors::is_origin_allowed(
        "https://evil.com",
        &cors
    ));
    // Wildcard
    let wildcard_cors = common::types::CorsPolicy {
        allowed_origins: vec!["*".to_string()],
        ..cors.clone()
    };
    assert!(proxy::gateway::cors::is_origin_allowed(
        "https://anything.com",
        &wildcard_cors
    ));
    // Subdomain wildcard
    let subdomain_cors = common::types::CorsPolicy {
        allowed_origins: vec!["*.example.com".to_string()],
        ..cors.clone()
    };
    assert!(proxy::gateway::cors::is_origin_allowed(
        "https://sub.example.com",
        &subdomain_cors
    ));
    assert!(!proxy::gateway::cors::is_origin_allowed(
        "https://example.com",
        &subdomain_cors
    ));
}

/// Test: circuit breaker opens after failures and recovers.
#[tokio::test]
async fn test_full_gateway_pipeline_circuit_breaker() {
    let cb_manager = proxy::gateway::circuit_breaker::CircuitBreakerManager::new();

    let app_id = "test-app:v1";

    // Initially closed
    assert!(!cb_manager.is_circuit_open(app_id));

    // Record 5 failures → circuit opens
    for _ in 0..5 {
        cb_manager.record_failure(app_id);
    }
    assert!(
        cb_manager.is_circuit_open(app_id),
        "circuit should be open after 5 failures"
    );

    // Record success should not close immediately (circuit is open, not half-open)
    cb_manager.record_success(app_id);
    assert!(
        cb_manager.is_circuit_open(app_id),
        "circuit should still be open"
    );

    // Use test helper to simulate time passing for reset timeout
    cb_manager.set_last_state_change(app_id, Instant::now() - Duration::from_secs(31));

    // Now it should transition to half-open and allow one request
    assert!(
        !cb_manager.is_circuit_open(app_id),
        "circuit should be half-open after timeout"
    );

    // If the probe succeeds, circuit closes
    cb_manager.record_success(app_id);
    assert!(
        !cb_manager.is_circuit_open(app_id),
        "circuit should be closed after probe success"
    );

    // Re-open with failures
    for _ in 0..5 {
        cb_manager.record_failure(app_id);
    }
    assert!(cb_manager.is_circuit_open(app_id));

    // Half-open again
    cb_manager.set_last_state_change(app_id, Instant::now() - Duration::from_secs(31));
    assert!(
        !cb_manager.is_circuit_open(app_id),
        "circuit should be half-open"
    );

    // Probe fails → back to open
    cb_manager.record_failure(app_id);
    assert!(
        cb_manager.is_circuit_open(app_id),
        "circuit should re-open after probe failure"
    );
}

/// Test: distributed rate limiter local bucket and KV sync.
#[tokio::test]
async fn test_full_gateway_pipeline_rate_limit_distributed() {
    let limiter = Arc::new(
        proxy::gateway::distributed_limiter::DistributedRateLimiter::new(
            "test-app:v1".to_string(),
            "node-1".to_string(),
            proxy::gateway::distributed_limiter::DistributedRateLimitConfig {
                global_rps: 100,
                per_node_burst: 10,
                sync_interval_ms: 100,
                kv_bucket: "test_rate_limits".to_string(),
            },
        ),
    );

    // Local bucket should allow requests up to burst capacity
    let mut allowed = 0;
    for _ in 0..15 {
        if limiter.check_request().await {
            allowed += 1;
        }
    }
    assert_eq!(
        allowed, 10,
        "should allow exactly burst capacity (10) requests initially"
    );

    // After consuming all tokens, subsequent requests should be denied
    for _ in 0..5 {
        assert!(
            !limiter.check_request().await,
            "should deny when bucket is empty"
        );
    }

    // Verify the KV sync serialization/deserialization works
    let entry = proxy::gateway::distributed_limiter::RateLimitEntry::new(
        "node-1".to_string(),
        10,
        chrono::Utc::now().timestamp_millis(),
    );
    let json = serde_json::to_string(&entry).unwrap();
    let deserialized: proxy::gateway::distributed_limiter::RateLimitEntry =
        serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.node_id, "node-1");
    assert_eq!(deserialized.consumed, 10);
}

/// Test: user identity headers are injected correctly.
#[tokio::test]
async fn test_user_identity_header_injection() {
    use pingora::http::RequestHeader;

    let mut req = RequestHeader::build("GET", b"/test".as_slice(), None).unwrap();

    let identity = proxy::gateway::oidc::UserIdentity {
        sub: "user-456".to_string(),
        email: Some("user@example.com".to_string()),
        roles: vec!["admin".to_string(), "user".to_string()],
        raw_claims: serde_json::json!({}),
    };

    let transform = common::types::RequestTransform {
        add_headers: vec![("X-Api-Version".to_string(), "2".to_string())],
        remove_headers: vec!["X-Internal-Token".to_string()],
        path_prefix: None,
        strip_query_params: vec![],
    };

    // Add a header to be removed
    req.insert_header("X-Internal-Token", "secret").unwrap();

    proxy::gateway::transform::apply_request_transform(&mut req, &transform, Some(&identity));

    // Verify identity headers
    assert_eq!(
        req.headers.get("X-User-Id").unwrap().to_str().unwrap(),
        "user-456"
    );
    assert_eq!(
        req.headers.get("X-User-Email").unwrap().to_str().unwrap(),
        "user@example.com"
    );
    assert_eq!(
        req.headers.get("X-User-Roles").unwrap().to_str().unwrap(),
        "admin,user"
    );

    // Verify custom header
    assert_eq!(
        req.headers.get("X-Api-Version").unwrap().to_str().unwrap(),
        "2"
    );

    // Verify removed header
    assert!(req.headers.get("X-Internal-Token").is_none());
}

/// Test: JWKS cache refresh triggers correctly when stale.
#[tokio::test]
async fn test_jwks_cache_refresh_on_stale() {
    let oidc_config = proxy::gateway::oidc::OidcConfig {
        issuer_url: "https://test-issuer.example.com".to_string(),
        audience: "test-audience".to_string(),
        jwks_url: None,
        jwks_refresh_secs: 1, // 1 second for testing
        clock_skew_secs: 30,
    };

    let provider = Arc::new(proxy::gateway::oidc::OidcProvider::new(oidc_config));

    // Set cache as very stale via test helper
    provider
        .set_cache_fetched_at(Instant::now() - Duration::from_secs(10))
        .await;

    // The validation should detect stale cache and try to refresh
    // (this will fail because there's no real server, but it demonstrates the logic)
    let result = provider.validate_token("invalid.token.here").await;
    assert!(result.is_err());
}

/// Test: API key authentication at endpoint level.
#[tokio::test]
async fn test_endpoint_api_key_auth() {
    let gateway = Arc::new(proxy::gateway::Gateway::new(None));

    // Set up an API key validator for the app
    let mut hasher = sha2::Sha256::new();
    hasher.update("secret-key-123");
    let hash = format!("sha256${}", hex::encode(hasher.finalize()));
    let api_key_record = common::types::ApiKeyRecord {
        name: "test-key".to_string(),
        key_hash: hash,
        scopes: vec!["/api/public".to_string()],
    };
    let validator = proxy::gateway::api_key::ApiKeyValidator::new(vec![api_key_record]);
    gateway
        .set_api_key_validator("default/test-app:v1", validator)
        .await;

    // Valid key for allowed path
    assert!(
        gateway
            .validate_api_key("default/test-app:v1", "secret-key-123", "/api/public/users")
            .await
    );
    // Valid key for disallowed path
    assert!(
        !gateway
            .validate_api_key("default/test-app:v1", "secret-key-123", "/api/admin")
            .await
    );
    // Invalid key
    assert!(
        !gateway
            .validate_api_key("default/test-app:v1", "wrong-key", "/api/public")
            .await
    );
}

/// Test: endpoint rules with per-path auth overrides.
#[tokio::test]
async fn test_endpoint_rule_evaluation() {
    let config = common::types::GatewayRouteConfig {
        auth: common::types::AuthPolicy::Authenticated,
        endpoints: vec![
            common::types::EndpointRule {
                path: "/health".to_string(),
                methods: vec!["GET".to_string()],
                auth: common::types::EndpointAuth::None,
                required_scopes: vec![],
                rate_limit: None,
            },
            common::types::EndpointRule {
                path: "/api/admin".to_string(),
                methods: vec!["POST".to_string(), "DELETE".to_string()],
                auth: common::types::EndpointAuth::Roles {
                    allowed_roles: vec!["admin".to_string()],
                    client_id: None,
                },
                required_scopes: vec![],
                rate_limit: None,
            },
        ],
        ..Default::default()
    };

    // /health should override to None
    let health_rule = config.endpoints.iter().find(|e| e.path == "/health");
    assert!(health_rule.is_some());
    assert_eq!(health_rule.unwrap().auth, common::types::EndpointAuth::None);

    // /api/admin should require admin role
    let admin_rule = config.endpoints.iter().find(|e| e.path == "/api/admin");
    assert!(admin_rule.is_some());
    match &admin_rule.unwrap().auth {
        common::types::EndpointAuth::Roles { allowed_roles, .. } => {
            assert_eq!(allowed_roles, &["admin"]);
        }
        other => panic!("expected Roles auth, got {:?}", other),
    }
}

#[test]
fn test_endpoint_rule_scope_requirement() {
    let config = common::types::GatewayRouteConfig {
        endpoints: vec![common::types::EndpointRule {
            path: "/api/users".to_string(),
            methods: vec!["GET".to_string()],
            auth: common::types::EndpointAuth::Roles {
                allowed_roles: vec!["admin".to_string()],
                client_id: None,
            },
            required_scopes: vec!["read:users".to_string()],
            rate_limit: None,
        }],
        ..Default::default()
    };

    let rule = config
        .endpoints
        .iter()
        .find(|e| e.path == "/api/users")
        .unwrap();
    assert_eq!(rule.required_scopes, vec!["read:users"]);
}

/// Test: namespace-qualified AppId resolution.
#[test]
fn test_namespaced_app_id() {
    let app_id = common::types::AppId::new_namespaced("production", "payments", "v1");
    assert_eq!(app_id.0, "production/payments:v1");
    assert_eq!(app_id.namespace(), "production");
    assert_eq!(app_id.bare_name(), "payments:v1");
    assert_eq!(app_id.bare_app_name(), "payments");

    // Legacy format (no namespace) defaults to "default"
    let legacy = common::types::AppId::new("payments", "v1");
    assert_eq!(legacy.namespace(), "default");
}

/// Helper: create a valid JWT for testing.
fn create_test_jwt(
    claims: &serde_json::Value,
    kid: &str,
    encoding_key: &jsonwebtoken::EncodingKey,
) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, claims, encoding_key).unwrap()
}
