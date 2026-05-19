//! Admin API authentication middleware.
//!
//! This module implements bearer-token authentication with separate read/write
//! permission levels, per-IP rate limiting, and Prometheus metrics for the
//! admin API.
//!
//! # Architecture
//!
//! The middleware runs as an Axum layer before any admin handler:
//!
//! ```text
//! Request → rate limit check → auth check → permission check → handler
//! ```
//!
//! Public endpoints (`/health`, `/status/metrics`) bypass authentication entirely
//! so that load balancers and Prometheus can probe the node without credentials.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use common::auth::{AuthConfig, Permission, TokenType};
use prometheus::{IntCounter, Opts, Registry};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// ── Audit Callback ────────────────────────────────────────────────────────────

/// Information about an admin API action for audit logging.
#[derive(Debug, Clone)]
pub struct AuditInfo {
    pub path: String,
    pub method: String,
    pub token_type: TokenType,
    pub client_ip: Option<IpAddr>,
    pub status_code: u16,
    pub node_id: String,
}

/// Callback type for audit logging.
///
/// The node crate provides an implementation that writes to the audit trail
/// via `supervisor::audit::write_audit_event`. This keeps the proxy crate
/// independent of the supervisor crate (avoiding circular dependencies).
pub type AuditCallback = Arc<dyn Fn(AuditInfo) + Send + Sync>;

// ── Auth State ────────────────────────────────────────────────────────────────

/// Shared state for the authentication middleware.
///
/// Held in an Axum `State` extractor and shared across all middleware invocations.
/// The `config` field is wrapped in `RwLock` to support hot-reloadable token rotation.
#[derive(Clone)]
pub struct AuthState {
    /// Current auth configuration (hot-reloadable for token rotation).
    pub config: Arc<RwLock<AuthConfig>>,

    /// Metrics for authentication events.
    pub metrics: Arc<AuthMetrics>,

    /// Admin API rate limiter (per IP).
    pub rate_limiter: Arc<AdminRateLimiter>,

    /// Optional audit callback. When set, successful admin API calls are logged.
    pub audit_fn: Option<AuditCallback>,

    /// Node identifier for audit logging.
    pub node_id: String,
}

// ── Auth Middleware ───────────────────────────────────────────────────────────

/// Axum middleware that checks authentication on admin API requests.
///
/// Flow:
/// 1. Public endpoints bypass all checks
/// 2. Rate limit check (before auth to prevent brute-force)
/// 3. Authenticate the request (bearer token comparison)
/// 4. Check authorization (permission level vs. required level)
/// 5. Log successful access
/// 6. Proceed to the handler
pub async fn auth_middleware(
    State(state): State<AuthState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // 1. Check if this path requires authentication
    if is_public_endpoint(&path) {
        return next.run(request).await;
    }

    // 2. Rate limit check (before auth to prevent brute-force)
    let client_ip = extract_client_ip(&headers);
    if !state.rate_limiter.allow(client_ip) {
        state.metrics.rate_limited_total.inc();
        tracing::warn!(
            path = %path,
            method = %method,
            ip = ?client_ip,
            "admin API rate limit exceeded"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "rate_limited",
                "message": "admin API rate limit exceeded, slow down"
            })),
        )
            .into_response();
    }

    // 3. Authenticate the request
    let auth_header = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let config = state.config.read().await;
    let auth_result = config.authenticate(auth_header);

    // 4. Check authorization (permission level vs. required level)
    let required = required_permission(&method, &path);
    if auth_result.permission < required {
        state.metrics.auth_failures_total.inc();

        match auth_result.permission {
            Permission::None => {
                tracing::warn!(
                    path = %path,
                    method = %method,
                    ip = ?client_ip,
                    "admin API authentication failed — invalid or missing token"
                );

                // Audit log for auth failure
                if let Some(ref audit_fn) = state.audit_fn {
                    audit_fn(AuditInfo {
                        path: path.clone(),
                        method: method.to_string(),
                        token_type: auth_result.token_type,
                        client_ip,
                        status_code: 401,
                        node_id: state.node_id.clone(),
                    });
                }

                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": "unauthorized",
                        "message": "valid Bearer token required via Authorization header"
                    })),
                )
                    .into_response();
            }
            Permission::Read => {
                tracing::warn!(
                    path = %path,
                    method = %method,
                    ip = ?client_ip,
                    "admin API authorization failed — read token used for write operation"
                );

                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "forbidden",
                        "message": "insufficient permissions — read token cannot perform write operations"
                    })),
                )
                    .into_response();
            }
            Permission::Write => {
                // Should not reach here (Write >= any required level)
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "forbidden",
                        "message": "insufficient permissions"
                    })),
                )
                    .into_response();
            }
        }
    }

    // 5. Log successful access
    tracing::debug!(
        path = %path,
        method = %method,
        token_type = %auth_result.token_type,
        "admin API request authenticated"
    );

    state.metrics.auth_successes_total.inc();

    // Audit log for successful admin action
    if let Some(ref audit_fn) = state.audit_fn {
        audit_fn(AuditInfo {
            path: path.clone(),
            method: method.to_string(),
            token_type: auth_result.token_type,
            client_ip,
            status_code: 200,
            node_id: state.node_id.clone(),
        });
    }

    // 6. Proceed to the handler
    drop(config);
    next.run(request).await
}

/// Determine if a path is public (no authentication required).
///
/// Public endpoints are used by load balancers and Prometheus and must
/// be accessible without credentials.
pub fn is_public_endpoint(path: &str) -> bool {
    matches!(
        path,
        "/health"
            | "/healthz"
            | "/readyz"
            | "/livez"
            | "/_platform/health"
            | "/status/metrics"
            | "/favicon.ico"
    ) || path.starts_with("/status/metrics")
}

/// Determine the minimum permission level required for a request.
///
/// - All GET/HEAD/OPTIONS requests require at least `Read`
/// - All mutation methods (POST, PATCH, DELETE, PUT) require `Write`
pub fn required_permission(method: &axum::http::Method, _path: &str) -> Permission {
    if method == axum::http::Method::GET
        || method == axum::http::Method::HEAD
        || method == axum::http::Method::OPTIONS
    {
        Permission::Read
    } else {
        Permission::Write
    }
}

// Re-export axum::http for use in this module's public API
pub use axum::http;

/// Extract the client IP from request headers.
///
/// Checks `X-Forwarded-For` (first IP) and `X-Real-IP` headers.
/// Returns `None` if no IP header is found (the rate limiter will
/// allow requests without IP info to avoid blocking legitimate traffic).
pub fn extract_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    // Check X-Forwarded-For (first IP in the list)
    if let Some(xff) = headers.get("X-Forwarded-For") {
        if let Ok(val) = xff.to_str() {
            if let Some(first) = val.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }

    // Check X-Real-IP
    if let Some(xri) = headers.get("X-Real-IP") {
        if let Ok(val) = xri.to_str() {
            if let Ok(ip) = val.parse() {
                return Some(ip);
            }
        }
    }

    None
}

// ── Admin API Rate Limiter ────────────────────────────────────────────────────

/// Per-IP rate limiter for the admin API.
///
/// Uses a simple token bucket algorithm. Each IP gets its own bucket
/// with a configurable refill rate and burst capacity.
///
/// Default: 10 requests/second per IP, burst of 20.
/// This makes brute-force token guessing infeasible (16^64 possible values
/// at 10 guesses/second ≈ 10^57 years).
pub struct AdminRateLimiter {
    /// Per-IP token buckets.
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,

    /// Tokens added per second.
    refill_rate: f64,

    /// Maximum tokens (burst capacity).
    max_tokens: f64,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl AdminRateLimiter {
    /// Create a new rate limiter with the given rate and burst.
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        AdminRateLimiter {
            buckets: Mutex::new(HashMap::new()),
            refill_rate: rate_per_second as f64,
            max_tokens: burst as f64,
        }
    }

    /// Create a rate limiter that always allows requests (disabled).
    pub fn disabled() -> Self {
        // Very high limits effectively disable rate limiting
        AdminRateLimiter {
            buckets: Mutex::new(HashMap::new()),
            refill_rate: 1_000_000.0,
            max_tokens: 1_000_000.0,
        }
    }

    /// Check if a request from the given IP is allowed.
    ///
    /// Returns `true` if allowed, `false` if rate-limited.
    /// Requests with no IP information are allowed (conservative: don't block
    /// unknown sources), but a debug log is emitted.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let ip = match ip {
            Some(ip) => ip,
            None => {
                tracing::debug!("admin API request with no client IP — skipping rate limit");
                return true;
            }
        };

        // If rate limiting is effectively disabled, always allow
        if self.max_tokens >= 1_000_000.0 {
            return true;
        }

        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(ip).or_insert_with(|| TokenBucket {
            tokens: self.max_tokens,
            last_refill: Instant::now(),
        });

        // Refill tokens based on elapsed time
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.last_refill = now;
        bucket.tokens = (bucket.tokens + elapsed * self.refill_rate).min(self.max_tokens);

        // Try to consume one token
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Prune stale IP buckets that haven't been used recently.
    ///
    /// Call this periodically (e.g., every 60 seconds) to prevent
    /// memory leaks from long-running nodes with many unique IPs.
    pub fn prune_stale(&self, max_age: Duration) {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let before = buckets.len();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_age);
        let after = buckets.len();
        if before != after {
            tracing::debug!(
                pruned = before - after,
                remaining = after,
                "pruned stale rate limit buckets"
            );
        }
    }
}

// ── Authentication Metrics ────────────────────────────────────────────────────

/// Prometheus metrics for admin API authentication events.
pub struct AuthMetrics {
    /// Total successful authentications.
    pub auth_successes_total: IntCounter,

    /// Total failed authentications (bad token, missing token).
    pub auth_failures_total: IntCounter,

    /// Total requests rate-limited on the admin API.
    pub rate_limited_total: IntCounter,
}

impl AuthMetrics {
    /// Create and register auth metrics with the given Prometheus registry.
    pub fn new(registry: &Registry) -> Self {
        let auth_successes_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_successes_total",
            "Successful admin API authentications",
        ))
        .expect("failed to create auth_successes_total counter");
        registry
            .register(Box::new(auth_successes_total.clone()))
            .expect("failed to register auth_successes_total counter");

        let auth_failures_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_failures_total",
            "Failed admin API authentications",
        ))
        .expect("failed to create auth_failures_total counter");
        registry
            .register(Box::new(auth_failures_total.clone()))
            .expect("failed to register auth_failures_total counter");

        let rate_limited_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_rate_limited_total",
            "Admin API requests rejected by rate limiter",
        ))
        .expect("failed to create rate_limited_total counter");
        registry
            .register(Box::new(rate_limited_total.clone()))
            .expect("failed to register rate_limited_total counter");

        AuthMetrics {
            auth_successes_total,
            auth_failures_total,
            rate_limited_total,
        }
    }

    /// Create auth metrics without registering with a registry (for testing).
    pub fn new_unregistered() -> Self {
        let auth_successes_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_successes_total",
            "Successful admin API authentications",
        ))
        .unwrap();

        let auth_failures_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_failures_total",
            "Failed admin API authentications",
        ))
        .unwrap();

        let rate_limited_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_rate_limited_total",
            "Admin API requests rejected by rate limiter",
        ))
        .unwrap();

        AuthMetrics {
            auth_successes_total,
            auth_failures_total,
            rate_limited_total,
        }
    }
}

// ── Token Rotation ────────────────────────────────────────────────────────────

use serde::Deserialize;

/// Request body for token rotation endpoint.
#[derive(Debug, Deserialize)]
pub struct RotateTokenRequest {
    /// Which token to rotate: "read" or "write".
    pub token_type: String,

    /// The new token value. If not provided, a random token is generated.
    pub new_token: Option<String>,
}

/// Validate a token rotation request and return the new token value.
///
/// Returns `Ok(new_token)` if the request is valid, or an error message.
pub fn validate_rotation_request(req: &RotateTokenRequest) -> Result<String, String> {
    if req.token_type != "read" && req.token_type != "write" {
        return Err("token_type must be 'read' or 'write'".to_string());
    }

    let new_token = req
        .new_token
        .clone()
        .unwrap_or_else(|| AuthConfig::generate_token());

    if new_token.len() < 16 {
        return Err(format!(
            "new token must be at least 16 characters (got {})",
            new_token.len()
        ));
    }

    Ok(new_token)
}

// ── Config File Permissions ───────────────────────────────────────────────────

/// Check if a config file has overly permissive permissions.
///
/// On Unix, warns if the file is readable by group or others (mode & 0o077 != 0).
/// This prevents accidental exposure of auth tokens in shared environments.
pub fn check_config_file_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{:o}", mode & 0o777),
                    "config file has overly permissive permissions — \
                     other users can read the auth tokens. \
                     Recommended: chmod 600"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ── TLS Enforcement ───────────────────────────────────────────────────────────

/// Check if the admin API TLS requirement is satisfied at startup.
///
/// Returns an error if auth is enabled with `require_tls = true` but
/// no TLS certificate is configured for the admin API.
pub fn check_admin_tls_requirement(
    auth_config: &AuthConfig,
    admin_tls_configured: bool,
) -> Result<(), String> {
    if !auth_config.enabled {
        return Ok(());
    }

    if !auth_config.require_tls {
        tracing::warn!(
            "Admin API authentication is enabled but TLS is NOT required. \
             Bearer tokens will be sent over plaintext HTTP. \
             Set auth.require_tls = true in production."
        );
        return Ok(());
    }

    if !admin_tls_configured {
        return Err(format!(
            "Admin API requires TLS when authentication is enabled, \
             but no TLS certificate is configured. \
             Either:\n\
             1. Configure admin.tls_cert / admin.tls_key (or shared proxy.tls_cert / proxy.tls_key) for the admin HTTPS listener\n\
             2. Set auth.require_tls = false (NOT recommended for production)\n\
             3. Disable authentication (auth.enabled = false, NOT recommended)"
        ));
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_public_endpoints() {
        assert!(is_public_endpoint("/health"));
        assert!(is_public_endpoint("/healthz"));
        assert!(is_public_endpoint("/readyz"));
        assert!(is_public_endpoint("/livez"));
        assert!(is_public_endpoint("/_platform/health"));
        assert!(is_public_endpoint("/status/metrics"));
        assert!(is_public_endpoint("/favicon.ico"));
    }

    #[test]
    fn test_non_public_endpoints() {
        assert!(!is_public_endpoint("/admin/config"));
        assert!(!is_public_endpoint("/admin/rebuild"));
        assert!(!is_public_endpoint("/admin/gc/force"));
        assert!(!is_public_endpoint("/admin/auth/rotate-token"));
        assert!(!is_public_endpoint("/status/pgbouncer"));
        assert!(!is_public_endpoint("/logs/my-app"));
        assert!(!is_public_endpoint("/upstreams"));
    }

    #[test]
    fn test_required_permission_get() {
        let get = axum::http::Method::GET;
        assert_eq!(required_permission(&get, "/admin/config"), Permission::Read);
        assert_eq!(
            required_permission(&get, "/status/metrics"),
            Permission::Read
        );
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
        let ip = extract_client_ip(&headers);
        assert_eq!(ip, Some("192.168.1.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_extract_client_ip_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
        let ip = extract_client_ip(&headers);
        assert_eq!(ip, Some("10.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_extract_client_ip_xff_priority() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Forwarded-For", "192.168.1.1".parse().unwrap());
        headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
        let ip = extract_client_ip(&headers);
        // X-Forwarded-For takes priority
        assert_eq!(ip, Some("192.168.1.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_extract_client_ip_none() {
        let headers = HeaderMap::new();
        let ip = extract_client_ip(&headers);
        assert!(ip.is_none());
    }

    #[test]
    fn test_extract_client_ip_ipv6() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Real-IP", "::1".parse().unwrap());
        let ip = extract_client_ip(&headers);
        assert_eq!(ip, Some("::1".parse::<IpAddr>().unwrap()));
    }

    // ── Rate Limiter Tests ─────────────────────────────────────────────

    #[test]
    fn test_rate_limiter_allows_within_burst() {
        let limiter = AdminRateLimiter::new(10, 5);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Should allow up to burst (5) requests
        for _ in 0..5 {
            assert!(limiter.allow(Some(ip)));
        }
    }

    #[test]
    fn test_rate_limiter_blocks_excess() {
        let limiter = AdminRateLimiter::new(10, 3);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Allow burst
        assert!(limiter.allow(Some(ip)));
        assert!(limiter.allow(Some(ip)));
        assert!(limiter.allow(Some(ip)));

        // Block the next one
        assert!(!limiter.allow(Some(ip)));
    }

    #[test]
    fn test_rate_limiter_no_ip_allowed() {
        let limiter = AdminRateLimiter::new(10, 3);
        // No IP — should be allowed (conservative)
        assert!(limiter.allow(None));
    }

    #[test]
    fn test_rate_limiter_different_ips_independent() {
        let limiter = AdminRateLimiter::new(10, 1);
        let ip1: IpAddr = "127.0.0.1".parse().unwrap();
        let ip2: IpAddr = "127.0.0.2".parse().unwrap();

        assert!(limiter.allow(Some(ip1)));
        assert!(!limiter.allow(Some(ip1))); // ip1 exhausted
        assert!(limiter.allow(Some(ip2))); // ip2 still has tokens
    }

    #[test]
    fn test_rate_limiter_disabled() {
        let limiter = AdminRateLimiter::disabled();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Should allow many requests when disabled
        for _ in 0..100 {
            assert!(limiter.allow(Some(ip)));
        }
    }

    #[test]
    fn test_rate_limiter_prune_stale() {
        let limiter = AdminRateLimiter::new(10, 5);
        let ip1: IpAddr = "127.0.0.1".parse().unwrap();
        let ip2: IpAddr = "127.0.0.2".parse().unwrap();

        // Use both IPs
        limiter.allow(Some(ip1));
        limiter.allow(Some(ip2));

        // Prune with very short max_age — all buckets are stale
        limiter.prune_stale(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(2));
        limiter.prune_stale(Duration::from_millis(1));

        // Both IPs should get fresh buckets (burst tokens)
        assert!(limiter.allow(Some(ip1)));
        assert!(limiter.allow(Some(ip2)));
    }

    // ── Token Rotation Validation Tests ────────────────────────────────

    #[test]
    fn test_validate_rotation_read() {
        let req = RotateTokenRequest {
            token_type: "read".to_string(),
            new_token: None,
        };
        let result = validate_rotation_request(&req);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 64); // generated token
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

    // ── TLS Enforcement Tests ──────────────────────────────────────────

    #[test]
    fn test_tls_check_auth_disabled() {
        let config = AuthConfig::default(); // enabled = false
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

    // ── Auth Metrics Tests ─────────────────────────────────────────────

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

        // Verify metrics are in the registry
        let families = registry.gather();
        let names: Vec<&str> = families.iter().map(|f| f.name()).collect();
        assert!(names.contains(&"wasm_admin_auth_successes_total"));
        assert!(names.contains(&"wasm_admin_auth_failures_total"));
        assert!(names.contains(&"wasm_admin_rate_limited_total"));
    }

    // ── Integration Tests with Real Axum Router ───────────────────────

    use axum::body::Body;
    use axum::routing::{get, post};
    use tower::util::ServiceExt;

    /// Helper: build a test Axum router with the auth middleware applied.
    fn test_auth_router(config: AuthConfig) -> axum::Router {
        let state = AuthState {
            config: Arc::new(RwLock::new(config)),
            metrics: Arc::new(AuthMetrics::new_unregistered()),
            rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)), // generous for tests
            audit_fn: None,
            node_id: "test-node".to_string(),
        };

        axum::Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/healthz", get(|| async { "ok" }))
            .route("/readyz", get(|| async { "ok" }))
            .route("/livez", get(|| async { "ok" }))
            .route("/status/metrics", get(|| async { "# metrics\n" }))
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
        let config = AuthConfig::default(); // enabled = false
        let app = test_auth_router(config);

        // GET without token should work
        let req = Request::builder()
            .uri("/admin/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // POST without token should also work
        let app = test_auth_router(AuthConfig::default());
        let req = Request::builder()
            .method("POST")
            .uri("/admin/rebuild")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_integration_public_endpoints_no_auth() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        };
        let app = test_auth_router(config);

        // All public endpoints should work without any token
        for path in &[
            "/health",
            "/healthz",
            "/readyz",
            "/livez",
            "/status/metrics",
        ] {
            let app = test_auth_router(AuthConfig {
                enabled: true,
                write_token: Some("write_token_1234567890".to_string()),
                read_token: Some("read_token_1234567890".to_string()),
                ..Default::default()
            });
            let req = Request::builder().uri(*path).body(Body::empty()).unwrap();
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
    async fn test_integration_missing_token_returns_401() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        };
        let app = test_auth_router(config);

        let req = Request::builder()
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

        let req = Request::builder()
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

        let req = Request::builder()
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

        let req = Request::builder()
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

        let req = Request::builder()
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

        let req = Request::builder()
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
        // Only write token configured — no read-only access possible
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: None,
            ..Default::default()
        };
        let app = test_auth_router(config);

        // Write token works for GET
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer write_token_1234567890")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // No read token means any other token fails
        let app = test_auth_router(AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: None,
            ..Default::default()
        });
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer some_other_token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_integration_rate_limit_returns_429() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            ..Default::default()
        };

        // Create a very restrictive rate limiter
        let state = AuthState {
            config: Arc::new(RwLock::new(config)),
            metrics: Arc::new(AuthMetrics::new_unregistered()),
            rate_limiter: Arc::new(AdminRateLimiter::new(1, 2)), // 1/s, burst 2
            audit_fn: None,
            node_id: "test-node".to_string(),
        };

        // Use a shared rate limiter across all requests in this test
        let shared_limiter = Arc::new(AdminRateLimiter::new(1, 2)); // 1/s, burst 2

        let make_state = || AuthState {
            config: Arc::new(RwLock::new(AuthConfig {
                enabled: true,
                write_token: Some("write_token_1234567890".to_string()),
                ..Default::default()
            })),
            metrics: Arc::new(AuthMetrics::new_unregistered()),
            rate_limiter: shared_limiter.clone(),
            audit_fn: None,
            node_id: "test-node".to_string(),
        };

        // Use up the burst (2 requests allowed)
        for _ in 0..2 {
            let app: axum::Router = axum::Router::new()
                .route("/admin/config", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn_with_state(
                    make_state(),
                    auth_middleware,
                ));
            let req = Request::builder()
                .uri("/admin/config")
                .header("Authorization", "Bearer write_token_1234567890")
                .header("X-Real-IP", "10.0.0.1")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // Next request should be rate-limited (burst exhausted)
        let app: axum::Router = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                make_state(),
                auth_middleware,
            ));
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer write_token_1234567890")
            .header("X-Real-IP", "10.0.0.1")
            .body(Body::empty())
            .unwrap();
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
            audit_fn: None,
            node_id: "test-node".to_string(),
        };

        let app = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        // Old write token works
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer old_write_token_12345")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Rotate the write token
        {
            let mut cfg = shared_config.write().await;
            cfg.write_token = Some("new_write_token_67890".to_string());
        }

        // Old write token should now fail
        let app = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                AuthState {
                    config: shared_config.clone(),
                    metrics: Arc::new(AuthMetrics::new_unregistered()),
                    rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                    audit_fn: None,
                    node_id: "test-node".to_string(),
                },
                auth_middleware,
            ));
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer old_write_token_12345")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // New write token should work
        let app = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                AuthState {
                    config: shared_config.clone(),
                    metrics: Arc::new(AuthMetrics::new_unregistered()),
                    rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                    audit_fn: None,
                    node_id: "test-node".to_string(),
                },
                auth_middleware,
            ));
        let req = Request::builder()
            .uri("/admin/config")
            .header("Authorization", "Bearer new_write_token_67890")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Read token should still work (unchanged)
        let app = axum::Router::new()
            .route("/admin/config", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                AuthState {
                    config: shared_config,
                    metrics: Arc::new(AuthMetrics::new_unregistered()),
                    rate_limiter: Arc::new(AdminRateLimiter::new(1000, 2000)),
                    audit_fn: None,
                    node_id: "test-node".to_string(),
                },
                auth_middleware,
            ));
        let req = Request::builder()
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

        // POST /admin/rebuild — read token should be forbidden
        let app = test_auth_router(config.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/admin/rebuild")
            .header("Authorization", "Bearer read_token_1234567890")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // POST /admin/gc/force — read token should be forbidden
        let app = test_auth_router(config.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/admin/gc/force")
            .header("Authorization", "Bearer read_token_1234567890")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // POST /admin/auth/rotate-token — read token should be forbidden
        let app = test_auth_router(config);
        let req = Request::builder()
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

        // "Basic" instead of "Bearer"
        let req = Request::builder()
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

        // 401 response should have JSON body
        let req = Request::builder()
            .uri("/admin/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "unauthorized");

        // 403 response should have JSON body
        let app = test_auth_router(AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        });
        let req = Request::builder()
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
}
