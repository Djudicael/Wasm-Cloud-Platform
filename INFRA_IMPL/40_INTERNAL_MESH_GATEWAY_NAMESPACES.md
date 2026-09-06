# Step 40 — Internal Mesh Gateway, Namespaces & Endpoint Security

> **Implementation status (2026-08-30): superseded security model.** This step
> records the original service-discovery-only design and its historical gap.
> Step 41 now supplies eBPF source-port/TID attribution; the gateway strips
> caller identity headers, fails closed when identity is unresolved, and denies
> resolved cross-namespace calls unless explicitly allowed. Do not treat later
> statements that port 9080 is "open to all namespaces" as current behavior.
> See `docs/internal-mesh.md` and
> `INFRA_IMPL/process/INTERNAL_MESH_OIDC_ROLE_VALIDATION.md` for the validated
> current contract.
>
> The current production contract is deliberately node-local. Manifests use
> `placement.policy = "every_node"` and declare fully qualified same-namespace
> `local_dependencies`. Missing dependencies fail locally with 502 and never
> trigger a remote-node lookup. Cross-host mesh identity is explicitly out of
> scope by design, not an unimplemented part of this step.

## Goal

Fix three gaps left by Steps 04, 33, and 39 **without requiring the Wasm application to know anything about namespaces, gateways, or platform topology.**

1. **Namespace segregation** — every app belongs to a namespace. Apps in the same namespace
can reach each other transparently. Apps in different namespaces are isolated by service
discovery (the Supervisor only injects service URLs for same-namespace apps). Direct
cross-namespace connections to known app ports are blocked at the WASI layer, but the
internal gateway port (9080) is currently open to all namespaces. Cross-namespace traffic
that must be allowed should use the external hostname (which flows through Pingora with
full TLS/authz/audit).

2. **Transparent internal proxy for East-West traffic** — today, East-West calls bypass Pingora
(Step 04) and lose rate limiting, circuit breaking, and JWT validation. A transparent internal
gateway restores these features without the app ever knowing a proxy exists. The app uses
normal HTTP URLs; the platform intercepts and routes them.

3. **Per-endpoint gateway security & a unified application deployment manifest** — Step 39
defines auth at the *route* level (the entire app). Production APIs need finer control:
`GET /health` is public, `POST /api/admin` requires role `admin`, `GET /api/billing`
requires an API key. This step adds path-level security rules and folds everything into a
single deploy manifest.

---

## Context & Rationale

### Why Direct East-West Is No Longer Enough

Step 04 routes App A → App B directly through the `LocalServiceRegistry`. The Supervisor
intercepts the WASI `connect()` and short-circuits to the local port. This is fast, but it
skips every middleware in Pingora:

| Feature | North-South (via Pingora) | East-West (direct) |
|---------|---------------------------|-------------------|
| Rate limiting | ✅ Token bucket (Step 24) | ❌ None |
| JWT auth / roles | ✅ OIDC (Step 39) | ❌ None |
| Circuit breaker | ✅ Per-app (Step 39) | ❌ None |
| CORS | ✅ Route config (Step 39) | ❌ None |
| Request transform | ✅ Header injection (Step 39) | ❌ None |
| Audit logging | ✅ Access log | ❌ Only Supervisor trace |

In a multi-tenant platform, a misbehaving internal service can DoS its neighbors just as
easily as an external client can. East-West traffic must be subject to the same policies.

### The Transparency Principle

The execution platform **must not** leak its topology into the Wasm application.

**What the app should write:**
```rust
// Inside the Wasm app — completely normal code
let client = reqwest::Client::new();
let resp = client
    .get("http://api-b.internal/users")
    .send()
    .await?;
```

**What the platform does:**
1. The WASI virtual DNS resolves `api-b.internal` to a loopback address.
2. The Supervisor's `socket_addr_check` intercepts the outbound TCP connection.
3. It looks up `api-b` in the caller's namespace, checks rate limits and circuit breaker.
4. If the target has simple endpoint policies, the connection is rewritten to the target's
   real port directly (fast path).
5. If the target has complex endpoint policies (per-path auth), the connection is
   transparently routed through an internal HTTP proxy that applies them.
6. The app receives the HTTP response exactly as if it had talked to `api-b` directly.

The app never sets `X-Target-App`, `X-Source-App`, or any platform-specific header.

### Why Namespaces?

Namespaces provide three things without affecting application code:

1. **Blast-radius isolation** — a runaway app in `namespace: staging` cannot directly reach
   apps in `namespace: production`.
2. **Multi-tenancy boundaries** — `tenant-a` and `tenant-b` can both deploy an app named
   `api-users` without collision.
3. **Policy enforcement boundaries** — cross-namespace traffic is isolated by service
   discovery (apps only learn about same-namespace services). Direct cross-namespace
   connections to known app ports are blocked at the WASI layer, but the internal gateway
   port is currently open to all namespaces.

### The Cross-Namespace Rule

```
Same namespace    →  Transparent internal routing (WASI layer)
                     App uses: http://api-b.internal/...
                     Platform enforces: rate limit, circuit breaker, endpoint auth

Different namespace →  Service discovery isolation (no <APP>_SERVICE_URL injected)
                     Direct port connections blocked at WASI layer (ECONNREFUSED)
                     Gateway port (9080) is currently open to all namespaces
                     App must use: https://api-b.other-ns.example.com/...
                     Routed through: External Gateway (Pingora, 443/80)
                     Policies: TLS + OIDC + audit + distributed rate limit
```

This rule is enforced inside the Supervisor's network interceptor — the Wasm module cannot
bypass it because it does not control the WASI host functions.

---

---

## 1. Namespace Data Model

### AppConfig Extension

```rust
// crates/common/src/types.rs — addition to AppConfig

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    // ... existing fields ...

    /// The namespace this app belongs to.
    /// Namespaces are flat strings (e.g., "production", "tenant-a").
    /// Default = "default". The Wasm app never sees this value.
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

fn default_namespace() -> String { "default".to_string() }
```

### Qualified App ID

Inside the platform, an app is uniquely identified by the tuple `(namespace, app_name, version)`.
For storage and routing we serialize this as a qualified string:

```
<namespace>/<app_name>:<version>
```

Examples:
- `default/api-users:v2`
- `tenant-a/api-users:v2`
- `production/payments:v1`

The `AppId` struct gains helpers for the platform to use internally:

```rust
// crates/common/src/types.rs

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppId(pub String);

impl AppId {
    pub fn new(namespace: &str, name: &str, version: &str) -> Self {
        AppId(format!("{}/{name}:{version}", namespace))
    }

    pub fn namespace(&self) -> &str {
        self.0.split('/').next().unwrap_or("default")
    }

    pub fn bare_name(&self) -> &str {
        self.0.split('/').nth(1).unwrap_or(&self.0)
    }
}
```

**Important:** The Wasm application does not receive the namespace in any environment variable.
The namespace is purely a platform construct.

### NamespaceRegistry (Replaces LocalServiceRegistry)

The `LocalServiceRegistry` from Step 04 is upgraded to be namespace-scoped, and it tracks
the mapping from **allocated source ports** to **app identity**. This lets the platform
know which app is making an outbound connection without trusting any HTTP header.

```rust
// crates/supervisor/src/namespace_registry.rs

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use common::types::AppId;

/// Registry of running instances, scoped by namespace.
#[derive(Clone, Default)]
pub struct NamespaceRegistry {
    /// namespace → app_id → list of (instance_id, socket_addr)
    instances: Arc<RwLock<HashMap<String, HashMap<String, Vec<SocketAddr>>>>>,

    /// Reverse lookup: source_port (TCP ephemeral port allocated to an instance) → AppId.
    /// Populated by the Supervisor when an instance is spawned.
    port_to_app: Arc<RwLock<HashMap<u16, AppId>>>,
}

impl NamespaceRegistry {
    pub async fn register(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.instances.write().await;
        map.entry(app_id.namespace().to_string())
            .or_default()
            .entry(app_id.bare_name().to_string())
            .or_default()
            .push(addr);
    }

    pub async fn deregister(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.instances.write().await;
        if let Some(ns) = map.get_mut(app_id.namespace()) {
            if let Some(addrs) = ns.get_mut(app_id.bare_name()) {
                addrs.retain(|a| a != addr);
            }
        }
    }

    /// Resolve a bare app name inside a given namespace to its local port.
    pub async fn resolve(&self, namespace: &str, bare_app_name: &str) -> Option<SocketAddr> {
        let map = self.instances.read().await;
        map.get(namespace)
            .and_then(|ns| ns.get(bare_app_name))
            .and_then(|addrs| addrs.first().copied())
    }

    /// Register which app owns a given source port (for outbound call attribution).
    pub async fn bind_source_port(&self, port: u16, app_id: AppId) {
        self.port_to_app.write().await.insert(port, app_id);
    }

    /// Look up the app that owns a source port.
    pub async fn resolve_source_app(&self, port: u16) -> Option<AppId> {
        self.port_to_app.read().await.get(&port).cloned()
    }

    /// Unregister a source port when an instance stops.
    pub async fn release_source_port(&self, port: u16) {
        self.port_to_app.write().await.remove(&port);
    }
}
```

---

## 2. Transparent Internal Routing

### How It Works

```
App A (payments:v1) in namespace "production"
  │
  │  Inside Wasm: TcpStream::connect("api-b.internal:8080")
  │  ───────────────────────────────────────────────────────►
  │                                                          │
  │                                                    ┌─────▼──────────────┐
  │                                                    │  WASI Host Layer   │
  │                                                    │  (Supervisor)      │
  │                                                    │                    │
  │                                                    │  1. Virtual DNS    │
  │                                                    │     resolves       │
  │                                                    │     "api-b.internal"
  │                                                    │     → 127.0.0.1:1  │
  │                                                    │     (placeholder)  │
  │                                                    │                    │
  │                                                    │  2. socket_addr_check()
  │                                                    │     sees dest port 1
  │                                                    │                    │
  │                                                    │  3. Looks up caller│
  │                                                    │     via source port│
  │                                                    │     → App A        │
  │                                                    │                    │
  │                                                    │  4. Resolves target│
  │                                                    │     in same ns:    │
  │                                                    │     "api-b" →      │
  │                                                    │     127.0.0.1:10101│
  │                                                    │                    │
  │                                                    │  5. Checks policies│
  │                                                    │     - same ns? ✅  │
  │                                                    │     - rate limit? ✅│
  │                                                    │     - circuit? ✅  │
  │                                                    │                    │
  │  ◄─────────────────────────────────────────────────│  6. Rewrites dest  │
  │     Connected (transparent)                        │     to 127.0.0.1:10101
  │                                                    └────────────────────┘
  │
  │  GET /invoice HTTP/1.1
  │  Host: api-b.internal
  │  ───────────────────────────────────────────────────────►
  │                                                          │
  │                                                    ┌─────▼──────────────┐
  │                                                    │  App B (api-b)     │
  │                                                    │  on port 10101     │
  │                                                    └────────────────────┘
```

### Virtual DNS

The WASI virtual network provides a DNS resolver that answers queries for `*.internal`
based on the **caller's namespace**:

```rust
// crates/runtime/src/virtual_dns.rs

use std::collections::HashMap;
use std::net::IpAddr;
use common::types::AppId;

/// Per-app virtual DNS resolver.
/// The Supervisor builds one of these for each spawned instance.
pub struct VirtualDns {
    /// The namespace of the app that owns this resolver.
    namespace: String,

    /// Map from hostname → placeholder IP.
    /// The placeholder is always 127.0.0.1; the port is what matters.
    records: HashMap<String, IpAddr>,
}

impl VirtualDns {
    pub fn new(namespace: String) -> Self {
        VirtualDns {
            namespace,
            records: HashMap::new(),
        }
    }

    /// Register a known internal service.
    /// All *.internal names in this resolver resolve to 127.0.0.1.
    pub fn register_service(&mut self, bare_name: &str) {
        self.records.insert(
            format!("{bare_name}.internal"),
            IpAddr::from([127, 0, 0, 1]),
        );
    }

    pub fn resolve(&self, name: &str) -> Option<Vec<IpAddr>> {
        if name.ends_with(".internal") {
            // Only resolve names that belong to this namespace.
            // The actual target is determined at connect time by
            // socket_addr_check, not by DNS.
            self.records.get(name).map(|ip| vec![*ip])
        } else {
            None // fall through to real DNS
        }
    }
}
```

### socket_addr_check: The Interception Point

The Supervisor already configures a `socket_addr_check` in Step 04. We extend it to be
namespace-aware and policy-enforcing:

```rust
// crates/supervisor/src/network_interceptor.rs

use std::net::SocketAddr;
use std::sync::Arc;
use common::policy::PolicyDenied;
use common::types::AppId;

/// Transparent network interceptor for East-West traffic.
/// Lives in the Supervisor and is wired into every instance's WasiCtx.
pub struct NetworkInterceptor {
    pub registry: Arc<super::network::NamespaceRegistry>,
    pub source_app: AppId,
}

impl NetworkInterceptor {
    /// Called by wasmtime-wasi for every outbound TCP connect.
    /// Returns the decision: Allow or Deny.
    pub async fn check_connect(
        &self,
        _source_addr: SocketAddr,
        dest_addr: SocketAddr,
    ) -> ConnectDecision {
        // 1. External connections → allow
        if !dest_addr.ip().is_loopback() {
            return ConnectDecision::Allow;
        }

        // 2. Loopback to known app port → check namespace match
        if let Some(target_app) = self.registry.resolve_app_by_port(dest_addr.port()).await {
            if target_app.namespace() == self.source_app.namespace() {
                // Same namespace → allow
                return ConnectDecision::Allow;
            } else {
                // Cross-namespace → deny
                return ConnectDecision::Deny(format!(
                    "cross-namespace connection blocked: {} → {}",
                    self.source_app.namespace(),
                    target_app.namespace()
                ));
            }
        }

        // 3. Unknown loopback → allow (could be internal gateway)
        ConnectDecision::Allow
    }
}
```

**Note:** The Wasm app connects to `api-b.internal:8080`. The virtual DNS resolves the hostname
to `127.0.0.1`, so the app tries `127.0.0.1:8080`. The Supervisor's `socket_addr_check` sees
destination port `8080`. Since `8080` is the Wasm app's *internal* bind port (not a real host
port), the interceptor knows this is an internal call and looks up the actual target.

Actually, there's a simpler approach: the virtual DNS resolves `api-b.internal` to
`127.0.0.1:<actual_target_port>`. The app connects directly to the target port.
The `socket_addr_check` simply validates the connection against policies.

```rust
// Simplified virtual DNS — resolves directly to the target's actual loopback port.
impl VirtualDns {
    pub async fn resolve_target_port(
        &self,
        registry: &NamespaceRegistry,
        name: &str,
    ) -> Option<u16> {
        let bare_name = name.trim_end_matches(".internal");
        let addr = registry.resolve(&self.namespace, bare_name).await?;
        Some(addr.port())
    }
}
```

With this, the app's TCP stack connects straight to the target. The `socket_addr_check` is
a pure validator — it checks namespace and policies, and either allows or denies the
already-resolved address. No rewriting needed.

### Cross-Namespace Enforcement

If App A in `ns1` tries to connect to App B in `ns2`, the virtual DNS for `ns1` will **not**
resolve `api-b.internal` (because `api-b` is not registered in `ns1`). If the app somehow
learns App B's real port and connects directly, `socket_addr_check` catches it:

```rust
// In socket_addr_check
if target_app.namespace() != caller.namespace() {
    // Return EACCES to the Wasm module
    return Err(PolicyDenied::DestinationDenied { ... });
}
```

**Important limitation:** The `socket_addr_check` only blocks cross-namespace connections to
**direct app ports**. The internal gateway port (9080) is explicitly exempted from the
namespace check, so any app can connect to the gateway regardless of namespace. Namespace
isolation therefore relies primarily on service discovery: the Supervisor only injects
`<APP>_SERVICE_URL` env vars for same-namespace apps, so well-behaved apps will not attempt
cross-namespace connections. A determined app that knows the gateway port and target hostname
could reach apps in other namespaces through the gateway.

The Wasm module receives a standard `Connection refused` or `Permission denied` error (for
direct port connections) and can handle it gracefully. The app developer then knows to use
the external hostname (`https://api-b.ns2.example.com/`) for cross-namespace communication.

---

## 3. Internal HTTP Proxy (For Endpoint-Level Policies)

The `socket_addr_check` handles connection-level policies (namespace boundary for direct
app-to-app connections, circuit breaker, connection rate limiting). Note: the internal
gateway port (9080) is currently open to all namespaces — namespace isolation for gateway
traffic relies on service discovery only. For **HTTP-level** policies — per-path
authentication, per-path rate limits, request transforms — we need an HTTP proxy.

This proxy is **optional and transparent**. It only activates when an app's `GatewayRouteConfig`
has `endpoints` rules. The virtual DNS resolves the app's `.internal` name to the proxy port
instead of the direct port.

### Architecture

```
App A calls "http://api-b.production.internal:9080/users"
  │
  ▼
Virtual DNS resolves api-b.production.internal → 127.0.0.1
  │
  ▼
socket_addr_check allows 127.0.0.1:9080 (gateway port is currently open to all
  namespaces — namespace isolation relies on service discovery only)
  │
  ▼
Internal Proxy (Axum on 127.0.0.1:9080 — single port)
  │  1. Reads Host header: "api-b.production.internal:9080"
  │  2. Resolves target app from Host header via NamespaceRegistry
  │  3. Looks up endpoint rules for "/users"
  │  4. Applies auth (JWT / API key / none)
  │  5. Applies rate limit
  │  6. Applies circuit breaker check
  │  7. Forwards to api-b's real port (e.g., 127.0.0.1:10101)
  │
  ▼
App B receives the request
```

### Implementation

```rust
// crates/internal_gateway/src/lib.rs

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

/// The internal gateway port for East-West traffic on the loopback interface.
/// Defined in common::INTERNAL_GATEWAY_PORT (9080).

/// The transparent internal gateway.
/// Listens on a single loopback port (9080) for East-West traffic between
/// apps on the same node. Applies policies (rate limiting, circuit breaker,
/// auth) and forwards requests to the target app.
///
/// The internal gateway does not currently enforce namespace boundaries.
/// The `socket_addr_check` blocks cross-namespace connections to direct app
/// ports, but the gateway port (9080) is open to all namespaces. Namespace
/// isolation currently relies on service discovery: the Supervisor only
/// injects `<APP>_SERVICE_URL` env vars for same-namespace apps.
pub struct InternalGateway {
    pub registry: Arc<supervisor::network::NamespaceRegistry>,
    pub rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
    pub circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
    pub gateway_config: Arc<proxy::gateway::Gateway>,
}

impl InternalGateway {
    pub fn new(
        registry: Arc<supervisor::network::NamespaceRegistry>,
        rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
        circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
        gateway_config: Arc<proxy::gateway::Gateway>,
    ) -> Self { ... }

    pub async fn run(self) -> Result<(), std::io::Error> {
        // Binds a single TcpListener on 127.0.0.1:9080
        // Uses axum::serve directly (no per-namespace listeners)
    }
}

async fn proxy_handler(
    State(gw): State<Arc<InternalGateway>>,
    ConnectInfo(_peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Request<axum::body::Body>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    // 1. Parse target from Host header (e.g., "api-b.production.internal:9080")
    // 2. Resolve target to real loopback address via NamespaceRegistry
    // 3. Apply endpoint policies (auth, rate limit)
    // 4. Circuit breaker check
    // 5. Forward to real target, stripping internal headers
}
```

---

## 4. Per-Endpoint Gateway Security

### PathRule Config

The `GatewayRouteConfig` from Step 39 is extended with endpoint-level overrides. Rules are
evaluated **in order**; the first matching rule wins. If no rule matches, the route-level
defaults apply.

```rust
// crates/proxy/src/gateway/config.rs — additions

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayRouteConfig {
    // ... existing fields (auth, cors, transform, rate_limit, circuit_breaker) ...

    /// Per-endpoint security overrides.
    /// Evaluated top-to-bottom; first match wins.
    #[serde(default)]
    pub endpoints: Vec<EndpointRule>,
}

/// A single endpoint security rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointRule {
    /// Path prefix to match. Exact match for now (can be extended later).
    pub path: String,

    /// HTTP methods this rule applies to. Empty = all methods.
    #[serde(default)]
    pub methods: Vec<String>,

    /// Authentication policy for this endpoint.
    #[serde(default)]
    pub auth: EndpointAuth,

    /// Optional rate limit override.
    pub rate_limit: Option<RouteRateLimit>,
}

/// Authentication methods supported at the endpoint level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAuth {
    /// Inherit from route-level config.
    #[default]
    Inherit,

    /// No authentication required.
    None,

    /// Valid JWT required.
    Authenticated,

    /// Valid JWT + one of the specified roles.
    Roles {
        allowed_roles: Vec<String>,
        client_id: Option<String>,
    },

    /// API key authentication via X-Api-Key header.
    ApiKey,
}
```

### Middleware Pipeline with Endpoint Rules

The `request_filter` from Step 39 is updated to check endpoint rules after route resolution
but before applying route-level defaults:

```rust
// crates/proxy/src/service.rs — updated request_filter snippet

async fn request_filter(
    &self,
    session: &mut Session,
    ctx: &mut RequestCtx,
) -> PingoraResult<bool> {
    // 1. Route resolution (existing)
    let host = extract_host(session);
    let path = session.req_header().uri.path().to_string();
    let method = session.req_header().method.as_str();
    let resolved = self.router.resolve(&host, &path).await;

    let route_config = resolved.as_ref()
        .and_then(|r| self.gateway.get_route_config(&r.app_id));

    if resolved.is_none() {
        session.respond_error(502).await?;
        return Ok(true);
    }
    ctx.app_id = Some(resolved.unwrap().app_id.clone());
    ctx.route_config = route_config.clone();

    // 2. Find the first matching endpoint rule
    let endpoint_rule = route_config.as_ref()
        .and_then(|cfg| cfg.endpoints.iter()
            .find(|e| {
                e.path == path &&
                (e.methods.is_empty() || e.methods.contains(&method.to_string()))
            })
        );

    // 3. Determine effective auth policy
    let effective_auth = match endpoint_rule {
        Some(rule) if rule.auth != EndpointAuth::Inherit => &rule.auth,
        _ => &route_config.as_ref().map(|c| c.auth.clone()).unwrap_or(AuthPolicy::None),
    };

    // 4. Authentication
    if *effective_auth != AuthPolicy::None {
        match self.gateway.authenticate(session, effective_auth).await {
            Ok(identity) => ctx.user_identity = Some(identity),
            Err(_) => {
                session.respond_error(401).await?;
                return Ok(true);
            }
        }
    }

    // 5. Authorization
    if let Some(ref identity) = ctx.user_identity {
        if let Some(rule) = endpoint_rule {
            if let EndpointAuth::Roles { allowed_roles, client_id } = &rule.auth {
                if !authorize_roles(identity, allowed_roles, client_id.as_deref()) {
                    session.respond_error(403).await?;
                    return Ok(true);
                }
            }
        }
    }

    // 6. Rate limiting (endpoint override or route default)
    let rate_limit = endpoint_rule
        .and_then(|e| e.rate_limit.clone())
        .or_else(|| route_config.as_ref().and_then(|c| c.rate_limit.clone()));

    if let Some(ref rl) = rate_limit {
        if !self.check_rate_limit(ctx.app_id.as_ref().unwrap(), rl).await {
            session.respond_error(429).await?;
            return Ok(true);
        }
    }

    // 7. Circuit breaker (existing)
    if let Some(ref app_id) = ctx.app_id {
        if self.gateway.is_circuit_open(app_id) {
            session.respond_error(503).await?;
            return Ok(true);
        }
    }

    Ok(false)
}
```

### API Key Authentication

API keys are pre-shared keys for machine-to-machine or public endpoint auth. Stored as
SHA-256 hashes in redb.

```rust
// crates/proxy/src/gateway/api_key.rs

use sha2::{Sha256, Digest};
use std::collections::HashMap;

/// API key record stored in redb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub name: String,
    pub key_hash: String,    // "sha256$<hex>"
    pub scopes: Vec<String>, // allowed path prefixes
}

pub struct ApiKeyValidator {
    keys: HashMap<String, ApiKeyRecord>, // key_hash → record
}

impl ApiKeyValidator {
    pub fn validate(&self, header_value: &str, path: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(header_value);
        let hash = format!("sha256${:x}", hasher.finalize());

        if let Some(record) = self.keys.get(&hash) {
            return record.scopes.is_empty()
                || record.scopes.iter().any(|s| path.starts_with(s));
        }
        false
    }
}
```

---

## 5. Application Deployment Manifest

Today, deployment configuration is scattered across `AppConfig`, `PolicyConfig` (Step 33),
and `GatewayRouteConfig` (Step 39). Operators need a single manifest file that describes
*everything* about an application. The CLI parses it and sends it as a `DeployApp` NATS event.

### Full TOML Schema

```toml
# deploy-manifest.toml
# Single source of truth for deploying an app on the WASI Cloud Platform

[app]
# Identity
name        = "api-users"
version     = "v2"
namespace   = "production"          # default = "default"
description = "User management API"

# Wasm binary
wasm_artifact = "./target/wasm32-wasip2/release/api-users.wasm"
wasm_bind_port = 8080               # port the Wasm code believes it binds to

# Resource limits
[fuel]
quota         = 500_000_000         # per-request fuel units
memory_pages  = 2048                # Wasm pages (64 KiB each = 128 MiB)
max_instances = 10
idle_timeout_secs = 300

# Step 33 — WASI Policy Enforcement
[policy]
profile = "http_api"                # or explicit overrides below

[policy.network]
allow_outbound_tcp = true
allow_outbound_udp = false
allow_dns = true
max_outbound_connections = 50
allowed_cidrs = ["10.0.0.0/8"]
denied_cidrs = ["169.254.169.254/32"]
max_egress_bytes = 1_073_741_824    # 1 GB

[policy.filesystem]
max_open_fds = 64
max_fs_write_bytes = 52_428_800     # 50 MB
allow_file_create = false
allowed_paths = []

# Step 39 — API Gateway (route-level defaults)
[gateway]
host = "api-users.example.com"      # external hostname

[gateway.auth]
policy = "roles"                    # "none" | "authenticated" | "roles" | "api_key"
allowed_roles = ["admin", "user"]
client_id = "api-users"

[gateway.cors]
allowed_origins = ["https://app.example.com"]
allow_credentials = true
max_age_secs = 3600

[gateway.rate_limit]
requests_per_second = 500
burst_capacity = 100
distributed = true

[gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30

[gateway.transform]
add_headers = [
    ["X-Api-Version", "2"],
    ["X-Platform-Region", "eu-west-1"],
]
remove_headers = ["X-Internal-Token"]

# NEW — Per-endpoint security rules (override route defaults)
[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"                       # public health check
rate_limit = { requests_per_second = 100, burst_capacity = 50 }

[[gateway.endpoints]]
path = "/api/public"
methods = ["GET"]
auth = "api_key"                    # requires X-Api-Key header
rate_limit = { requests_per_second = 200, burst_capacity = 40 }

[[gateway.endpoints]]
path = "/api/users"
methods = ["GET", "POST", "PUT"]
auth = "roles"
allowed_roles = ["user", "admin"]
rate_limit = { requests_per_second = 100, burst_capacity = 20 }

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST", "DELETE"]
auth = "roles"
allowed_roles = ["admin"]           # stricter than the route default
rate_limit = { requests_per_second = 20, burst_capacity = 5 }

# Environment variables (non-secret)
[env]
LOG_LEVEL = "info"
DATABASE_POOL_SIZE = "10"

# Secrets (references — actual values are injected by the platform)
[secrets]
DATABASE_URL = { ref = "prod-postgres-url" }
JWT_SECRET   = { ref = "api-users-jwt-secret" }

# API Keys (for endpoint-level auth)
[[api_keys]]
name = "public-api-key"
key_hash = "sha256$abc123..."       # hashed key stored in redb
scopes = ["/api/public"]
```

### Example Manifests

**Public API (no auth, open CORS):**

```toml
[app]
name = "public-api"
version = "v1"
namespace = "default"

wasm_artifact = "./public-api.wasm"

[fuel]
quota = 100_000_000
memory_pages = 512

[policy]
profile = "static_site"

[gateway]
host = "public.example.com"

[gateway.cors]
allowed_origins = ["*"]

[[gateway.endpoints]]
path = "/"
methods = ["GET"]
auth = "none"
```

**Internal Microservice (strict namespace, no external host):**

```toml
[app]
name = "payment-processor"
version = "v1"
namespace = "payments"

wasm_artifact = "./payments.wasm"

[fuel]
quota = 1_000_000_000
memory_pages = 4096

[policy]
profile = "background_worker"

# No external host — this service is only reachable via internal gateway
# within the "payments" namespace, or via cross-namespace external call.

[[gateway.endpoints]]
path = "/process"
methods = ["POST"]
auth = "api_key"
rate_limit = { requests_per_second = 50, burst_capacity = 10 }
```

---

## 6. CLI Integration

```bash
# Deploy from a manifest
wasm-ctl deploy --manifest ./api-users.toml

# Or inline flags (manifest overrides defaults)
wasm-ctl deploy \
  --name api-users \
  --version v2 \
  --namespace production \
  --wasm ./api-users.wasm \
  --manifest ./api-users.toml

# List apps by namespace
wasm-ctl app list --namespace production

# View effective manifest for an app (merged config)
wasm-ctl app manifest api-users:v2 --namespace production

# Add an API key to an app
wasm-ctl gateway api-key add api-users:v2 \
  --namespace production \
  --name "mobile-client" \
  --scopes "/api/public" \
  --key "ak_live_xxxxxxxx"
```

---

## 7. Storage Schema Additions

```rust
// crates/storage/src/tables.rs

/// Key   : "<namespace>/<app_name>:<version>"
/// Value : JSON-serialized AppConfig (now includes namespace)
pub const CONFIGS: TableDefinition<&str, &str> = TableDefinition::new("configs");

/// Key   : "<namespace>/<app_name>:<version>/api_keys"
/// Value : JSON-serialized Vec<ApiKeyRecord>
pub const API_KEYS: TableDefinition<&str, &str> = TableDefinition::new("api_keys");
```

---

## 8. Security Considerations

### Namespace Isolation at the WASI Layer

Namespace isolation is enforced at two levels:

1. **Service discovery isolation (primary boundary):** The Supervisor only injects
   `<APP>_SERVICE_URL` env vars for apps in the same namespace. Well-behaved apps only
   learn about same-namespace services and therefore only make same-namespace connections.

2. **Direct port blocking (secondary boundary):** The `NetworkInterceptor` / `socket_addr_check`
   blocks cross-namespace connections to **direct app ports**. When a Wasm app attempts an
   outbound TCP connection to a known app port in a different namespace, the interceptor
   denies it.

**Known limitation:** The internal gateway port (9080) is explicitly exempted from the
namespace check in `socket_addr_check`. A determined app that knows the gateway port and
the target app's `.internal` hostname can connect to the gateway and reach apps in other
namespaces. This is a current limitation — the gateway does not enforce namespace boundaries.

The only way for a Wasm app to communicate with another app is through WASI socket syscalls.
The Supervisor controls these syscalls. A malicious app cannot:
- Forge its namespace (it's not exposed to the app)
- Bypass the network interceptor (all outbound TCP is intercepted by `socket_addr_check`)
- Connect to a **direct app port** in another namespace (cross-namespace connections to
  known app ports are denied at the WASI layer)

However, a malicious app **can**:
- Connect to the internal gateway port (9080) from any namespace — this port is open to all
- Reach apps in other namespaces through the gateway if it knows the target hostname

### Cross-Namespace Data Leakage

Even if an attacker compromises a Wasm module:
1. The WASI policy (Step 33) restricts outbound connections to allowed CIDRs.
2. The `socket_addr_check` blocks cross-namespace internal connections.
3. The only escape route is the external gateway, which enforces TLS + OIDC + rate limiting.

### API Key Storage

API keys are stored as SHA-256 hashes, never plaintext. The CLI accepts a plaintext key
once (`--key`), hashes it immediately, and discards the plaintext. Only the hash is written
to redb and replicated via NATS.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Namespaces
- [ ] `AppConfig` has a `namespace` field (default = "default")
- [ ] `AppId` supports `namespace()`, `bare_name()`, and qualified string format
- [ ] `NamespaceRegistry` replaces `LocalServiceRegistry`, includes `port_to_app` reverse lookup
- [ ] Two apps with the same bare name in different namespaces do not collide in storage
- [ ] The namespace is **never** exposed to the Wasm app via env vars

### Transparent Internal Routing
- [ ] Virtual DNS resolves `*.internal` names within the caller's namespace
- [ ] `socket_addr_check` intercepts ALL outbound TCP connections
- [ ] Connection to a local app in the same namespace is allowed after policy checks
- [ ] Connection to a local app in a different namespace returns `ECONNREFUSED`
- [ ] External connections fall through to the external NetworkPolicy from Step 33
- [ ] The Wasm app uses normal HTTP URLs; no platform-specific headers or env vars required

### Internal HTTP Proxy (Optional)
- [ ] A loopback Axum proxy runs on `127.0.0.1:9080` (single port)
- [ ] Activated only when an app has endpoint-level gateway rules
- [ ] Namespace isolation relies on service discovery (apps only learn about same-namespace services)
- [ ] Applies endpoint auth, rate limit, and transforms
- [ ] Forwards to the real target port after stripping internal metadata

### Cross-Namespace Routing
- [ ] Apps in namespace A cannot reach apps in namespace B via internal DNS or direct ports
- [ ] Cross-namespace calls must use the external hostname and pass through Pingora
- [ ] The Supervisor does not inject `INTERNAL_GATEWAY_URL` or `APP_NAMESPACE` into Wasm modules

### Deployment Manifest
- [ ] A single TOML manifest describes app identity, fuel, policy, gateway, endpoints, env, and secrets
- [ ] `wasm-ctl deploy --manifest ./app.toml` parses and deploys the app
- [ ] The manifest schema is validated at deploy time
- [ ] `wasm-ctl app manifest <app>` reconstructs the effective manifest from redb

### Per-Endpoint Security
- [ ] `EndpointRule` supports path + method matching
- [ ] Endpoint-level `auth` overrides route-level `auth`
- [ ] `AuthPolicy::ApiKey` validates `X-Api-Key` against SHA-256 hashed keys in redb
- [ ] API keys can be scoped to specific paths
- [ ] Endpoint rules are evaluated top-to-bottom; first match wins

### Integration
- [ ] Internal proxy reuses existing `RateLimiter` and `CircuitBreakerManager`
- [ ] Deploy events include namespace; all nodes store the app under the correct namespace key
- [ ] Route management (Step 15) includes namespace in the Host→AppId resolution

### Tests
- [ ] A test deploys two apps in the same namespace and verifies transparent internal routing
- [ ] A test deploys two apps in different namespaces and verifies `ECONNREFUSED` on internal connect
- [ ] A test verifies that `GET /health` with `auth = "none"` passes without a JWT
- [ ] A test verifies that `POST /api/admin` with `auth = "roles"` and wrong role returns 403
- [ ] A test verifies that an `X-Api-Key` header matching a stored hash allows access
- [ ] A test verifies that an invalid API key returns 401
- [ ] A test verifies cross-namespace traffic succeeds when routed through the external hostname
- [ ] A test verifies that a Wasm app can connect to `api-b.internal` with zero platform-specific config

---

## Migration Path

### Phase 1: Namespace Infrastructure (Non-Breaking)

- Add `namespace` field to `AppConfig` (default = "default")
- Migrate `LocalServiceRegistry` → `NamespaceRegistry` with `port_to_app`
- Update storage keys to include namespace
- **No behavior change** for existing apps (all live in "default")

### Phase 2: Transparent Internal Routing

- Add `VirtualDns` per-instance
- Update `socket_addr_check` to be namespace-aware
- Block cross-namespace connections at the WASI layer
- **Apps do not change** — they just start using `.internal` hostnames

### Phase 3: Internal HTTP Proxy

- Add `crates/internal_gateway/` (Axum-based, single loopback port 9080)
- Activate per-app when endpoint rules exist
- Namespace isolation relies on service discovery filtering

### Phase 4: Per-Endpoint Security + Unified Manifest

- Add `EndpointRule` and `EndpointAuth` to `GatewayRouteConfig`
- Update `request_filter()` to evaluate endpoint rules
- Add `ApiKeyValidator` and `API_KEYS` redb table
- Define the full TOML schema and update `wasm-ctl deploy --manifest`
