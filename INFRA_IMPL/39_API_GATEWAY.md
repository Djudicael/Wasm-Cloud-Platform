# Step 39 — Built-In API Gateway

## Goal
Transform the existing Pingora proxy into a full API gateway so that every Wasm
application deployed on the platform automatically gets authentication,
authorization, distributed rate limiting, CORS, circuit breaking, and request
transformation — without any external system (no KrakenD, no Kong, no Envoy
sidecar). The platform IS the gateway.

The system must:
- Validate JWT/OIDC tokens at the proxy layer before traffic reaches Wasm apps
- Support Keycloak as an external identity provider (and any OIDC-compliant IdP)
- Enforce per-route authentication policies (public, authenticated, role-based)
- Share rate-limit counters across nodes via NATS KV (no Redis)
- Provide per-route CORS policies
- Implement circuit breaking per upstream app
- Transform requests (header injection, path rewriting, query param stripping)
- Expose gateway metrics to Prometheus
- Require zero changes to Wasm application code — enforcement is transparent
- Keep the architecture simple: middleware inside the existing Pingora pipeline

---

## Context & Rationale

### The Problem This Solves

Every multi-tenant API platform needs an API gateway. Without one, each Wasm app
must implement its own:

- **Authentication** — validate JWTs, check token expiry, verify signatures
- **Authorization** — check roles/permissions per endpoint
- **Rate limiting** — prevent abuse (currently node-local only, not shared)
- **CORS** — handle preflight requests so browsers can call the API
- **Circuit breaking** — stop sending traffic to a crashing app

This leads to duplicated effort, inconsistent security, and apps that are
vulnerable because the developer forgot to validate the token on one endpoint.

### Why Built-In (Not External Gateway)

| Approach | Extra Infra | Latency | Config Sync | Complexity |
|----------|-------------|---------|-------------|------------|
| KrakenD sidecar | +1 process per app | +1 hop | Manual | High |
| Kong/Envoy as separate gateway | +1 cluster | +1 hop | Decoupled | High |
| **Built into Pingora** | **None** | **0 extra hops** | **Automatic** | **Low** |

We already have a Pingora proxy that every request passes through. Adding
gateway middleware to the existing `request_filter` → `upstream_peer` →
`upstream_request_filter` pipeline costs zero extra network hops and requires
zero additional infrastructure.

The gateway is not a separate component — it is a set of middleware modules
inside `crates/proxy/` that execute in the Pingora request pipeline.

### Why Keycloak (OIDC) for Authentication

Keycloak is the most widely deployed open-source identity provider. It supports:

- OAuth 2.0 / OpenID Connect (standard protocols)
- JWT token issuance with RS256 signing
- Role-based access control (realm roles, client roles)
- User federation (LDAP, Active Directory)
- Token revocation and refresh
- Multi-tenancy via realms

But we don't couple to Keycloak specifically — we validate JWTs using the
OIDC Discovery protocol (`/.well-known/openid-configuration`), which works
with any OIDC-compliant IdP (Auth0, Okta, Dex, etc.).

### Why NATS KV for Distributed Rate Limiting

The current rate limiter (Step 24) is node-local: each node tracks its own
token buckets independently. This means the effective global rate limit is
`per_node_limit × number_of_nodes`, which is imprecise.

KrakenD's community edition has the same problem — no shared state. The
enterprise edition uses Redis. We don't want Redis as a dependency.

NATS KV provides:
- **Eventually consistent key-value store** — built into NATS, which we already run
- **Atomic counter operations** — `KeyValue::update()` with expected revision
- **TTL on keys** — counters auto-expire, no pruning needed
- **No new infrastructure** — NATS is already required for cluster messaging

The trade-off is latency: a NATS KV read on the same LAN takes ~0.5ms. We
mitigate this with a **two-tier strategy**: local token bucket for the hot path
(sub-microsecond), NATS KV sync for periodic reconciliation (every 100ms).

### What About Simplicity?

The user's requirement is clear: keep things as simple as possible. This spec
follows these principles:

1. **No new infrastructure** — everything uses NATS, Pingora, and the existing
   proxy pipeline
2. **Opt-in, not opt-out** — routes are public by default; auth is added
   explicitly via route configuration
3. **Progressive enhancement** — Phase 1 is auth + distributed rate limiting;
   Phase 2 adds CORS, circuit breaking, transforms
4. **Zero app changes** — the Wasm app receives clean, authenticated requests
   with user identity in headers; it doesn't need to validate tokens

---

## 1. Gateway Middleware Architecture

The gateway is implemented as a middleware chain inside Pingora's
`request_filter()` method. Each middleware is a function that takes the
session, context, and route config, and returns `Ok(continue)` or
`Err(reject)`.

```
Incoming Request
     │
     ▼
┌─────────────────────────┐
│ 1. Route Resolution     │  ← existing: HostRouter.resolve()
│    (host + path → app)  │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 2. CORS Preflight       │  ← NEW: return 200 + CORS headers for OPTIONS
│    (if route has CORS)  │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 3. Authentication       │  ← NEW: validate JWT from Authorization header
│    (if route requires)  │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 4. Authorization        │  ← NEW: check roles/permissions against route policy
│    (if route requires)  │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 5. Rate Limiting        │  ← ENHANCED: distributed via NATS KV
│    (per-app + per-IP)   │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 6. Circuit Breaker      │  ← NEW: check upstream health before proxying
│    (per-app)            │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│ 7. Request Transform    │  ← NEW: inject headers, rewrite path
│    (if route configured)│
└────────┬────────────────┘
         │
         ▼
    Upstream (Wasm App)
```

Each middleware is a separate module in `crates/proxy/src/gateway/`. The
`request_filter()` method calls them in order. If any middleware rejects the
request, the pipeline short-circuits and returns an error response.

### Updated `request_filter` Flow

```rust
// crates/proxy/src/service.rs — updated request_filter

async fn request_filter(
    &self,
    session: &mut Session,
    ctx: &mut Self::CTX,
) -> PingoraResult<bool> {
    // 1. Route resolution (existing)
    let host = extract_host(session);
    let path = session.req_header().uri.path().to_string();
    let resolved = self.router.resolve(&host, &path).await;

    let route_config = resolved.as_ref()
        .and_then(|r| self.gateway.get_route_config(&r.app_id));

    if resolved.is_none() {
        session.respond_error(502).await?;
        return Ok(true);
    }
    ctx.app_id = Some(resolved.unwrap().app_id.clone());
    ctx.route_config = route_config;

    // 2. CORS preflight (new)
    if let Some(ref cfg) = ctx.route_config {
        if cfg.cors.is_some() && session.req_header().method == "OPTIONS" {
            return self.gateway.handle_cors_preflight(session, cfg).await;
        }
    }

    // 3. Authentication (new)
    if let Some(ref cfg) = ctx.route_config {
        if cfg.auth != AuthPolicy::None {
            match self.gateway.authenticate(session, cfg).await {
                Ok(identity) => ctx.user_identity = Some(identity),
                Err(e) => {
                    session.respond_error(401).await?;
                    return Ok(true);
                }
            }
        }
    }

    // 4. Authorization (new)
    if let Some(ref cfg) = ctx.route_config {
        if let Some(ref identity) = ctx.user_identity {
            if !self.gateway.authorize(identity, cfg) {
                session.respond_error(403).await?;
                return Ok(true);
            }
        }
    }

    // 5. Rate limiting (enhanced with NATS KV)
    // ... existing rate limit code, now with distributed counters ...

    // 6. Circuit breaker (new)
    if let Some(ref app_id) = ctx.app_id {
        if self.gateway.is_circuit_open(app_id) {
            session.respond_error(503).await?;
            return Ok(true);
        }
    }

    // 7. Backpressure (existing)
    if !self.backpressure.is_accepting() {
        session.respond_error(503).await?;
        return Ok(true);
    }

    Ok(false)
}
```

---

## 2. Route Configuration Extension

### GatewayRouteConfig

Each route can have an optional gateway configuration. This is stored alongside
the existing route in the `HostRouter` and persisted in the redb storage.

```rust
// crates/proxy/src/gateway/config.rs

use serde::{Deserialize, Serialize};

/// Gateway configuration for a single route.
/// Stored per-route in the route table. All fields are optional —
/// a route with no gateway config is fully public (no auth, no CORS,
/// default rate limits).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayRouteConfig {
    /// Authentication policy for this route.
    #[serde(default)]
    pub auth: AuthPolicy,

    /// CORS policy. None = no CORS headers (browsers will block cross-origin).
    #[serde(default)]
    pub cors: Option<CorsPolicy>,

    /// Request transformation rules.
    #[serde(default)]
    pub transform: Option<RequestTransform>,

    /// Override the app-level rate limit for this route.
    #[serde(default)]
    pub rate_limit: Option<RouteRateLimit>,

    /// Circuit breaker configuration for this route's upstream.
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerConfig>,
}

/// Authentication policy for a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    /// No authentication required. Anyone can access this route.
    #[default]
    None,

    /// Valid JWT required. The token is validated against the OIDC provider's
    /// JWKS endpoint. User identity is extracted and forwarded to the upstream
    /// via X-User-Id, X-User-Roles headers.
    Authenticated,

    /// Valid JWT required AND the user must have at least one of the specified
    /// roles. Roles are checked against the JWT's `realm_access.roles` or
    /// `resource_access.<client_id>.roles` claims.
    Roles {
        /// Which roles are accepted (OR logic — any match passes).
        allowed_roles: Vec<String>,
        /// Which Keycloak client's roles to check.
        /// None = check realm_access.roles.
        /// Some("my-client") = check resource_access.my-client.roles.
        client_id: Option<String>,
    },
}

/// CORS policy for a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorsPolicy {
    /// Allowed origins. Use "*" for public APIs, or list specific origins.
    /// Examples: ["*"], ["https://app.example.com", "https://admin.example.com"]
    pub allowed_origins: Vec<String>,

    /// Allowed HTTP methods.
    #[serde(default = "CorsPolicy::default_methods")]
    pub allowed_methods: Vec<String>,

    /// Allowed request headers.
    #[serde(default = "CorsPolicy::default_headers")]
    pub allowed_headers: Vec<String>,

    /// Headers exposed to the browser (not all response headers are visible
    /// to JavaScript by default).
    #[serde(default)]
    pub expose_headers: Vec<String>,

    /// Whether to include credentials (cookies, Authorization header)
    /// in cross-origin requests. Cannot be true when origins is ["*"].
    #[serde(default)]
    pub allow_credentials: bool,

    /// How long the browser can cache the preflight response (seconds).
    #[serde(default = "CorsPolicy::default_max_age")]
    pub max_age_secs: u32,
}

impl CorsPolicy {
    fn default_methods() -> Vec<String> {
        vec!["GET".into(), "POST".into(), "PUT".into(), "DELETE".into(), "PATCH".into(), "OPTIONS".into()]
    }
    fn default_headers() -> Vec<String> {
        vec!["Authorization".into(), "Content-Type".into(), "X-Request-Id".into()]
    }
    fn default_max_age() -> u32 { 86400 } // 24 hours
}

/// Request transformation rules applied before forwarding to upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RequestTransform {
    /// Headers to add to the request before forwarding.
    /// Useful for injecting API keys, upstream auth tokens, etc.
    pub add_headers: Vec<(String, String)>,

    /// Headers to remove from the request before forwarding.
    /// Useful for stripping internal headers that should not reach the app.
    pub remove_headers: Vec<String>,

    /// Path prefix to add before the existing path.
    /// Example: prefix = "/api/v2" → /users becomes /api/v2/users
    pub path_prefix: Option<String>,

    /// Query parameters to strip from the request before forwarding.
    /// Useful for removing tracking params that the upstream doesn't need.
    pub strip_query_params: Vec<String>,
}

/// Per-route rate limit override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteRateLimit {
    /// Requests per second for this specific route (overrides app-level).
    pub requests_per_second: u32,

    /// Burst capacity for this route.
    pub burst_capacity: u32,

    /// Whether this route's rate limit is shared across all nodes (distributed)
    /// or node-local only. Default: distributed.
    #[serde(default = "default_true")]
    pub distributed: bool,
}

fn default_true() -> bool { true }

/// Circuit breaker configuration per upstream app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    #[serde(default = "CircuitBreakerConfig::default_failure_threshold")]
    pub failure_threshold: u32,

    /// Duration in seconds to keep the circuit open before allowing a
    /// half-open probe request through.
    #[serde(default = "CircuitBreakerConfig::default_reset_timeout_secs")]
    pub reset_timeout_secs: u32,

    /// What counts as a "failure". Default: 5xx responses and connection errors.
    #[serde(default)]
    pub failure_criteria: FailureCriteria,
}

impl CircuitBreakerConfig {
    fn default_failure_threshold() -> u32 { 5 }
    fn default_reset_timeout_secs() -> u32 { 30 }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureCriteria {
    /// Default: 5xx responses and connection errors.
    #[default]
    ServerErrors,
    /// 5xx responses, connection errors, and 4xx responses.
    AllErrors,
    /// Custom: count only specific HTTP status codes as failures.
    StatusCodes(Vec<u16>),
}
```

### Deploy Manifest Example

```toml
# deploy.toml — API with authentication and CORS

app]
id = "api-users:v2"
fuel_quota = 500000000
memory_pages = 2048
max_instances = 10
wasm_bind_port = 8080

# Gateway configuration (new section)
[app.gateway.auth]
policy = "roles"
allowed_roles = ["admin", "user"]
client_id = "api-users"

[app.gateway.cors]
allowed_origins = ["https://app.example.com"]
allow_credentials = true
max_age_secs = 3600

[app.gateway.transform]
add_headers = [["X-Api-Version", "2"]]
remove_headers = ["X-Internal-Token"]

[app.gateway.rate_limit]
requests_per_second = 500
burst_capacity = 100
distributed = true

[app.gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30
```

```toml
# deploy.toml — Public API (no auth, CORS open)

[app]
id = "public-api:v1"
wasm_bind_port = 8080

[app.gateway.cors]
allowed_origins = ["*"]
max_age_secs = 86400

# No [app.gateway.auth] = public route (default: no auth)
```

```toml
# deploy.toml — Internal service (authenticated, no CORS, strict rate limit)

[app]
id = "payment-processor:v1"
wasm_bind_port = 8080

[app.gateway.auth]
policy = "authenticated"

[app.gateway.rate_limit]
requests_per_second = 50
burst_capacity = 10
distributed = true

[app.gateway.circuit_breaker]
failure_threshold = 3
reset_timeout_secs = 60
```

---

## 3. OIDC Authentication Middleware

### How It Works

```
Browser → Pingora Proxy → Wasm App
           │
           │ 1. Extract Authorization: Bearer <token>
           │ 2. Decode JWT header → get kid (key ID)
           │ 3. Look up signing key in JWKS cache
           │ 4. Verify signature + expiry + audience
           │ 5. Extract claims (sub, roles, email)
           │ 6. Inject X-User-Id, X-User-Roles headers
           │ 7. Forward to upstream
```

### OIDC Provider Configuration

```rust
// crates/proxy/src/gateway/oidc.rs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// OIDC provider configuration. Stored once per platform (not per-route).
/// All routes that require auth share the same OIDC provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    /// The OIDC issuer URL (e.g., "https://keycloak.example.com/realms/my-realm")
    /// Used to fetch /.well-known/openid-configuration
    pub issuer_url: String,

    /// Expected audience (aud claim) in the JWT.
    /// Must match the client_id configured in Keycloak.
    pub audience: String,

    /// How often to refresh the JWKS cache (seconds).
    /// Default: 3600 (1 hour). Keycloak rotates keys infrequently.
    #[serde(default = "default_jwks_refresh_secs")]
    pub jwks_refresh_secs: u64,

    /// Clock skew tolerance (seconds). Allows JWTs that are slightly
    /// expired due to clock differences between nodes.
    #[serde(default = "default_clock_skew_secs")]
    pub clock_skew_secs: u64,
}

fn default_jwks_refresh_secs() -> u64 { 3600 }
fn default_clock_skew_secs() -> u64 { 30 }

/// Cached OIDC provider state.
pub struct OidcProvider {
    config: OidcConfig,

    /// The JWKS (JSON Web Key Set) used to verify JWT signatures.
    /// Fetched from <issuer_url>/protocol/openid-connect/certs
    /// and refreshed periodically.
    jwks: Arc<RwLock<JwksCache>>,

    /// HTTP client for fetching JWKS and OIDC discovery.
    http_client: reqwest::Client,
}

/// Cached JWKS with the last fetch time.
struct JwksCache {
    /// Map from key ID (kid) to the decoded public key.
    keys: HashMap<String, jsonwebtoken::DecodingKey>,

    /// When the JWKS was last fetched.
    fetched_at: std::time::Instant,
}

impl OidcProvider {
    pub fn new(config: OidcConfig) -> Self {
        OidcProvider {
            config,
            jwks: Arc::new(RwLock::new(JwksCache {
                keys: HashMap::new(),
                fetched_at: std::time::Instant::now()
                    - std::time::Duration::from_secs(3601), // force initial fetch
            })),
            http_client: reqwest::Client::new(),
        }
    }

    /// Start the background JWKS refresh loop.
    pub fn start_refresh_loop(self: Arc<Self>) {
        let provider = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(provider.config.jwks_refresh_secs)
            );
            loop {
                interval.tick().await;
                if let Err(e) = provider.refresh_jwks().await {
                    tracing::error!(error = %e, "failed to refresh JWKS");
                }
            }
        });
    }

    /// Fetch the JWKS from the OIDC provider.
    async fn refresh_jwks(&self) -> Result<(), GatewayError> {
        // Build the JWKS URL from the issuer URL
        // Keycloak: <issuer_url>/protocol/openid-connect/certs
        // Standard: <issuer_url>/jwks.json (from discovery document)
        let jwks_url = format!(
            "{}/protocol/openid-connect/certs",
            self.config.issuer_url.trim_end_matches('/')
        );

        let resp = self.http_client.get(&jwks_url)
            .send().await
            .map_err(|e| GatewayError::Oidc(format!("JWKS fetch failed: {e}")))?;

        let jwks_json: serde_json::Value = resp.json().await
            .map_err(|e| GatewayError::Oidc(format!("JWKS parse failed: {e}")))?;

        let mut keys = HashMap::new();
        if let Some(key_array) = jwks_json.get("keys").and_then(|k| k.as_array()) {
            for key_json in key_array {
                if let (Some(kid), Some(kty), Some(n), Some(e)) = (
                    key_json.get("kid").and_then(|v| v.as_str()),
                    key_json.get("kty").and_then(|v| v.as_str()),
                    key_json.get("n").and_then(|v| v.as_str()),
                    key_json.get("e").and_then(|v| v.as_str()),
                ) {
                    if kty == "RSA" {
                        // Build the JWK JSON in the format jsonwebtoken expects
                        let jwk = serde_json::json!({
                            "kty": kty,
                            "kid": kid,
                            "n": n,
                            "e": e,
                            "alg": "RS256"
                        });
                        if let Ok(decoding_key) = jsonwebtoken::DecodingKey::from_rsa_jwk(&serde_json::from_value(jwk).unwrap()) {
                            keys.insert(kid.to_string(), decoding_key);
                        }
                    }
                }
            }
        }

        let mut cache = self.jwks.write().await;
        let key_count = keys.len();
        cache.keys = keys;
        cache.fetched_at = std::time::Instant::now();
        tracing::info!(key_count, "JWKS refreshed from OIDC provider");

        Ok(())
    }

    /// Validate a JWT token and extract the user identity.
    pub async fn validate_token(&self, token: &str) -> Result<UserIdentity, GatewayError> {
        // 1. Decode the JWT header to get the key ID (kid)
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| GatewayError::Auth(format!("invalid JWT header: {e}")))?;
        let kid = header.kid
            .ok_or_else(|| GatewayError::Auth("JWT missing kid header".to_string()))?;

        // 2. Look up the signing key
        let cache = self.jwks.read().await;

        // If cache is stale, force a refresh
        if cache.fetched_at.elapsed().as_secs() > self.config.jwks_refresh_secs * 2 {
            drop(cache); // release read lock
            self.refresh_jwks().await?;
            // Re-acquire read lock after refresh
            let cache = self.jwks.read().await;
            return self.validate_with_cache(token, kid, &cache).await;
        }

        self.validate_with_cache(token, kid, &cache).await
    }

    async fn validate_with_cache(
        &self,
        token: &str,
        kid: String,
        cache: &JwksCache,
    ) -> Result<UserIdentity, GatewayError> {
        let decoding_key = cache.keys.get(&kid)
            .ok_or_else(|| GatewayError::Auth(format!("unknown key ID: {}", kid)))?;

        // 3. Validate signature, expiry, and audience
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&[&self.config.audience]);
        validation.set_issuer(&[&self.config.issuer_url]);
        validation.leeway = self.config.clock_skew_secs;

        let token_data = jsonwebtoken::decode::<serde_json::Value>(token, decoding_key, &validation)
            .map_err(|e| GatewayError::Auth(format!("JWT validation failed: {e}")))?;

        // 4. Extract user identity from claims
        let claims = &token_data.claims;
        let sub = claims.get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let email = claims.get("email")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Extract roles from realm_access.roles or resource_access.<client>.roles
        let realm_roles = claims.get("realm_access")
            .and_then(|ra| ra.get("roles"))
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>())
            .unwrap_or_default();

        let client_roles = claims.get("resource_access")
            .and_then(|ra| ra.as_object())
            .map(|obj| {
                obj.iter().flat_map(|(client_id, client_val)| {
                    client_val.get("roles")
                        .and_then(|r| r.as_array())
                        .map(|arr| arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|role| format!("{}:{}", client_id, role))
                }).collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let all_roles: Vec<String> = realm_roles.into_iter()
            .chain(client_roles.into_iter())
            .collect();

        Ok(UserIdentity {
            sub,
            email,
            roles: all_roles,
            raw_claims: token_data.claims.clone(),
        })
    }
}

/// Extracted user identity from a validated JWT.
#[derive(Debug, Clone)]
pub struct UserIdentity {
    /// Subject — unique user identifier (Keycloak user ID).
    pub sub: String,

    /// Email (if present in token).
    pub email: Option<String>,

    /// All roles (realm + client-scoped).
    pub roles: Vec<String>,

    /// Raw JWT claims for custom extraction.
    pub raw_claims: serde_json::Value,
}
```

### Authorization Check

```rust
// crates/proxy/src/gateway/authz.rs

use super::config::AuthPolicy;
use super::oidc::UserIdentity;

/// Check if a user identity satisfies the route's auth policy.
pub fn authorize(identity: &UserIdentity, policy: &AuthPolicy) -> bool {
    match policy {
        AuthPolicy::None => true,
        AuthPolicy::Authenticated => true, // token already validated
        AuthPolicy::Roles { allowed_roles, client_id } => {
            // Check if the user has any of the allowed roles
            allowed_roles.iter().any(|required| {
                // Check realm roles (plain name)
                identity.roles.iter().any(|r| r == required)
                // Also check client-scoped roles (client_id:role_name)
                || client_id.as_ref().map_or(false, |cid| {
                    identity.roles.iter().any(|r| r == &format!("{}:{}", cid, required))
                })
            })
        }
    }
}
```

---

## 4. Distributed Rate Limiting via NATS KV

### The Problem with Node-Local Rate Limiting

Current state (Step 24): each node has its own `DashMap<String, TokenBucket>`.
If the limit is 1000 req/s and there are 3 nodes, the effective limit is 3000
req/s. An attacker sending 2500 req/s to a single node would be blocked, but
distributing 900 req/s to each node would pass all checks.

### Two-Tier Strategy

```
┌──────────────────────────────────────────────────────┐
│ Tier 1: Local Token Bucket (sub-microsecond)         │
│                                                       │
│  Each node maintains a local token bucket that        │
│  refills at (global_rate / node_count) per second.    │
│  This handles 99.9% of requests without any network   │
│  call.                                                │
│                                                       │
│  Example: 1000 req/s global / 3 nodes = 333 req/s    │
│  per node locally.                                    │
├──────────────────────────────────────────────────────┤
│ Tier 2: NATS KV Reconciliation (every 100ms)         │
│                                                       │
│  Periodically, each node reports its local counter    │
│  to NATS KV and reads the cluster-wide total.         │
│  If the cluster total exceeds the global limit,       │
│  the local refill rate is reduced to compensate.      │
│                                                       │
│  This provides eventual consistency — a burst may     │
│  briefly exceed the limit, but the system converges   │
│  within ~200ms.                                       │
└──────────────────────────────────────────────────────┘
```

### Implementation

```rust
// crates/proxy/src/gateway/distributed_limiter.rs

use async_nats::jetstream::kv::Store as KvStore;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Distributed rate limiter using NATS KV for cross-node coordination.
pub struct DistributedRateLimiter {
    /// Local token bucket for the hot path.
    local: Arc<tokio::sync::Mutex<LocalBucket>>,

    /// NATS KV store for cluster-wide counter sync.
    kv: Arc<RwLock<Option<KvStore>>>,

    /// This node's ID (used as the KV key suffix).
    node_id: String,

    /// The app this limiter is for.
    app_id: String,

    /// Configuration.
    config: DistributedRateLimitConfig,
}

#[derive(Debug, Clone)]
pub struct DistributedRateLimitConfig {
    /// Global requests per second across all nodes.
    pub global_rps: u32,

    /// Burst capacity per node.
    pub per_node_burst: u32,

    /// How often to sync with NATS KV (milliseconds).
    pub sync_interval_ms: u64,

    /// NATS KV bucket name.
    pub kv_bucket: String,
}

struct LocalBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second (global_rps / node_count)
    last_refill: Instant,
    consumed_since_sync: u64,
}

impl DistributedRateLimiter {
    pub fn new(app_id: String, node_id: String, config: DistributedRateLimitConfig) -> Self {
        // Start with a conservative refill rate; adjusted after first KV sync
        let initial_refill_rate = config.global_rps as f64 / 3.0; // assume 3 nodes
        DistributedRateLimiter {
            local: Arc::new(tokio::sync::Mutex::new(LocalBucket {
                tokens: config.per_node_burst as f64,
                max_tokens: config.per_node_burst as f64,
                refill_rate: initial_refill_rate,
                last_refill: Instant::now(),
                consumed_since_sync: 0,
            })),
            kv: Arc::new(RwLock::new(None)),
            node_id,
            app_id,
            config,
        }
    }

    /// Set the NATS KV store (called after NATS connection is established).
    pub async fn set_kv_store(&self, store: KvStore) {
        *self.kv.write().await = Some(store);
    }

    /// Check if a request is allowed. Fast path: local token bucket only.
    pub async fn check_request(&self) -> bool {
        let mut bucket = self.local.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.last_refill = now;

        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_rate)
            .min(bucket.max_tokens);

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.consumed_since_sync += 1;
            true
        } else {
            false
        }
    }

    /// Start the background sync loop.
    pub fn start_sync_loop(self: Arc<Self>) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                Duration::from_millis(limiter.config.sync_interval_ms)
            );
            loop {
                interval.tick().await;
                if let Err(e) = limiter.sync_with_cluster().await {
                    tracing::warn!(
                        app = %limiter.app_id,
                        error = %e,
                        "distributed rate limit sync failed"
                    );
                }
            }
        });
    }

    /// Sync local counter with NATS KV and adjust refill rate.
    async fn sync_with_cluster(&self) -> Result<(), String> {
        let kv = self.kv.read().await;
        let kv = kv.as_ref().ok_or("NATS KV not initialized")?;

        // 1. Write our local counter to KV
        let key = format!("ratelimit:{}:{}", self.app_id, self.node_id);
        let bucket = self.local.lock().await;
        let consumed = bucket.consumed_since_sync;
        drop(bucket);

        let payload = serde_json::to_string(&RateLimitEntry {
            node_id: self.node_id.clone(),
            consumed,
            timestamp: chrono::Utc::now().timestamp_millis(),
        }).map_err(|e| format!("serialize: {e}"))?;

        // Use put (not update) — we always overwrite our own entry
        kv.put(key, payload.into()).await
            .map_err(|e| format!("kv put: {e}"))?;

        // 2. Read all entries for this app to compute cluster-wide consumption
        let prefix = format!("ratelimit:{}:", self.app_id);
        let entries = kv.keys().await
            .map_err(|e| format!("kv keys: {e}"))?;

        let mut total_consumed: u64 = 0;
        let mut node_count: u64 = 0;
        let now = chrono::Utc::now().timestamp_millis();
        let stale_threshold = (self.config.sync_interval_ms as i64) * 5; // 5x sync interval

        for key in entries {
            if !key.starts_with(&prefix) { continue; }
            if let Ok(Some(value)) = kv.get(&key).await {
                if let Ok(entry) = serde_json::from_slice::<RateLimitEntry>(&value) {
                    // Ignore stale entries (node may be down)
                    if now - entry.timestamp < stale_threshold {
                        total_consumed += entry.consumed;
                        node_count += 1;
                    }
                }
            }
        }

        // 3. Adjust local refill rate based on cluster size
        node_count = node_count.max(1); // at least 1 node
        let fair_share_rps = self.config.global_rps as f64 / node_count as f64;

        let mut bucket = self.local.lock().await;
        bucket.refill_rate = fair_share_rps;
        bucket.consumed_since_sync = 0; // reset for next interval

        tracing::debug!(
            app = %self.app_id,
            nodes = node_count,
            total_consumed,
            fair_share_rps,
            "distributed rate limit sync"
        );

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RateLimitEntry {
    node_id: String,
    consumed: u64,
    timestamp: i64,
}
```

### NATS KV Bucket Setup

```rust
// In the node startup code, create the rate-limit KV bucket:

async fn setup_rate_limit_kv(client: &async_nats::Client) -> Result<KvStore, PlatformError> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let kv = jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "rate_limits".to_string(),
            max_age: Duration::from_secs(10), // entries auto-expire
            history: 1,
            ..Default::default()
        })
        .await
        .map_err(|e| PlatformError::runtime(format!("NATS KV create: {e}")))?;
    Ok(kv)
}
```

---

## 5. CORS Middleware

```rust
// crates/proxy/src/gateway/cors.rs

use super::config::CorsPolicy;
use pingora_core::Result as PingoraResult;
use pingora_proxy::{Session, ProxyHttp};

impl<C> WasmProxy
where C: ProxyHttp<CTX = RequestCtx>
{
    /// Handle a CORS preflight (OPTIONS) request.
    /// Returns true to abort the request (we send the response ourselves).
    pub async fn handle_cors_preflight(
        &self,
        session: &mut Session,
        route_config: &GatewayRouteConfig,
    ) -> PingoraResult<bool> {
        let cors = match &route_config.cors {
            Some(c) => c,
            None => return Ok(false), // no CORS config, pass through
        };

        let origin = session.req_header().headers
            .get("origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        // Check if the origin is allowed
        if !is_origin_allowed(origin, cors) {
            session.respond_error(403).await?;
            return Ok(true);
        }

        // Build CORS response headers
        let resp_header = session.response_written()
            .map(|r| r.headers.clone())
            .unwrap_or_default();

        // Write 200 OK with CORS headers
        let mut resp = pingora::http::Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", origin)
            .header("Access-Control-Allow-Methods", cors.allowed_methods.join(", "))
            .header("Access-Control-Allow-Headers", cors.allowed_headers.join(", "))
            .header("Access-Control-Max-Age", cors.max_age_secs.to_string());

        if cors.allow_credentials {
            resp = resp.header("Access-Control-Allow-Credentials", "true");
        }

        if !cors.expose_headers.is_empty() {
            resp = resp.header("Access-Control-Expose-Headers", cors.expose_headers.join(", "));
        }

        // Send the preflight response
        let body = resp.body(String::new())
            .map_err(|e| pingora_core::Error::because(
                pingora_core::ErrorType::InternalError,
                "CORS response build",
                e,
            ))?;
        session.write_response_header(Box::new(body), true).await?;

        Ok(true) // abort — we've sent the response
    }

    /// Add CORS headers to a normal (non-preflight) response.
    pub fn add_cors_headers(
        &self,
        upstream_response: &mut pingora::http::ResponseHeader,
        route_config: &GatewayRouteConfig,
        origin: &str,
    ) {
        let cors = match &route_config.cors {
            Some(c) => c,
            None => return,
        };

        if !is_origin_allowed(origin, cors) {
            return;
        }

        let _ = upstream_response.insert_header("Access-Control-Allow-Origin", origin);
        let _ = upstream_response.insert_header(
            "Access-Control-Allow-Methods",
            cors.allowed_methods.join(", "),
        );
        let _ = upstream_response.insert_header(
            "Access-Control-Allow-Headers",
            cors.allowed_headers.join(", "),
        );

        if cors.allow_credentials {
            let _ = upstream_response.insert_header("Access-Control-Allow-Credentials", "true");
        }

        if !cors.expose_headers.is_empty() {
            let _ = upstream_response.insert_header(
                "Access-Control-Expose-Headers",
                cors.expose_headers.join(", "),
            );
        }
    }
}

fn is_origin_allowed(origin: &str, cors: &CorsPolicy) -> bool {
    if cors.allowed_origins.contains(&"*".to_string()) {
        return true;
    }
    cors.allowed_origins.iter().any(|allowed| {
        allowed == origin
            || (allowed.starts_with("*.") && origin.ends_with(&allowed[1..]))
    })
}
```

---

## 6. Circuit Breaker

```rust
// crates/proxy/src/gateway/circuit_breaker.rs

use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Circuit state for a single upstream app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally.
    Closed,
    /// Requests are rejected immediately (too many recent failures).
    Open,
    /// A single probe request is allowed through to test recovery.
    HalfOpen,
}

/// Per-app circuit breaker state.
struct Circuit {
    state: CircuitState,
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
    last_state_change: Instant,
}

/// Circuit breaker manager for all upstream apps.
pub struct CircuitBreakerManager {
    circuits: DashMap<String, Circuit>,
    /// Default config for apps without explicit circuit breaker config.
    default_failure_threshold: u32,
    default_reset_timeout_secs: u32,
}

impl CircuitBreakerManager {
    pub fn new() -> Self {
        CircuitBreakerManager {
            circuits: DashMap::new(),
            default_failure_threshold: 5,
            default_reset_timeout_secs: 30,
        }
    }

    /// Check if the circuit is open (requests should be rejected).
    pub fn is_circuit_open(&self, app_id: &str) -> bool {
        let mut circuit = self.circuits.entry(app_id.to_string())
            .or_insert_with(|| Circuit {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                last_failure_at: None,
                last_state_change: Instant::now(),
            });

        match circuit.state {
            CircuitState::Closed => false,
            CircuitState::Open => {
                // Check if enough time has passed to try half-open
                let elapsed = circuit.last_state_change.elapsed().as_secs();
                if elapsed >= self.default_reset_timeout_secs as u64 {
                    circuit.state = CircuitState::HalfOpen;
                    circuit.last_state_change = Instant::now();
                    tracing::info!(app = app_id, "circuit breaker: OPEN → HALF-OPEN");
                    false // allow the probe request
                } else {
                    true
                }
            }
            CircuitState::HalfOpen => false, // allow probe request
        }
    }

    /// Record a successful response from the upstream.
    pub fn record_success(&self, app_id: &str) {
        if let Some(mut circuit) = self.circuits.get_mut(app_id) {
            if circuit.state == CircuitState::HalfOpen {
                circuit.state = CircuitState::Closed;
                circuit.consecutive_failures = 0;
                circuit.last_state_change = Instant::now();
                tracing::info!(app = app_id, "circuit breaker: HALF-OPEN → CLOSED (recovered)");
            } else {
                circuit.consecutive_failures = 0;
            }
        }
    }

    /// Record a failure response from the upstream.
    pub fn record_failure(&self, app_id: &str) {
        if let Some(mut circuit) = self.circuits.get_mut(app_id) {
            circuit.consecutive_failures += 1;
            circuit.last_failure_at = Some(Instant::now());

            match circuit.state {
                CircuitState::Closed => {
                    if circuit.consecutive_failures >= self.default_failure_threshold {
                        circuit.state = CircuitState::Open;
                        circuit.last_state_change = Instant::now();
                        tracing::warn!(
                            app = app_id,
                            failures = circuit.consecutive_failures,
                            "circuit breaker: CLOSED → OPEN (too many failures)"
                        );
                    }
                }
                CircuitState::HalfOpen => {
                    // Probe failed — go back to open
                    circuit.state = CircuitState::Open;
                    circuit.last_state_change = Instant::now();
                    tracing::warn!(app = app_id, "circuit breaker: HALF-OPEN → OPEN (probe failed)");
                }
                CircuitState::Open => {} // already open, nothing to do
            }
        }
    }
}
```

---

## 7. Request Transformation

```rust
// crates/proxy/src/gateway/transform.rs

use super::config::RequestTransform;
use pingora::http::RequestHeader;

/// Apply request transformations before forwarding to upstream.
pub fn apply_request_transform(
    request: &mut RequestHeader,
    transform: &RequestTransform,
    user_identity: Option<&super::oidc::UserIdentity>,
) {
    // 1. Inject user identity headers (always, when authenticated)
    if let Some(identity) = user_identity {
        let _ = request.insert_header("X-User-Id", &identity.sub);
        if let Some(ref email) = identity.email {
            let _ = request.insert_header("X-User-Email", email);
        }
        if !identity.roles.is_empty() {
            let _ = request.insert_header("X-User-Roles", identity.roles.join(","));
        }
    }

    // 2. Add custom headers from route config
    for (key, value) in &transform.add_headers {
        let _ = request.insert_header(key.as_str(), value.as_str());
    }

    // 3. Remove headers from route config
    for key in &transform.remove_headers {
        request.remove_header(key.as_str());
    }

    // 4. Path prefix injection
    if let Some(ref prefix) = transform.path_prefix {
        let original = request.uri.to_string();
        let new_path = format!("{}{}", prefix.trim_end_matches('/'), original);
        if let Ok(parsed) = new_path.parse() {
            request.uri = parsed;
        }
    }

    // 5. Strip query parameters
    if !transform.strip_query_params.is_empty() {
        let original = request.uri.to_string();
        if let Some((path, query)) = original.split_once('?') {
            let remaining: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let key = pair.split('=').next().unwrap_or("");
                    !transform.strip_query_params.iter().any(|s| s == key)
                })
                .collect();
            let new_uri = if remaining.is_empty() {
                path.to_string()
            } else {
                format!("{}?{}", path, remaining.join("&"))
            };
            if let Ok(parsed) = new_uri.parse() {
                request.uri = parsed;
            }
        }
    }
}
```

---

## 8. Gateway Module Structure

```
crates/proxy/src/gateway/
├── mod.rs              — Gateway struct, middleware pipeline orchestration
├── config.rs           — GatewayRouteConfig, AuthPolicy, CorsPolicy, etc.
├── oidc.rs             — OIDC provider, JWKS cache, JWT validation
├── authz.rs            — Role-based authorization checks
├── cors.rs             — CORS preflight handling and response header injection
├── distributed_limiter.rs — NATS KV-based distributed rate limiting
├── circuit_breaker.rs  — Per-app circuit breaker
├── transform.rs        — Request transformation (headers, path, query)
└── metrics.rs          — Gateway-specific Prometheus metrics
```

```rust
// crates/proxy/src/gateway/mod.rs

pub mod config;
pub mod oidc;
pub mod authz;
pub mod cors;
pub mod distributed_limiter;
pub mod circuit_breaker;
pub mod transform;
pub mod metrics;

use config::GatewayRouteConfig;
use oidc::OidcProvider;
use circuit_breaker::CircuitBreakerManager;
use distributed_limiter::DistributedRateLimiter;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The API gateway. Owns all middleware state and orchestrates the pipeline.
pub struct Gateway {
    /// OIDC provider for JWT validation. None = auth disabled globally.
    pub oidc: Option<Arc<OidcProvider>>,

    /// Circuit breaker manager for all upstream apps.
    pub circuit_breaker: Arc<CircuitBreakerManager>,

    /// Per-app distributed rate limiters.
    pub distributed_limiters: Arc<RwLock<HashMap<String, Arc<DistributedRateLimiter>>>>,

    /// Per-route gateway configurations.
    pub route_configs: Arc<RwLock<HashMap<String, GatewayRouteConfig>>>,

    /// Gateway metrics.
    pub metrics: Arc<metrics::GatewayMetrics>,
}

impl Gateway {
    pub fn new(oidc: Option<Arc<OidcProvider>>) -> Self {
        Gateway {
            oidc,
            circuit_breaker: Arc::new(CircuitBreakerManager::new()),
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
        route_config: &GatewayRouteConfig,
    ) -> Result<oidc::UserIdentity, GatewayError> {
        let provider = self.oidc.as_ref()
            .ok_or(GatewayError::Auth("OIDC not configured".to_string()))?;

        let token = session.req_header().headers
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
```

---

## 9. Gateway Metrics

```rust
// crates/proxy/src/gateway/metrics.rs

use prometheus::{IntCounter, IntGauge, Opts, Registry};
use std::sync::Arc;

/// Prometheus metrics for the API gateway.
pub struct GatewayMetrics {
    /// Total requests that passed authentication.
    pub auth_success_total: IntCounter,

    /// Total requests that failed authentication.
    pub auth_failure_total: IntCounter,

    /// Total requests rejected by authorization (wrong roles).
    pub authz_denied_total: IntCounter,

    /// Total CORS preflight requests handled.
    pub cors_preflight_total: IntCounter,

    /// Total requests rejected by distributed rate limiter.
    pub rate_limit_denied_total: IntCounter,

    /// Total requests rejected by circuit breaker.
    pub circuit_breaker_rejected_total: IntCounter,

    /// Currently open circuits.
    pub circuits_open: IntGauge,

    /// JWKS refresh count.
    pub jwks_refresh_total: IntCounter,

    /// JWKS refresh failures.
    pub jwks_refresh_failures: IntCounter,
}

impl GatewayMetrics {
    pub fn new() -> Self {
        // Use the global Prometheus default registry for simplicity.
        // In production, share the registry from the metrics crate.
        let registry = Registry::new();

        let auth_success_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_auth_success_total",
            "Requests that passed authentication",
        )).unwrap();
        registry.register(Box::new(auth_success_total.clone())).unwrap();

        let auth_failure_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_auth_failure_total",
            "Requests that failed authentication",
        )).unwrap();
        registry.register(Box::new(auth_failure_total.clone())).unwrap();

        let authz_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_authz_denied_total",
            "Requests denied by authorization (wrong roles)",
        )).unwrap();
        registry.register(Box::new(authz_denied_total.clone())).unwrap();

        let cors_preflight_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_cors_preflight_total",
            "CORS preflight requests handled",
        )).unwrap();
        registry.register(Box::new(cors_preflight_total.clone())).unwrap();

        let rate_limit_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_rate_limit_denied_total",
            "Requests denied by distributed rate limiter",
        )).unwrap();
        registry.register(Box::new(rate_limit_denied_total.clone())).unwrap();

        let circuit_breaker_rejected_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_circuit_breaker_rejected_total",
            "Requests rejected by circuit breaker",
        )).unwrap();
        registry.register(Box::new(circuit_breaker_rejected_total.clone())).unwrap();

        let circuits_open = IntGauge::with_opts(Opts::new(
            "wasm_gateway_circuits_open",
            "Currently open circuit breakers",
        )).unwrap();
        registry.register(Box::new(circuits_open.clone())).unwrap();

        let jwks_refresh_total = IntCounter::with_opts(Opts::new(
            "wasm_gateway_jwks_refresh_total",
            "JWKS cache refresh attempts",
        )).unwrap();
        registry.register(Box::new(jwks_refresh_total.clone())).unwrap();

        let jwks_refresh_failures = IntCounter::with_opts(Opts::new(
            "wasm_gateway_jwks_refresh_failures",
            "JWKS cache refresh failures",
        )).unwrap();
        registry.register(Box::new(jwks_refresh_failures.clone())).unwrap();

        GatewayMetrics {
            auth_success_total,
            auth_failure_total,
            authz_denied_total,
            cors_preflight_total,
            rate_limit_denied_total,
            circuit_breaker_rejected_total,
            circuits_open,
            jwks_refresh_total,
            jwks_refresh_failures,
        }
    }
}
```

---

## 10. Updated RequestCtx and WasmProxy

```rust
// crates/proxy/src/service.rs — updated types

use crate::gateway::{Gateway, GatewayRouteConfig, oidc::UserIdentity};

/// Context passed through the Pingora request pipeline.
pub struct RequestCtx {
    pub app_id: Option<AppId>,
    pub upstream_addr: Option<std::net::SocketAddr>,
    pub start: std::time::Instant,
    pub strip_prefix: bool,
    pub matched_prefix: Option<String>,
    pub trace_id: Option<String>,

    // ── New gateway fields ──────────────────────────────────────
    /// Gateway configuration for the matched route.
    pub route_config: Option<GatewayRouteConfig>,

    /// Authenticated user identity (set by auth middleware).
    pub user_identity: Option<UserIdentity>,
}

/// The main Pingora proxy service — now with gateway capabilities.
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    pub rate_limiter: Arc<RateLimiter>,
    pub backpressure: BackpressureSignal,
    pub node_table: Arc<NodeLoadTable>,
    pub metrics: Option<Arc<RateLimitMetrics>>,

    // ── New gateway field ───────────────────────────────────────
    /// The API gateway (auth, CORS, circuit breaker, transforms).
    pub gateway: Arc<Gateway>,

    pub cold_start: Arc<
        dyn Fn(AppId) -> futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
            + Send
            + Sync,
    >,
}
```

---

## 11. CLI Commands

```
# Configure OIDC provider for the platform
wasm-ctl gateway setup-oidc \
  --issuer-url https://keycloak.example.com/realms/my-realm \
  --audience my-platform-api

# Set gateway config for a route
wasm-ctl gateway set-auth api-users:v2 --policy roles --roles admin,user --client-id api-users
wasm-ctl gateway set-cors api-users:v2 --origins "https://app.example.com" --credentials
wasm-ctl gateway set-rate-limit api-users:v2 --rps 500 --burst 100 --distributed
wasm-ctl gateway set-circuit-breaker api-users:v2 --failure-threshold 5 --reset-timeout 30

# View gateway config for a route
wasm-ctl gateway show api-users:v2

# Remove gateway config for a route (reverts to public)
wasm-ctl gateway reset api-users:v2

# List all routes with auth enabled
wasm-ctl gateway list-authenticated
```

### Deploy with Gateway Config

```
# Deploy with authentication
wasm-ctl deploy \
  --app api-users \
  --version v2 \
  --wasm api-users.wasm \
  --gateway-auth roles \
  --gateway-roles admin,user \
  --gateway-oidc-client api-users \
  --gateway-cors-origins "https://app.example.com" \
  --gateway-cors-credentials \
  --gateway-rps 500 \
  --gateway-rps-distributed

# Deploy public API
wasm-ctl deploy \
  --app public-api \
  --version v1 \
  --wasm public-api.wasm \
  --gateway-cors-origins "*"
```

---

## 12. Storage Schema

### Gateway Config Table

The gateway route configs are stored in the existing redb storage alongside
routes and app configs.

```rust
// crates/storage/src/store.rs — new table definition

/// Table: "gateway_configs"
/// Key: app_id (String) — e.g., "api-users:v2"
/// Value: GatewayRouteConfig (JSON-serialized)

impl Store {
    pub fn save_gateway_config(&self, app_id: &str, config: &GatewayRouteConfig) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(GatewayConfigTable)?;
            let json = serde_json::to_string(config)?;
            table.insert(app_id, json)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn load_gateway_config(&self, app_id: &str) -> Result<Option<GatewayRouteConfig>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(GatewayConfigTable)?;
        Ok(table.get(app_id)?.and_then(|v| {
            serde_json::from_str(v.value()).ok()
        }))
    }

    pub fn list_gateway_configs(&self) -> Result<Vec<(String, GatewayRouteConfig)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(GatewayConfigTable)?;
        Ok(table.iter()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let app_id = entry.0.value().to_string();
                let config = serde_json::from_str(entry.1.value()).ok()?;
                Some((app_id, config))
            })
            .collect())
    }
}
```

---

## 13. OIDC Configuration in Node Config

The OIDC provider is configured at the node level (not per-route) because all
routes share the same Keycloak realm and JWKS endpoint.

```toml
# config/node.toml — new section

[gateway.oidc]
issuer_url = "https://keycloak.example.com/realms/my-realm"
audience = "my-platform-api"
jwks_refresh_secs = 3600
clock_skew_secs = 30

[gateway.rate_limit]
# Default distributed rate limit settings
kv_bucket = "rate_limits"
sync_interval_ms = 100

[gateway.circuit_breaker]
default_failure_threshold = 5
default_reset_timeout_secs = 30
```

---

## 14. Error Responses

The gateway returns structured JSON error responses so that API consumers
can programmatically handle failures.

```rust
// crates/proxy/src/gateway/errors.rs

use pingora_proxy::Session;

/// Send a gateway error response with a structured JSON body.
pub async fn send_gateway_error(
    session: &mut Session,
    status: u16,
    error_code: &str,
    message: &str,
) -> pingora_core::Result<bool> {
    let body = serde_json::json!({
        "error": error_code,
        "message": message,
        "status": status,
    });

    let resp = pingora::http::Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .map_err(|e| pingora_core::Error::because(
            pingora_core::ErrorType::InternalError,
            "gateway error response",
            e,
        ))?;

    session.write_response_header(Box::new(resp), true).await?;
    Ok(true) // abort the request
}

// Standard error codes:
// 401 — {"error": "unauthorized", "message": "missing or invalid JWT token"}
// 403 — {"error": "forbidden", "message": "user lacks required role: admin"}
// 429 — {"error": "rate_limit_exceeded", "message": "app rate limit: 500 req/s"}
// 503 — {"error": "circuit_open", "message": "upstream is unhealthy, retry later"}
// 403 — {"error": "cors_forbidden", "message": "origin not allowed: https://evil.com"}
```

---

## 15. Testing Strategy

### Unit Tests

```bash
cargo test -p proxy --lib  # Gateway middleware logic
```

Tests to implement:
- `test_auth_policy_none_allows_all` — no auth = no token needed
- `test_auth_policy_authenticated_requires_token` — missing token → 401
- `test_auth_policy_roles_check` — wrong roles → 403, correct roles → pass
- `test_jwt_validation_valid_token` — well-formed JWT passes validation
- `test_jwt_validation_expired_token` — expired JWT → 401
- `test_jwt_validation_wrong_audience` — wrong aud claim → 401
- `test_jwt_validation_wrong_signature` — tampered JWT → 401
- `test_jwks_cache_refresh` — JWKS refreshes on schedule
- `test_cors_preflight_allowed_origin` — OPTIONS with allowed origin → 200
- `test_cors_preflight_disallowed_origin` — OPTIONS with bad origin → 403
- `test_cors_response_headers` — normal response includes CORS headers
- `test_circuit_breaker_closed_to_open` — 5 failures → circuit opens
- `test_circuit_breaker_open_rejects` — open circuit → 503
- `test_circuit_breaker_half_open_recovery` — probe success → circuit closes
- `test_circuit_breaker_half_open_failure` — probe failure → circuit reopens
- `test_request_transform_add_headers` — custom headers injected
- `test_request_transform_remove_headers` — headers stripped
- `test_request_transform_path_prefix` — path rewritten
- `test_request_transform_strip_query` — query params removed
- `test_distributed_rate_limit_local_bucket` — local bucket works
- `test_distributed_rate_limit_kv_sync` — KV sync adjusts refill rate
- `test_user_identity_headers` — X-User-Id, X-User-Roles injected
- `test_is_origin_allowed_wildcard` — "*" matches everything
- `test_is_origin_allowed_subdomain` — "*.example.com" matches subdomains

### Integration Tests

```bash
cargo test -p proxy --tests  # With real Pingora + mock OIDC
```

Tests to implement:
- `test_full_gateway_pipeline_public_route` — no auth, no CORS → direct proxy
- `test_full_gateway_pipeline_authenticated_route` — JWT validated, identity forwarded
- `test_full_gateway_pipeline_cors_preflight` — OPTIONS handled without upstream
- `test_full_gateway_pipeline_circuit_breaker` — circuit opens after failures
- `test_full_gateway_pipeline_rate_limit_distributed` — NATS KV sync works

### E2E Tests

```bash
cargo test -p e2e -- --ignored --test-threads=1
```

Tests to implement:
- `test_keycloak_auth_flow` — deploy app with auth, get token from Keycloak,
  verify request passes with valid token, fails with invalid token
- `test_distributed_rate_limit_two_nodes` — deploy app with 100 req/s limit,
  send 80 req/s from each of 2 nodes, verify total is capped at ~100
- `test_circuit_breaker_recovery` — deploy crashing app, verify circuit opens,
  fix the app, verify circuit closes after reset timeout

---

## 16. Migration Path

### Phase 1: Gateway Infrastructure (Non-Breaking)

- Add `crates/proxy/src/gateway/` module with all types
- Add `Gateway` struct to `WasmProxy` (initially with no-op middleware)
- Add `GatewayRouteConfig` to storage schema
- Add `gateway` CLI subcommand
- **No behavior change** — all routes are public by default

### Phase 2: Authentication + Authorization

- Implement OIDC provider + JWKS cache
- Wire auth middleware into `request_filter()`
- Wire authorization checks
- Add user identity header injection
- Deploy with `--gateway-auth` flag

### Phase 3: Distributed Rate Limiting

- Implement `DistributedRateLimiter` with NATS KV
- Create `rate_limits` KV bucket on node startup
- Wire into `request_filter()` alongside existing local limiter
- Two-tier: local bucket for hot path, NATS KV for reconciliation

### Phase 4: CORS + Circuit Breaker + Transforms

- Implement CORS preflight handling
- Implement circuit breaker
- Implement request transformation
- Wire all into the middleware pipeline

### Phase 5: Observability + Hardening

- Gateway Prometheus metrics
- Prometheus alerting rules for auth failures, circuit opens
- Structured access logs with auth context
- OIDC provider health monitoring
- JWKS refresh failure alerting

---

## 17. Security Considerations

### JWT Validation Is Not Optional

When a route has `auth: Authenticated` or `auth: AuthPolicy::Roles`, the
gateway validates the JWT **before** the request reaches the Wasm app. The
app never sees an unauthenticated request. This is critical because:

1. Wasm apps may forget to validate tokens
2. Wasm apps may have bugs in their validation logic
3. The gateway provides a single, auditable enforcement point

### Token Forwarding

The gateway does **not** forward the raw `Authorization` header to the
upstream by default. Instead, it injects `X-User-Id`, `X-User-Email`, and
`X-User-Roles` headers. This prevents the Wasm app from accidentally
re-validating the token or forwarding it to a third party.

If the app needs the raw token (e.g., for token exchange), the route config
can explicitly enable it:

```toml
[app.gateway.transform]
add_headers = [["X-Forwarded-Token", "{{token}}"]]
```

### JWKS Cache Poisoning

The JWKS cache is fetched over HTTPS from the OIDC provider. If an attacker
can MITM the connection, they could inject their own signing key. Mitigations:

1. Always use HTTPS for the issuer URL (enforced in code)
2. Pin the CA certificate in the node config (optional)
3. Alert on JWKS refresh failures (monitoring)

### Rate Limit Bypass via Node Addition

When a new node joins the cluster, it starts with a local refill rate based
on the current node count. There is a brief window (~200ms) where the new
node's rate limit is not yet synchronized. This is acceptable because:

1. The window is very short
2. The total overshoot is bounded by one sync interval's worth of requests
3. The alternative (locking on every request) is too expensive

### CORS and Credentials

When `allow_credentials: true`, the `Access-Control-Allow-Origin` header
must be a specific origin (not `*`). The gateway enforces this — if the
route config has both `allowed_origins: ["*"]` and `allow_credentials: true`,
the gateway returns a config validation error at deploy time.

---

## 18. Dependencies

### New Crate Dependencies

```toml
# crates/proxy/Cargo.toml — additions

[dependencies]
# JWT validation
jsonwebtoken = "9"

# OIDC discovery (optional — we can build the URL ourselves)
# No additional crate needed — we use reqwest (already present)

# Chrono for timestamps in rate limit entries
chrono = { workspace = true }
```

### No New Infrastructure

| Component | Already Exists? | Purpose |
|-----------|----------------|---------|
| NATS KV | ✅ Yes (async-nats) | Distributed rate limit state |
| Pingora proxy | ✅ Yes | Gateway middleware host |
| redb storage | ✅ Yes | Gateway route configs |
| Prometheus | ✅ Yes | Gateway metrics |
| Keycloak | ❌ External | OIDC identity provider |

Keycloak is the only external dependency, and it's optional — routes without
auth don't need it. The platform works without Keycloak; it just can't
authenticate requests.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Gateway Infrastructure
- [ ] `crates/proxy/src/gateway/` module created with all submodules
- [ ] `Gateway` struct added to `WasmProxy`
- [ ] `GatewayRouteConfig` stored in redb alongside routes
- [ ] `gateway` CLI subcommand with `set-auth`, `set-cors`, `show`, `reset`
- [ ] Deploy flags: `--gateway-auth`, `--gateway-cors-origins`, `--gateway-rps`

### Authentication
- [ ] `OidcProvider` with JWKS cache and automatic refresh
- [ ] JWT validation: signature, expiry, audience, issuer
- [ ] `AuthPolicy::None` — no auth (default)
- [ ] `AuthPolicy::Authenticated` — valid JWT required
- [ ] `AuthPolicy::Roles` — JWT + role check
- [ ] User identity injected as `X-User-Id`, `X-User-Email`, `X-User-Roles`
- [ ] 401 response for missing/invalid tokens
- [ ] 403 response for insufficient roles
- [ ] OIDC provider configured via node config `[gateway.oidc]`

### Authorization
- [ ] Role extraction from `realm_access.roles` and `resource_access.<client>.roles`
- [ ] OR logic for allowed roles (any match passes)
- [ ] Client-scoped role support

### Distributed Rate Limiting
- [ ] `DistributedRateLimiter` with local token bucket + NATS KV sync
- [ ] NATS KV bucket `rate_limits` created on node startup
- [ ] Two-tier strategy: local bucket (hot path) + KV reconciliation (100ms)
- [ ] Fair-share refill rate adjustment based on cluster node count
- [ ] Stale entry cleanup via KV TTL (10 seconds)
- [ ] Fallback to local-only when NATS is unavailable

### CORS
- [ ] CORS preflight (OPTIONS) handled at proxy layer (no upstream hit)
- [ ] `Access-Control-Allow-Origin` from route config
- [ ] Wildcard origin support (`*`)
- [ ] Subdomain wildcard support (`*.example.com`)
- [ ] Credentials support with specific origin enforcement
- [ ] CORS headers added to normal responses

### Circuit Breaker
- [ ] Per-app circuit breaker with Closed → Open → HalfOpen → Closed states
- [ ] Configurable failure threshold and reset timeout
- [ ] 503 response when circuit is open
- [ ] Half-open probe request for recovery detection

### Request Transformation
- [ ] Custom header injection
- [ ] Header removal
- [ ] Path prefix injection
- [ ] Query parameter stripping
- [ ] User identity header injection (automatic for authenticated routes)

### Metrics
- [ ] `wasm_gateway_auth_success_total`
- [ ] `wasm_gateway_auth_failure_total`
- [ ] `wasm_gateway_authz_denied_total`
- [ ] `wasm_gateway_cors_preflight_total`
- [ ] `wasm_gateway_rate_limit_denied_total`
- [ ] `wasm_gateway_circuit_breaker_rejected_total`
- [ ] `wasm_gateway_circuits_open`
- [ ] `wasm_gateway_jwks_refresh_total`
- [ ] `wasm_gateway_jwks_refresh_failures`

### Error Handling
- [ ] Structured JSON error responses for all gateway rejections
- [ ] Standard error codes: `unauthorized`, `forbidden`, `rate_limit_exceeded`,
      `circuit_open`, `cors_forbidden`
- [ ] No internal details leaked in error messages

### Testing
- [ ] Unit tests for all middleware (25+ tests)
- [ ] Integration tests with mock OIDC server (5+ tests)
- [ ] E2E test: Keycloak auth flow
- [ ] E2E test: distributed rate limiting across 2 nodes
- [ ] E2E test: circuit breaker recovery

### Documentation
- [ ] Deploy manifest format updated with gateway fields
- [ ] OIDC setup guide (Keycloak realm, client, audience)
- [ ] CORS configuration examples
- [ ] Circuit breaker tuning guide
- [ ] Security considerations documented
