use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::ConnectInfo,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use common::auth::{AuthConfig, Permission, TokenType, TrustedProxyNet};
use tokio::sync::RwLock;

use super::{extract_client_ip, AdminRateLimiter, AuthMetrics};

/// Information about an admin API action for audit logging.
#[derive(Debug, Clone)]
pub struct AuditInfo {
    pub path: String,
    pub method: String,
    pub token_type: TokenType,
    pub client_ip: Option<std::net::IpAddr>,
    pub status_code: u16,
    pub node_id: String,
}

/// Callback type for audit logging.
///
/// The node crate provides an implementation that writes to the audit trail
/// via `supervisor::audit::write_audit_event`. This keeps the proxy crate
/// independent of the supervisor crate (avoiding circular dependencies).
pub type AuditCallback = Arc<dyn Fn(AuditInfo) + Send + Sync>;

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

    /// Immediate peer IPs/CIDRs allowed to supply forwarded client IP headers.
    pub trusted_proxies: Arc<Vec<TrustedProxyNet>>,

    /// Optional audit callback. When set, successful admin API calls are logged.
    pub audit_fn: Option<AuditCallback>,

    /// Node identifier for audit logging.
    pub node_id: String,
}

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

    if is_public_endpoint(&path) {
        return next.run(request).await;
    }

    let client_ip = extract_client_ip(
        &headers,
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0),
        state.trusted_proxies.as_ref(),
    );
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

    let auth_header = headers.get("Authorization").and_then(|v| v.to_str().ok());
    let config = state.config.read().await;
    let auth_result = config.authenticate(auth_header);

    let required = required_permission(&method, &path);
    if auth_result.permission < required {
        state.metrics.auth_failures_total.inc();

        match auth_result.permission {
            Permission::None => {
                tracing::warn!(
                    path = %path,
                    method = %method,
                    ip = ?client_ip,
                    "admin API authentication failed - invalid or missing token"
                );

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
                    "admin API authorization failed - read token used for write operation"
                );

                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "forbidden",
                        "message": "insufficient permissions - read token cannot perform write operations"
                    })),
                )
                    .into_response();
            }
            Permission::Write => {
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

    tracing::debug!(
        path = %path,
        method = %method,
        token_type = %auth_result.token_type,
        "admin API request authenticated"
    );

    state.metrics.auth_successes_total.inc();

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
