pub mod authz;
pub mod circuit_breaker;
pub mod config;
pub mod cors;
pub mod distributed_limiter;
pub mod errors;
pub mod metrics;
pub mod oidc;
pub mod transform;

use config::GatewayRouteConfig;
use oidc::OidcProvider;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The API gateway. Owns all middleware state and orchestrates the pipeline.
pub struct Gateway {
    /// OIDC provider for JWT validation. None = auth disabled globally.
    pub oidc: Option<Arc<OidcProvider>>,

    /// Circuit breaker manager for all upstream apps.
    pub circuit_breaker: Arc<circuit_breaker::CircuitBreakerManager>,

    /// Per-app distributed rate limiters.
    pub distributed_limiters: Arc<RwLock<HashMap<String, Arc<distributed_limiter::DistributedRateLimiter>>>>,

    /// Per-route gateway configurations.
    pub route_configs: Arc<RwLock<HashMap<String, GatewayRouteConfig>>>,

    /// Gateway metrics.
    pub metrics: Arc<metrics::GatewayMetrics>,
}

impl Gateway {
    pub fn new(oidc: Option<Arc<OidcProvider>>) -> Self {
        Gateway {
            oidc,
            circuit_breaker: Arc::new(circuit_breaker::CircuitBreakerManager::new()),
            distributed_limiters: Arc::new(RwLock::new(HashMap::new())),
            route_configs: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(metrics::GatewayMetrics::new()),
        }
    }

    /// Get the gateway config for a route (by app_id).
    pub async fn get_route_config(&self, app_id: &common::types::AppId) -> Option<GatewayRouteConfig> {
        self.route_configs.read().await.get(&app_id.0).cloned()
    }

    /// Set the gateway config for a route.
    pub async fn set_route_config(&self, app_id: &str, config: GatewayRouteConfig) {
        self.route_configs.write().await.insert(app_id.to_string(), config);
    }

    /// Authenticate a request. Returns the user identity if auth is required.
    pub async fn authenticate(
        &self,
        session: &pingora_proxy::Session,
    ) -> Result<oidc::UserIdentity, GatewayError> {
        let provider = self
            .oidc
            .as_ref()
            .ok_or(GatewayError::Auth("OIDC not configured".to_string()))?;

        let token = session
            .req_header()
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(GatewayError::Auth("missing Authorization header".to_string()))?;

        provider.validate_token(token).await
    }

    /// Check if the circuit breaker is open for an app.
    pub fn is_circuit_open(&self, app_id: &common::types::AppId) -> bool {
        self.circuit_breaker.is_circuit_open(&app_id.0)
    }
}

/// Gateway errors returned to the client.
#[derive(Debug)]
pub enum GatewayError {
    Auth(String),
    Oidc(String),
    RateLimit(String),
    CircuitOpen(String),
    Cors(String),
    Internal(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Auth(msg) => write!(f, "authentication error: {msg}"),
            GatewayError::Oidc(msg) => write!(f, "OIDC error: {msg}"),
            GatewayError::RateLimit(msg) => write!(f, "rate limit: {msg}"),
            GatewayError::CircuitOpen(msg) => write!(f, "circuit open: {msg}"),
            GatewayError::Cors(msg) => write!(f, "CORS error: {msg}"),
            GatewayError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for GatewayError {}
