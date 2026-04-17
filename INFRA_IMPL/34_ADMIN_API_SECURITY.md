# Step 34 — Admin API Security & Authentication

## Goal
Implement authentication, authorization, and audit logging for the `wasm-node` admin
API. The system must:
- Require authentication for all admin API endpoints (config changes, rebuild, GC, etc.)
- Support bearer token authentication with configurable tokens
- Support separate tokens for read-only vs. read-write operations
- Rate-limit admin API requests to prevent brute-force token guessing
- Log every admin API call to the audit system with actor identity
- Require TLS for admin API in production (reject plaintext tokens over HTTP)
- Provide a health check endpoint that does NOT require authentication (for load balancers)
- Support token rotation without restarting the node
- Expose authentication failure metrics to Prometheus
- Require no external identity provider — the node is self-sufficient

---

## Context & Rationale

### The Problem This Solves

The admin API (Step 14, port 9090) currently has **zero authentication**. Anyone who can
reach port 9090 can:

- Delete the database (`POST /admin/rebuild`)
- Force garbage collection (`POST /admin/gc/force`)
- Change rate limits and thresholds (`PATCH /admin/config`)
- View all configuration including secrets paths (`GET /admin/config`)

In a production deployment, port 9090 must be exposed for:
- Prometheus metrics scraping (`GET /status/metrics`)
- Load balancer health checks (`GET /health`)
- Operator management via `wasm-ctl`

But only the health and metrics endpoints should be accessible without authentication.
All mutation endpoints and config-read endpoints must require proof of identity.

### Why Bearer Tokens (Not mTLS, Not OAuth2, Not JWT)

| Option     │ Complexity │ External Dep │ Revocation │ Suitable
|────────────┼────────────┼──────────────┼────────────┼────────────────
| Bearer token│ Low        │ None         │ Config reload│ Yes — simple, self-sufficient
| mTLS       │ High       │ CA infra     │ CRL/OCSP    │ Overkill for admin API
| OAuth2     │ High       │ IdP server   │ Token introspection│ Violates shared-nothing
| JWT        │ Medium     │ Key management│ Expiry only │ Revocation is hard
| Basic Auth │ Low        │ None         │ Config reload│ Credentials in every request

Bearer tokens are the simplest mechanism that provides meaningful security:

- **No external dependency**: The token is stored in the node's configuration
- **Easy to rotate**: Change the config file or hot-reload the token
- **Standard mechanism**: Every HTTP client supports `Authorization: Bearer <token>`
- **Revocable**: Remove the token from the config, and all existing holders are locked out
- **No credentials in URLs**: Unlike basic auth, tokens go in a header (not logged by proxies)

### Why Separate Read vs. Write Tokens

Operators need different privilege levels:

- **Monitoring systems** (Prometheus, Grafana) need read-only access to metrics and status
- **Operators** need read-write access to change config, trigger GC, rebuild nodes
- **CI/CD pipelines** need deploy-level access (specific scopes)

A single token means giving Prometheus the ability to delete the database. Separate
tokens enforce least privilege.

### Why Rate-Limit Admin Endpoints

Bearer tokens are vulnerable to brute-force guessing. An attacker who can reach port 9090
can try millions of token values. Rate limiting admin endpoints to 10 requests per second
per IP makes brute-force impractical:

- A 32-character hex token has 16^32 ≈ 3.4 × 10^38 possible values
- At 10 guesses/second, exhaustive search takes ~10^30 years
- Even a targeted attack (knowing the token format) is infeasible

### Why TLS Matters for Token Security

Bearer tokens sent over plaintext HTTP are visible to any network observer. If an
attacker can sniff traffic on port 9090, they capture the token and gain full admin
access. In production, the admin API MUST use TLS.

In development, TLS is optional (convenience). The node logs a warning on startup
if the admin API is listening on plaintext HTTP with authentication enabled.

---

---

## 1. Token Types & Configuration

### Token Configuration

```rust
// crates/common/src/auth.rs
use serde::{Deserialize, Serialize};

/// Authentication configuration for the admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Enable authentication on admin API endpoints.
    /// When disabled, all endpoints are accessible without a token.
    /// WARNING: Disabling auth in production is a security risk.
    #[serde(default)]
    pub enabled: bool,

    /// Read-only bearer token. Grants access to GET endpoints:
    /// /status/*, /admin/config (GET), /health, /metrics
    #[serde(default)]
    pub read_token: Option<String>,

    /// Read-write bearer token. Grants access to all endpoints including:
    /// /admin/rebuild, /admin/gc/force, /admin/config (PATCH/DELETE)
    #[serde(default)]
    pub write_token: Option<String>,

    /// Require TLS for admin API when authentication is enabled.
    /// If true and the admin API is on HTTP (not HTTPS), the node refuses to start.
    /// Set to false for development environments.
    #[serde(default = "default_require_tls")]
    pub require_tls: bool,

    /// Rate limit for admin API requests (requests per second per IP).
    /// Set to 0 to disable rate limiting.
    #[serde(default = "default_admin_rate_limit")]
    pub rate_limit_per_second: u32,

    /// Maximum burst for admin API rate limiting.
    #[serde(default = "default_admin_burst")]
    pub rate_limit_burst: u32,
}

fn default_require_tls() -> bool { true }
fn default_admin_rate_limit() -> u32 { 10 }
fn default_admin_burst() -> u32 { 20 }

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            enabled: false, // Off by default for backward compatibility
            read_token: None,
            write_token: None,
            require_tls: true,
            rate_limit_per_second: default_admin_rate_limit(),
            rate_limit_burst: default_admin_burst(),
        }
    }
}

/// Permission levels for admin API access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    /// No access — authentication failed.
    None = 0,
    /// Read-only access: GET endpoints only.
    Read = 1,
    /// Read-write access: all endpoints.
    Write = 2,
}

/// Result of authenticating a request.
#[derive(Debug)]
pub struct AuthResult {
    pub permission: Permission,
    pub token_type: TokenType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    ReadToken,
    WriteToken,
}

impl AuthConfig {
    /// Authenticate a request's Authorization header.
    /// Returns the granted permission level.
    pub fn authenticate(&self, auth_header: Option<&str>) -> AuthResult {
        if !self.enabled {
            // Auth disabled: everyone gets write access (backward compatible)
            return AuthResult {
                permission: Permission::Write,
                token_type: TokenType::WriteToken,
            };
        }

        let token = match Self::extract_bearer_token(auth_header) {
            Some(t) => t,
            None => {
                return AuthResult {
                    permission: Permission::None,
                    token_type: TokenType::ReadToken, // Placeholder
                };
            }
        };

        // Check write token first (higher privilege)
        if let Some(ref write) = self.write_token {
            if crate::crypto::constant_time_eq(token.as_bytes(), write.as_bytes()) {
                return AuthResult {
                    permission: Permission::Write,
                    token_type: TokenType::WriteToken,
                };
            }
        }

        // Check read token
        if let Some(ref read) = self.read_token {
            if crate::crypto::constant_time_eq(token.as_bytes(), read.as_bytes()) {
                return AuthResult {
                    permission: Permission::Read,
                    token_type: TokenType::ReadToken,
                };
            }
        }

        AuthResult {
            permission: Permission::None,
            token_type: TokenType::ReadToken,
        }
    }

    /// Extract the bearer token from an Authorization header value.
    /// Expected format: "Bearer <token>"
    fn extract_bearer_token(header: Option<&str>) -> Option<String> {
        let header = header?;
        let prefix = "Bearer ";
        if header.starts_with(prefix) {
            Some(header[prefix.len()..].trim().to_string())
        } else {
            None
        }
    }

    /// Validate the auth configuration at startup.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        // At least one token must be set
        if self.read_token.is_none() && self.write_token.is_none() {
            return Err("auth is enabled but no tokens are configured. \
                        Set auth.read_token and/or auth.write_token in config.".to_string());
        }

        // Tokens must be different
        if let (Some(ref read), Some(ref write)) = (&self.read_token, &self.write_token) {
            if read == write {
                return Err("auth.read_token and auth.write_token must be different".to_string());
            }
        }

        // Token minimum length (security: short tokens are brute-forceable)
        if let Some(ref token) = self.read_token {
            if token.len() < 16 {
                return Err(format!(
                    "auth.read_token is too short ({} chars, minimum 16)",
                    token.len()
                ));
            }
        }
        if let Some(ref token) = self.write_token {
            if token.len() < 16 {
                return Err(format!(
                    "auth.write_token is too short ({} chars, minimum 16)",
                    token.len()
                ));
            }
        }

        // Rate limit must be reasonable
        if self.rate_limit_per_second == 0 {
            // 0 means disabled — warn but allow
        } else if self.rate_limit_per_second > 1000 {
            return Err(format!(
                "auth.rate_limit_per_second is too high ({}, maximum 1000)",
                self.rate_limit_per_second
            ));
        }

        Ok(())
    }
}
```

### Constant-Time Token Comparison

Token comparison MUST be constant-time to prevent timing attacks. An attacker who
can measure response time differences can determine the token character-by-character.

```rust
// crates/common/src/crypto.rs
/// Constant-time comparison of two byte slices.
/// Returns true if they are equal, false otherwise.
/// Takes the same amount of time regardless of where the first difference occurs.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Still do a comparison to avoid leaking length via timing.
        // Compare a against itself to burn the same CPU cycles.
        let mut result = 0u8;
        for byte in a.iter().chain(b.iter()) {
            result |= byte ^ byte;
        }
        false
    } else {
        let mut result = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            result |= x ^ y;
        }
        result == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_not_equal() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"hello", b"helloworld"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }
}
```

---

## 2. Token Generation

The `wasm-node` binary can generate cryptographically secure tokens at setup time.

```rust
// crates/common/src/auth.rs — token generation

impl AuthConfig {
    /// Generate a new random token suitable for use as a bearer token.
    /// Returns a 32-byte hex string (64 characters).
    pub fn generate_token() -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    /// Generate a default auth config with random tokens.
    /// Used by `wasm-node --generate-config`.
    pub fn generate_default() -> Self {
        AuthConfig {
            enabled: true,
            read_token: Some(Self::generate_token()),
            write_token: Some(Self::generate_token()),
            require_tls: true,
            rate_limit_per_second: 10,
            rate_limit_burst: 20,
        }
    }
}
```

### CLI Token Generation

```bash
# Generate tokens and print to stdout (for initial setup)
wasm-node --generate-tokens
# Output:
# read_token:  a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456
# write_token: f6e5d4c3b2a1098765432109876543210987fedcba0987654321fedcba098765
#
# Add these to your config.toml under [auth] section.

# Generate a complete config file with random tokens
wasm-node --generate-config > /etc/wasm-node/config.toml
```

---

## 3. TOML Configuration

```toml
# /etc/wasm-node/config.toml — auth section

[auth]
# Enable authentication on admin API.
enabled = true

# Read-only token (for Prometheus, monitoring dashboards).
read_token = "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456"

# Read-write token (for operators, CI/CD).
write_token = "f6e5d4c3b2a1098765432109876543210987fedcba0987654321fedcba098765"

# Require TLS when auth is enabled.
# Set to false ONLY in development.
require_tls = true

# Rate limit for admin API (per IP).
rate_limit_per_second = 10
rate_limit_burst = 20
```

### Environment Variable Override

```bash
# Tokens can be provided via environment variables (for container deployments)
export WASM_NODE_AUTH_ENABLED=true
export WASM_NODE_AUTH_READ_TOKEN="a1b2c3d4..."
export WASM_NODE_AUTH_WRITE_TOKEN="f6e5d4c3..."
```

---

## 4. Authentication Middleware

An Axum middleware layer checks authentication before any admin handler runs.

```rust
// crates/proxy/src/auth_middleware.rs
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use common::auth::{AuthConfig, AuthResult, Permission, TokenType};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    /// Current auth configuration (hot-reloadable for token rotation).
    pub config: Arc<RwLock<AuthConfig>>,

    /// Metrics for authentication events.
    pub metrics: Arc<AuthMetrics>,

    /// Admin API rate limiter (per IP).
    pub rate_limiter: Arc<AdminRateLimiter>,
}

/// Axum middleware that checks authentication on admin API requests.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, &'static str)> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // 1. Check if this path requires authentication
    if is_public_endpoint(&path) {
        return Ok(next.run(request).await);
    }

    // 2. Rate limit check (before auth to prevent brute-force)
    let client_ip = extract_client_ip(&headers);
    if !state.rate_limiter.allow(client_ip) {
        state.metrics.rate_limited_total.inc();
        return Err((StatusCode::TOO_MANY_REQUESTS, "admin API rate limit exceeded"));
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
                // Log the failure
                tracing::warn!(
                    path = %path,
                    method = %method,
                    ip = ?client_ip,
                    "admin API authentication failed — invalid or missing token"
                );
                return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
            }
            Permission::Read => {
                tracing::warn!(
                    path = %path,
                    method = %method,
                    ip = ?client_ip,
                    "admin API authorization failed — read token used for write operation"
                );
                return Err((StatusCode::FORBIDDEN, "insufficient permissions"));
            }
            Permission::Write => {
                // Should not reach here (Write >= any required level)
                return Err((StatusCode::FORBIDDEN, "insufficient permissions"));
            }
        }
    }

    // 5. Log successful access
    tracing::debug!(
        path = %path,
        method = %method,
        token_type = ?auth_result.token_type,
        "admin API request authenticated"
    );

    state.metrics.auth_successes_total.inc();

    // 6. Proceed to the handler
    Ok(next.run(request).await)
}

/// Determine if a path is public (no authentication required).
fn is_public_endpoint(path: &str) -> bool {
    matches!(path,
        "/health" | "/healthz" | "/readyz" | "/livez" |
        "/status/metrics" |  // Prometheus scrape endpoint
        "/favicon.ico"
    )
}

/// Determine the minimum permission level required for a request.
fn required_permission(method: &http::Method, path: &str) -> Permission {
    // All GET requests require at least Read
    if method == http::Method::GET {
        return Permission::Read;
    }

    // All mutation methods (POST, PATCH, DELETE, PUT) require Write
    Permission::Write
}

/// Extract the client IP from headers (X-Forwarded-For or X-Real-IP)
/// or return a default.
fn extract_client_ip(headers: &HeaderMap) -> Option<std::net::IpAddr> {
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
```

---

## 5. Admin API Rate Limiter

A dedicated rate limiter for the admin API, separate from the per-app rate limiter
(Step 24). This limiter is per-IP and much stricter.

```rust
// crates/proxy/src/auth_middleware.rs — admin rate limiter

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Per-IP rate limiter for the admin API.
/// Uses a simple token bucket algorithm.
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
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        AdminRateLimiter {
            buckets: Mutex::new(HashMap::new()),
            refill_rate: rate_per_second as f64,
            max_tokens: burst as f64,
        }
    }

    /// Check if a request from the given IP is allowed.
    /// Returns true if allowed, false if rate-limited.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let ip = match ip {
            Some(ip) => ip,
            None => {
                // No IP information — allow (conservative: don't block unknown sources)
                // But log a warning
                tracing::debug!("admin API request with no client IP — skipping rate limit");
                return true;
            }
        };

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

    /// Prune stale IP buckets (called periodically).
    pub fn prune_stale(&self, max_age: Duration) {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_age);
    }
}
```

---

## 6. Authentication Metrics

```rust
// crates/proxy/src/auth_middleware.rs — metrics

use prometheus::{IntCounter, Opts, Registry};

pub struct AuthMetrics {
    /// Total successful authentications.
    pub auth_successes_total: IntCounter,

    /// Total failed authentications (bad token, missing token).
    pub auth_failures_total: IntCounter,

    /// Total requests rate-limited on the admin API.
    pub rate_limited_total: IntCounter,
}

impl AuthMetrics {
    pub fn new(registry: &Registry) -> Self {
        let auth_successes_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_successes_total",
            "Successful admin API authentications",
        )).unwrap();
        registry.register(Box::new(auth_successes_total.clone())).unwrap();

        let auth_failures_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_auth_failures_total",
            "Failed admin API authentications",
        )).unwrap();
        registry.register(Box::new(auth_failures_total.clone())).unwrap();

        let rate_limited_total = IntCounter::with_opts(Opts::new(
            "wasm_admin_rate_limited_total",
            "Admin API requests rejected by rate limiter",
        )).unwrap();
        registry.register(Box::new(rate_limited_total.clone())).unwrap();

        AuthMetrics {
            auth_successes_total,
            auth_failures_total,
            rate_limited_total,
        }
    }
}
```

### Prometheus Alerting Rules

```yaml
groups:
  - name: admin_auth
    rules:
      - alert: AdminAuthBruteForce
        expr: rate(wasm_admin_auth_failures_total[5m]) > 5
        for: 2m
        annotations:
          summary: "High rate of admin API auth failures on {{ $labels.node }}"
          description: "Possible brute-force attack on the admin API. Consider blocking the source IP."

      - alert: AdminAuthNoToken
        expr: rate(wasm_admin_auth_failures_total[1h]) > 0 and rate(wasm_admin_auth_successes_total[1h]) == 0
        for: 5m
        annotations:
          summary: "Admin API auth failures with no successes on {{ $labels.node }}"
          description: "All admin API requests are failing auth. Check token configuration."
```

---

## 7. Audit Logging for Admin Actions

Every admin API call that passes authentication is logged to the audit system.

```rust
// crates/proxy/src/auth_middleware.rs — audit logging

use common::auth::TokenType;
use supervisor::audit::{AuditEvent, AuditEventType};

/// Log an admin API action to the audit system.
pub fn log_admin_action(
    path: &str,
    method: &str,
    token_type: TokenType,
    client_ip: Option<std::net::IpAddr>,
    status_code: u16,
    node_id: &str,
) {
    let event = AuditEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        node_id: node_id.to_string(),
        event_type: AuditEventType::AdminApiCall,
        actor: format!("admin:{}", match token_type {
            TokenType::ReadToken => "read_token",
            TokenType::WriteToken => "write_token",
        }),
        app_id: "_platform".to_string(),
        details: serde_json::json!({
            "path": path,
            "method": method,
            "client_ip": client_ip.map(|ip| ip.to_string()).unwrap_or("unknown".to_string()),
            "status_code": status_code,
        }),
    };

    supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &event);
}
```

### AuditEventType Extension

```rust
// crates/supervisor/src/audit.rs — addition

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    AppDeployed,
    AppRemoved,
    InstanceSpawned,
    InstanceKilled,
    SecretRotated,
    TrapOccurred,
    BinaryHashMismatch,
    RateLimitExceeded,
    PolicyViolation,
    AdminApiCall,       // NEW: All admin API mutations
    AuthFailure,        // NEW: Failed authentication attempts
    TokenRotated,       // NEW: Token rotation events
}
```

---

## 8. Token Rotation Without Restart

Tokens can be rotated at runtime via the admin API itself (authenticated with the
current write token) or by updating the config file and signaling the node.

### Rotation via Admin API

```rust
// crates/proxy/src/admin.rs — token rotation endpoint

use axum::{
    extract::State,
    Json,
    http::StatusCode,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct RotateTokenRequest {
    /// Which token to rotate: "read" or "write".
    pub token_type: String,

    /// The new token value. If not provided, a random token is generated.
    pub new_token: Option<String>,
}

/// POST /admin/auth/rotate-token — Rotate an authentication token.
///
/// Requires: Write permission (only the write token can rotate tokens).
///
/// After rotation, the old token is immediately invalidated.
/// The response includes the new token — this is the only time it is shown.
pub async fn rotate_token(
    State(state): State<AdminState>,
    Json(req): Json<RotateTokenRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let new_token = req.new_token
        .unwrap_or_else(|| common::auth::AuthConfig::generate_token());

    // Validate new token length
    if new_token.len() < 16 {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "new token must be at least 16 characters"
        }))));
    }

    let mut config = state.auth_config.write().await;

    match req.token_type.as_str() {
        "read" => {
            let old = config.read_token.clone();
            config.read_token = Some(new_token.clone());
            tracing::warn!(
                old_prefix = old.map(|t| &t[..8]).unwrap_or("none"),
                new_prefix = &new_token[..8],
                "read token rotated"
            );
        }
        "write" => {
            let old = config.write_token.clone();
            config.write_token = Some(new_token.clone());
            tracing::warn!(
                old_prefix = old.map(|t| &t[..8]).unwrap_or("none"),
                new_prefix = &new_token[..8],
                "write token rotated"
            );
        }
        _ => {
            return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": "token_type must be 'read' or 'write'"
            }))));
        }
    }

    // Persist the updated config to redb
    if let Some(ref store) = state.store {
        if let Err(e) = store.save_auth_config(&config) {
            tracing::error!(error = %e, "failed to persist rotated token");
        }
    }

    // Audit log
    supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &AuditEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        node_id: state.node_id.clone(),
        event_type: AuditEventType::TokenRotated,
        actor: "admin:write_token".to_string(),
        app_id: "_platform".to_string(),
        details: serde_json::json!({
            "token_type": req.token_type,
        }),
    });

    Ok((StatusCode::OK, Json(serde_json::json!({
        "status": "rotated",
        "token_type": req.token_type,
        "new_token": new_token,
        "warning": "Save this token securely. It will not be shown again.",
    }))))
}
```

### Rotation via Config File + Signal

```bash
# 1. Edit the config file with new tokens
vim /etc/wasm-node/config.toml

# 2. Send SIGHUP to the node to reload auth config
kill -HUP $(pidof wasm-node)

# The node reads the updated config file and applies the new tokens.
# Old tokens are immediately invalidated.
```

```rust
// crates/node/src/main.rs — SIGHUP handler for config reload

fn setup_sighup_handler(auth_config: Arc<RwLock<AuthConfig>>, config_path: Option<PathBuf>) {
    use tokio::signal::unix::{signal, SignalKind};

    tokio::spawn(async move {
        let mut stream = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");
        loop {
            stream.recv().await;

            if let Some(ref path) = config_path {
                tracing::info!("SIGHUP received — reloading auth config from file");

                match std::fs::read_to_string(path) {
                    Ok(content) => {
                        match toml::from_str::<common::config::NodeConfig>(&content) {
                            Ok(new_config) => {
                                let mut auth = auth_config.write().await;
                                *auth = new_config.auth;
                                tracing::info!("auth config reloaded from file");
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to parse config file on reload");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, path = %path.display(), "failed to read config file on reload");
                    }
                }
            } else {
                tracing::warn!("SIGHUP received but no config file path — cannot reload");
            }
        }
    });
}
```

---

## 9. Auth Config Persistence in redb

Token rotations are persisted so they survive node restart.

```rust
// crates/storage/src/auth_config.rs
use crate::{tables::SCHEMA_META, Store};
use common::auth::AuthConfig;
use common::error::PlatformError;
use redb::ReadableTable;

const AUTH_CONFIG_KEY: &str = "auth_config_override";

impl Store {
    pub fn save_auth_config(&self, config: &AuthConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(AUTH_CONFIG_KEY, json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn load_auth_config(&self) -> Result<Option<AuthConfig>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(SCHEMA_META)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(AUTH_CONFIG_KEY)
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            Some(v) => {
                let config: AuthConfig = serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }
}
```

---

## 10. Admin API Router with Authentication

```rust
// crates/proxy/src/admin.rs — updated router

use axum::{
    Router,
    routing::{get, post, patch, delete},
    middleware,
};

pub fn admin_router(state: AdminState) -> Router {
    let auth_state = AuthState {
        config: state.auth_config.clone(),
        metrics: state.auth_metrics.clone(),
        rate_limiter: state.admin_rate_limiter.clone(),
    };

    Router::new()
        // ── Public endpoints (no auth required) ────────────────────
        .route("/health", get(health_handler))
        .route("/healthz", get(health_handler))
        .route("/readyz", get(readiness_handler))
        .route("/livez", get(liveness_handler))
        .route("/status/metrics", get(metrics_handler))

        // ── Authenticated endpoints ────────────────────────────────
        .route("/status/pgbouncer", get(pgbouncer_status))
        .route("/admin/config", get(get_config))
        .route("/admin/config", patch(update_config))
        .route("/admin/config", delete(reset_config))
        .route("/admin/rebuild", post(rebuild_handler))
        .route("/admin/gc/force", post(force_gc_handler))
        .route("/admin/auth/rotate-token", post(rotate_token))
        .route("/logs/{app_id}", get(log_stream_handler))

        // Apply auth middleware to all routes
        .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
        .with_state(state)
}
```

---

## 11. CLI Authentication

`wasm-ctl` must send the bearer token with every admin API request.

### Token Configuration for CLI

```bash
# Option 1: Command-line flag
wasm-ctl --auth-token "f6e5d4c3..." node health

# Option 2: Environment variable
export WASM_CTL_AUTH_TOKEN="f6e5d4c3..."
wasm-ctl node health

# Option 3: Config file (~/.wasm-ctl/config.toml)
# [auth]
# token = "f6e5d4c3..."
# node_api = "https://node-0:9090"
```

### CLI Implementation

```rust
// crates/ctl/src/main.rs — updated HTTP client

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

fn build_http_client(token: Option<&str>) -> reqwest::Client {
    let mut headers = HeaderMap::new();

    if let Some(t) = token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", t))
                .expect("invalid token format"),
        );
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP client")
}

/// Resolve the auth token from: CLI flag > env var > config file.
fn resolve_auth_token(cli_token: Option<&str>) -> Option<String> {
    // 1. CLI flag (highest priority)
    if let Some(t) = cli_token {
        return Some(t.to_string());
    }

    // 2. Environment variable
    if let Ok(t) = std::env::var("WASM_CTL_AUTH_TOKEN") {
        return Some(t);
    }

    // 3. Config file (~/.wasm-ctl/config.toml)
    let config_path = dirs::home_dir()
        .map(|p| p.join(".wasm-ctl").join("config.toml"))?;

    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(config) = toml::from_str::<CtlConfig>(&content) {
                return config.auth.token;
            }
        }
    }

    None
}
```

### CLI Error Handling for Auth Failures

```rust
// crates/ctl/src/main.rs — auth error handling

/// Handle HTTP responses that may indicate auth failures.
fn handle_response(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    match response.status() {
        s if s == StatusCode::UNAUTHORIZED => {
            anyhow::bail!(
                "Authentication failed. Set --auth-token or WASM_CTL_AUTH_TOKEN environment variable."
            );
        }
        s if s == StatusCode::FORBIDDEN => {
            anyhow::bail!(
                "Permission denied. Your token has read-only access but this operation requires write access.\n\
                 Use the write token (auth.write_token in config.toml)."
            );
        }
        s if s == StatusCode::TOO_MANY_REQUESTS => {
            anyhow::bail!(
                "Admin API rate limit exceeded. Wait a moment and try again."
            );
        }
        s if s.is_server_error() => {
            anyhow::bail!("Server error: {}", s);
        }
        _ => Ok(response),
    }
}
```

---

## 12. TLS Enforcement for Admin API

When authentication is enabled and `require_tls` is true, the admin API must use HTTPS.

### Startup Check

```rust
// crates/node/src/main.rs — TLS enforcement check

fn check_admin_tls_requirement(auth_config: &AuthConfig, admin_port: u16, tls_config: Option<&()>) -> Result<(), String> {
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

    // Check if the admin API has TLS configured
    // The admin API uses axum, not Pingora, so it needs its own TLS.
    // For now, we check if a TLS cert is configured.
    if tls_config.is_none() {
        return Err(format!(
            "Admin API requires TLS when authentication is enabled, \
             but no TLS certificate is configured. \
             Either:\n\
             1. Configure TLS for the admin API (--admin-tls-cert / --admin-tls-key)\n\
             2. Set auth.require_tls = false (NOT recommended for production)\n\
             3. Disable authentication (auth.enabled = false, NOT recommended)"
        ));
    }

    Ok(())
}
```

### Admin API TLS Configuration

The admin API (Axum) needs its own TLS configuration, separate from Pingora's TLS.

```rust
// crates/proxy/src/admin_tls.rs
use axum_server::tls_rustls::RustlsConfig;
use std::path::Path;

/// Build a Rustls configuration for the admin API.
pub fn admin_tls_config(
    cert_path: &Path,
    key_path: &Path,
) -> RustlsConfig {
    RustlsConfig::from_pem_file(cert_path, key_path)
        .expect("failed to load admin API TLS certificate")
}
```

---

## 13. Endpoint Permission Matrix

```
Endpoint                         │ Method │ Permission │ Auth Required
─────────────────────────────────┼────────┼────────────┼──────────────
/health, /healthz, /livez       │ GET    │ None       │ No
/readyz                          │ GET    │ None       │ No
/status/metrics                  │ GET    │ None       │ No (Prometheus)
/status/pgbouncer                │ GET    │ Read       │ Yes
/admin/config                    │ GET    │ Read       │ Yes
/admin/config                    │ PATCH  │ Write      │ Yes
/admin/config                    │ DELETE │ Write      │ Yes
/admin/rebuild                   │ POST   │ Write      │ Yes
/admin/gc/force                  │ POST   │ Write      │ Yes
/admin/auth/rotate-token         │ POST   │ Write      │ Yes
/logs/{app_id}                   │ GET    │ Read       │ Yes
```

---

## 14. Testing Strategy

### Unit Tests

```bash
cargo test -p common --lib  # Auth config, token comparison, validation
cargo test -p proxy --lib   # Auth middleware, rate limiter
```

Tests to implement:
- `test_auth_disabled_grants_write`: When auth is disabled, all requests get Write
- `test_auth_valid_write_token`: Write token grants Write permission
- `test_auth_valid_read_token`: Read token grants Read permission
- `test_auth_invalid_token`: Invalid token gets None permission
- `test_auth_missing_header`: Missing Authorization header gets None
- `test_auth_read_token_cannot_write`: Read token on POST returns 403
- `test_auth_write_token_can_read`: Write token on GET returns 200
- `test_constant_time_eq`: Timing-safe comparison works correctly
- `test_auth_config_validate_no_tokens`: Enabled auth with no tokens is rejected
- `test_auth_config_validate_short_token`: Token < 16 chars is rejected
- `test_auth_config_validate_same_tokens`: Same read and write token is rejected
- `test_rate_limiter_allows_burst`: Burst within limit is allowed
- `test_rate_limiter_blocks_excess`: Requests beyond burst are blocked
- `test_rate_limiter_prune`: Stale buckets are cleaned up
- `test_public_endpoints_no_auth`: Health/metrics endpoints skip auth
- `test_token_rotation_updates_config`: Rotation changes the active token
- `test_token_rotation_invalidates_old`: Old token fails after rotation

### Integration Tests

```bash
cargo test -p proxy --tests  # Admin API with real HTTP
```

Tests to implement:
- `test_admin_api_auth_flow`: Full request flow with token in header
- `test_admin_api_auth_failure`: Request without token returns 401
- `test_admin_api_forbidden`: Read token on write endpoint returns 403
- `test_admin_api_rate_limit`: Exceeding rate limit returns 429
- `test_admin_api_token_rotation`: Rotate token, verify old fails and new works

### E2E Tests

```bash
cargo test -p e2e -- --ignored --test-threads=1
```

Tests to implement:
- `test_e2e_admin_auth_required`: Deploy a node with auth enabled, verify
  unauthenticated requests are rejected
- `test_e2e_admin_token_rotation`: Rotate a token via admin API, verify
  the old token is rejected and the new token works
- `test_e2e_cli_auth`: Use `wasm-ctl` with `--auth-token` to manage a node

---

## 15. Security Considerations

### Token Storage

Tokens are stored in:
1. **Config file** (`/etc/wasm-node/config.toml`) — file permissions must be 0600
2. **redb** (after rotation) — encrypted at rest if KEK is configured
3. **Environment variables** — visible in `/proc/<pid>/environ` on Linux
4. **CLI config** (`~/.wasm-ctl/config.toml`) — file permissions must be 0600

The node should warn at startup if the config file has overly permissive permissions.

```rust
fn check_config_file_permissions(path: &Path) {
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
}
```

### Token in Process Memory

Bearer tokens exist in the node's process memory. A memory dump or `/proc/<pid>/mem`
access reveals them. This is inherent to bearer token authentication and cannot be
fully mitigated. Mitigations:

- Tokens are stored as `String` in `AuthConfig`. They are not zeroed on drop.
- For higher security, use a hardware security module (HSM) or external secret store
  (Vault) — but this violates the shared-nothing principle.
- The current design is appropriate for the platform's threat model: the admin API
  is on an internal network, not exposed to the internet.

### Brute-Force Protection

The rate limiter allows 10 requests/second per IP. At this rate:
- A 64-character hex token has 16^64 ≈ 1.16 × 10^77 possible values
- Even with a targeted attack knowing the format, brute-force is infeasible
- The rate limiter also prevents DoS on the admin API itself

### Token Leakage via Logs

The audit log records admin API calls but does NOT log the token value. Only the
token type (`read_token` or `write_token`) is logged. The first 8 characters of
the old token may be logged during rotation for identification purposes.

---

## 16. Migration Path

### Phase 1: Add Auth Middleware (Non-Breaking)

Add the `AuthConfig`, `AuthState`, and `auth_middleware` to the codebase. By default,
`auth.enabled = false`, so all existing deployments continue to work without changes.

### Phase 2: Generate Tokens

Add `--generate-tokens` flag and `--generate-config` support. Operators generate tokens
and add them to their config files.

### Phase 3: Enable Auth

Set `auth.enabled = true` in production config files. All admin API requests now require
a token. Prometheus and health checks continue to work without tokens (public endpoints).

### Phase 4: Enable TLS

Add TLS configuration for the admin API. Set `auth.require_tls = true`. The node refuses
to start if auth is enabled without TLS.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Token Configuration
- [ ] `AuthConfig` struct with enabled, read_token, write_token, require_tls, rate limits
- [ ] `AuthConfig::authenticate()` validates bearer tokens with constant-time comparison
- [ ] `AuthConfig::validate()` rejects invalid configurations
- [ ] `AuthConfig::generate_token()` creates cryptographically secure 32-byte hex tokens
- [ ] `AuthConfig::generate_default()` creates a complete config with random tokens
- [ ] `constant_time_eq()` prevents timing attacks on token comparison
- [ ] Token minimum length enforced (16 characters)

### Authentication Middleware
- [ ] `auth_middleware` Axum layer checks auth before handler execution
- [ ] Public endpoints (`/health`, `/metrics`) bypass authentication
- [ ] `required_permission()` maps method+path to Read or Write
- [ ] Missing token returns 401 Unauthorized
- [ ] Read token on write endpoint returns 403 Forbidden
- [ ] Invalid token returns 401 Unauthorized
- [ ] Successful auth logged at debug level
- [ ] Failed auth logged at warn level with client IP

### Rate Limiting
- [ ] `AdminRateLimiter` with per-IP token buckets
- [ ] Default: 10 requests/second per IP, burst of 20
- [ ] Rate-limited requests return 429 Too Many Requests
- [ ] Stale IP buckets pruned periodically
- [ ] Rate limit configurable via `auth.rate_limit_per_second`

### Metrics
- [ ] `wasm_admin_auth_successes_total` counter
- [ ] `wasm_admin_auth_failures_total` counter
- [ ] `wasm_admin_rate_limited_total` counter
- [ ] Prometheus alerting rules for brute-force detection

### Audit Logging
- [ ] `AdminApiCall` added to `AuditEventType`
- [ ] `AuthFailure` added to `AuditEventType`
- [ ] `TokenRotated` added to `AuditEventType`
- [ ] Every admin API call logged with path, method, token type, client IP
- [ ] Failed auth attempts logged with client IP
- [ ] Token values NEVER logged (only token type)

### Token Rotation
- [ ] `POST /admin/auth/rotate-token` endpoint (requires Write permission)
- [ ] Rotation immediately invalidates old token
- [ ] New token returned in response (shown only once)
- [ ] Rotation persisted to redb (survives restart)
- [ ] SIGHUP handler reloads auth config from file
- [ ] Rotation logged to audit system

### TLS Enforcement
- [ ] `auth.require_tls` flag checked at startup
- [ ] Node refuses to start if auth + TLS required but no TLS configured
- [ ] Warning logged if auth enabled without TLS requirement
- [ ] Admin API TLS configuration (separate from Pingora TLS)
- [ ] Config file permissions checked (warn if world-readable)

### CLI Integration
- [ ] `--auth-token` CLI flag for `wasm-ctl`
- [ ] `WASM_CTL_AUTH_TOKEN` environment variable support
- [ ] `~/.wasm-ctl/config.toml` config file support
- [ ] Token resolution priority: CLI > env > config file
- [ ] Clear error messages for 401, 403, 429 responses
- [ ] `wasm-ctl auth rotate --token-type read` command

### Persistence
- [ ] Auth config saved to redb `SCHEMA_META` table
- [ ] Auth config loaded on startup (overrides TOML file values)
- [ ] Corrupted auth config falls back to TOML file with warning

### Testing
- [ ] Unit tests for auth config validation (6+ tests)
- [ ] Unit tests for token comparison and extraction (4+ tests)
- [ ] Unit tests for rate limiter (3+ tests)
- [ ] Unit tests for middleware permission checks (5+ tests)
- [ ] Integration tests with real HTTP (5+ tests)
- [ ] E2E test: admin API requires auth token
- [ ] E2E test: token rotation works end-to-end
- [ ] E2E test: CLI auth with `--auth-token`

### Documentation
- [ ] `auth` section added to config.toml schema
- [ ] Token generation instructions documented
- [ ] Endpoint permission matrix documented
- [ ] TLS setup guide for admin API
- [ ] Token rotation runbook
- [ ] Security considerations documented (token storage, brute-force, memory)
- [ ] Config file permission warning documented
