use super::upstream::UpstreamRegistry;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

#[derive(Clone)]
struct AdminState {
    upstream: Arc<UpstreamRegistry>,
    admin_token: Option<String>,
}

/// Build the admin API router with optional bearer-token authentication.
///
/// If `admin_token` is `Some(token)`, every request (except `/_platform/health`)
/// must include an `Authorization: Bearer <token>` header.  Requests without
/// a valid token receive `401 Unauthorized`.
///
/// If `admin_token` is `None`, authentication is disabled — useful for local
/// development or when the admin port is bound to localhost only.
pub fn admin_router(upstream: Arc<UpstreamRegistry>, admin_token: Option<String>) -> Router {
    let state = AdminState {
        upstream,
        admin_token,
    };
    Router::new()
        .route("/upstreams", get(list_upstreams))
        .route("/health", get(health_check))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Bearer-token authentication middleware.
///
/// - If no token is configured, all requests pass through.
/// - Health-check paths (`/health`, `/_platform/health`) are always allowed
///   so that load-balancers can probe the node without credentials.
/// - Otherwise the `Authorization` header must be `Bearer <token>`.
async fn auth_middleware(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // No token configured → auth disabled
    let expected_token = match &state.admin_token {
        Some(t) => t,
        None => return next.run(request).await,
    };

    // Health endpoints are always unauthenticated
    let path = request.uri().path();
    if path == "/health" || path == "/_platform/health" {
        return next.run(request).await;
    }

    // Extract and validate Bearer token
    let authorized = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == expected_token)
        .unwrap_or(false);

    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "valid Bearer token required via Authorization header"
            })),
        )
            .into_response()
    }
}

async fn health_check() -> &'static str {
    "ok"
}

async fn list_upstreams(State(s): State<AdminState>) -> Json<serde_json::Value> {
    let map = s.upstream.inner.read().await;
    let out: serde_json::Map<_, _> = map
        .iter()
        .map(|(k, (_, addrs))| {
            let addrs_str: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            (k.clone(), serde_json::json!(addrs_str))
        })
        .collect();
    Json(serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    fn test_router(token: Option<String>) -> Router {
        let upstream = Arc::new(UpstreamRegistry::default());
        admin_router(upstream, token)
    }

    #[tokio::test]
    async fn test_no_token_configured_allows_all() {
        let app = test_router(None);
        let req = Request::builder()
            .uri("/upstreams")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_token_required_rejects_without_header() {
        let app = test_router(Some("secret123".to_string()));
        let req = Request::builder()
            .uri("/upstreams")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_token_required_allows_with_correct_bearer() {
        let app = test_router(Some("secret123".to_string()));
        let req = Request::builder()
            .uri("/upstreams")
            .header("Authorization", "Bearer secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_token_required_rejects_wrong_bearer() {
        let app = test_router(Some("secret123".to_string()));
        let req = Request::builder()
            .uri("/upstreams")
            .header("Authorization", "Bearer wrongtoken")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_health_endpoint_always_allowed() {
        let app = test_router(Some("secret123".to_string()));
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
