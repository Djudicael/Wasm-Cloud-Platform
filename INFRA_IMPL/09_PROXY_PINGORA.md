# Step 09 — Pingora Reverse Proxy

## Goal
Build the `proxy` crate around Cloudflare's `pingora` library. Pingora acts as the entry point
for all external (North-South) HTTP traffic. It:
- Terminates TLS
- Looks up the target app from the request Host/path
- Picks an upstream instance address (round-robin across available instances)
- Signals the Supervisor for a cold-start if no instances are running
- Reports request latency back to the metrics system

---

## Context & Rationale

### The Problem This Solves

External HTTP traffic needs a single entry point that can:
1. Terminate TLS (so Wasm apps don't need to manage certificates)
2. Route requests to the right app based on the Host header
3. Load-balance across multiple instances of the same app
4. Trigger cold-starts when an app has no running instances
5. Enforce rate limits before load reaches the Wasm layer

Without a proxy, every Wasm instance would need to manage its own TLS certificates,
and there would be no way to route a single domain to multiple instances.

### Why Pingora (not NGINX, Traefik, Envoy)?

| Option | Language | Dynamic config | Integration |
|--------|----------|----------------|-------------|
| NGINX | C | Requires reload signal | Separate process, IPC needed |
| Traefik | Go | Built-in, via API | Separate process, HTTP API |
| Envoy | C++ | xDS protocol | Separate process, gRPC API |
| Pingora | Rust | In-process shared data | Same binary, shared memory |

The key advantage of Pingora: it runs **in the same process** as the Supervisor. The
`UpstreamRegistry` and `HostRouter` are plain Rust structs protected by `RwLock`. The
Supervisor writes to them directly; Pingora reads from them. There is no IPC, no API call,
no serialization — just a memory read. This makes upstream updates (instance spawned or
killed) reflect in the proxy **instantly**, with zero latency.

With any external proxy, an upstream update requires an API call (HTTP or gRPC) to the proxy
process. During that round-trip, in-flight requests may be routed to a dead instance.

### The Cold-Start Callback Design

`WasmProxy` holds a `cold_start` function pointer:
```
cold_start: Arc<dyn Fn(AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync>
```

This is how Pingora calls back into the Supervisor without holding a direct reference to it.
This design avoids a circular dependency: `proxy` would depend on `supervisor` which depends
on `proxy` (for the UpstreamRegistry).

Instead:
- `proxy` defines the `cold_start` type as a function pointer
- `node/main.rs` wires them together: `cold_start = |id| supervisor.ensure_instance(id)`
- `proxy` calls it; `supervisor` handles it; neither crate knows about the other

### Round-Robin Load Balancing

The `UpstreamRegistry` uses a per-app `AtomicUsize` counter for round-robin. The counter
increments on every `next()` call and wraps with modulo. This is lock-free for reads:
`AtomicUsize::fetch_add` uses a single CPU instruction (LOCK XADD on x86), so even with
thousands of concurrent requests, there is no contention on the counter.

The `RwLock` on the outer map is only held for `add()` and `remove()` operations (infrequent)
or for `read()` (many concurrent readers, no blocking between them).

### Host-Based Routing vs Path-Based Routing

The `HostRouter` uses the HTTP `Host` header for routing. This is the correct approach for
multi-tenant platforms because:
- Each tenant has their own domain (`tenant-a.com`, `tenant-b.com`)
- The Wasm app does not need to handle routing logic — it receives requests as if it owns
  the entire domain
- Wildcard matching (`www.` stripping) handles common browser behavior

Path-based routing (`/app-a/`, `/app-b/`) is not implemented as a primary mechanism because
it leaks the platform's internal structure to tenants.

### TLS Termination at the Proxy

TLS is terminated at Pingora. The connection from Pingora to the Wasm instance is plain HTTP
over loopback (`127.0.0.1`). This is standard practice and is safe because:
- Loopback traffic never leaves the machine
- The Wasm instance is in the same OS process, so there is no network interception possible
- This eliminates certificate management complexity inside Wasm apps

---

---

## 1. Upstream Registry

Shared between the Proxy and the Supervisor. The Supervisor writes to it; Pingora reads from it.

```rust
// crates/proxy/src/upstream.rs
use common::types::AppId;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
};
use tokio::sync::RwLock;

/// Thread-safe registry of all live instance addresses, per app.
#[derive(Clone, Default)]
pub struct UpstreamRegistry {
    /// app_id → (round-robin counter, list of addresses)
    inner: Arc<RwLock<HashMap<String, (AtomicUsize, Vec<SocketAddr>)>>>,
}

impl UpstreamRegistry {
    pub async fn add(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.inner.write().await;
        let entry = map.entry(app_id.0.clone())
            .or_insert_with(|| (AtomicUsize::new(0), Vec::new()));
        if !entry.1.contains(&addr) {
            entry.1.push(addr);
            tracing::info!(app = %app_id.0, %addr, "upstream added");
        }
    }

    pub async fn remove(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.inner.write().await;
        if let Some(entry) = map.get_mut(&app_id.0) {
            entry.1.retain(|a| a != addr);
            tracing::info!(app = %app_id.0, %addr, "upstream removed");
        }
    }

    /// Get the next upstream address using round-robin.
    /// Returns None if no instances are available (cold start needed).
    pub async fn next(&self, app_id: &AppId) -> Option<SocketAddr> {
        let map = self.inner.read().await;
        let (counter, addrs) = map.get(&app_id.0)?;
        if addrs.is_empty() {
            return None;
        }
        let idx = counter.fetch_add(1, Ordering::Relaxed) % addrs.len();
        Some(addrs[idx])
    }

    pub async fn count(&self, app_id: &AppId) -> usize {
        let map = self.inner.read().await;
        map.get(&app_id.0).map(|(_, v)| v.len()).unwrap_or(0)
    }
}
```

---

## 2. Request Routing (App Resolution)

Determine which app handles a given request based on the Host header.

```rust
// crates/proxy/src/router.rs
use common::types::AppId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Maps Host header values to AppIds.
/// e.g. "api.myapp.com" → AppId("api-users:v2")
#[derive(Clone, Default)]
pub struct HostRouter {
    routes: Arc<RwLock<HashMap<String, AppId>>>,
}

impl HostRouter {
    pub async fn add_route(&self, host: String, app_id: AppId) {
        self.routes.write().await.insert(host, app_id);
    }

    pub async fn resolve(&self, host: &str) -> Option<AppId> {
        let routes = self.routes.read().await;
        // Exact match first
        if let Some(id) = routes.get(host) {
            return Some(id.clone());
        }
        // Wildcard: strip "www." prefix
        let bare = host.trim_start_matches("www.");
        routes.get(bare).cloned()
    }
}
```

---

## 3. ProxyHttp Implementation

The core Pingora hook — called for every incoming HTTP request.

```rust
// crates/proxy/src/service.rs
use async_trait::async_trait;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};
use pingora_core::Result as PingoraResult;
use std::sync::Arc;
use std::time::Duration;
use super::{upstream::UpstreamRegistry, router::HostRouter};
use common::types::AppId;

/// Context passed through the Pingora request pipeline.
pub struct RequestCtx {
    pub app_id: Option<AppId>,
    pub upstream_addr: Option<std::net::SocketAddr>,
    pub start: std::time::Instant,
}

/// The main Pingora proxy service.
pub struct WasmProxy {
    pub router: Arc<HostRouter>,
    pub upstream: Arc<UpstreamRegistry>,
    /// Callback to trigger a cold-start when no instances are running.
    /// Returns the address of the newly spawned instance.
    pub cold_start: Arc<dyn Fn(AppId) -> futures::future::BoxFuture<'static, Option<std::net::SocketAddr>> + Send + Sync>,
}

#[async_trait]
impl ProxyHttp for WasmProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> RequestCtx {
        RequestCtx {
            app_id: None,
            upstream_addr: None,
            start: std::time::Instant::now(),
        }
    }

    /// Step 1: Resolve the app from the Host header.
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
    ) -> PingoraResult<bool> {
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // Health check bypass
        if host.is_empty() || session.req_header().uri.path() == "/_platform/health" {
            return Ok(false); // Continue without routing
        }

        ctx.app_id = self.router.resolve(&host).await;
        if ctx.app_id.is_none() {
            tracing::warn!(host, "no route found for host");
            // Will result in a 502 from Pingora
        }
        Ok(false) // false = do NOT abort the request
    }

    /// Step 2: Select the upstream (with cold-start if needed).
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
    ) -> PingoraResult<Box<HttpPeer>> {
        let app_id = ctx.app_id.as_ref()
            .ok_or_else(|| pingora_core::Error::new_str("no app for this host"))?;

        // Try to get an existing instance
        let addr = match self.upstream.next(app_id).await {
            Some(addr) => addr,
            None => {
                // Cold start: ask the Supervisor to spawn an instance
                tracing::info!(app = %app_id.0, "cold start triggered");
                (self.cold_start)(app_id.clone()).await
                    .ok_or_else(|| pingora_core::Error::new_str("cold start failed"))?
            }
        };

        ctx.upstream_addr = Some(addr);
        Ok(Box::new(HttpPeer::new(
            addr,
            false, // not TLS to upstream (internal)
            app_id.0.clone(),
        )))
    }

    /// Step 3 (optional): Modify request headers before forwarding.
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora_http::RequestHeader,
        ctx: &mut RequestCtx,
    ) -> PingoraResult<()> {
        if let Some(id) = &ctx.app_id {
            upstream_request.insert_header("X-App-Id", &id.0)?;
        }
        Ok(())
    }

    /// Step 4 (optional): Log after the response is sent.
    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora_core::Error>,
        ctx: &mut RequestCtx,
    ) {
        let latency_ms = ctx.start.elapsed().as_millis();
        let status = session.response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        tracing::info!(
            app = ctx.app_id.as_ref().map(|a| a.0.as_str()).unwrap_or("unknown"),
            status,
            latency_ms,
            "request completed"
        );
        // TODO: push to metrics channel
    }
}
```

---

## 4. TLS Configuration

The implemented server uses separate Pingora services for the cleartext and TLS
listeners. Cleartext retains h2c preface handling. TLS enables HTTP/2 and
HTTP/1.1 through ALPN; h2c preface sniffing must not be enabled on that service
because the TLS stream cannot reliably support Pingora's cleartext peek path.
The P10-09 runtime contract verifies both HTTPS and plaintext rejection.

```rust
// crates/proxy/src/tls.rs
use pingora_core::listeners::TlsSettings;
use std::path::Path;

/// Load TLS cert and key from PEM files.
/// In production, use certbot + ACME or mount from secrets.
pub fn tls_settings(cert_pem: &Path, key_pem: &Path) -> TlsSettings {
    TlsSettings::intermediate(
        cert_pem.to_str().unwrap(),
        key_pem.to_str().unwrap(),
    ).expect("TLS config failed")
}
```

---

## 5. Server Assembly

```rust
// crates/proxy/src/lib.rs
pub mod upstream;
pub mod router;
pub mod service;
pub mod tls;

use pingora::{server::Server, services::listening::Service};
use pingora_proxy::http_proxy_service;
use service::WasmProxy;
use std::sync::Arc;

pub struct ProxyServer {
    server: Server,
}

impl ProxyServer {
    pub fn build(
        proxy: WasmProxy,
        http_port: u16,
        https_port: Option<u16>,
        tls: Option<pingora_core::listeners::TlsSettings>,
    ) -> Self {
        let mut server = Server::new(None).expect("Pingora server init failed");
        server.bootstrap();

        let proxy = Arc::new(proxy);
        let mut svc = http_proxy_service(&server.configuration, proxy);

        // HTTP listener
        svc.add_tcp(&format!("0.0.0.0:{http_port}"));

        // HTTPS listener (optional)
        if let (Some(port), Some(tls_cfg)) = (https_port, tls) {
            svc.add_tls(&format!("0.0.0.0:{port}"), tls_cfg);
        }

        server.add_service(svc);
        ProxyServer { server }
    }

    /// Run the Pingora server (blocks the current thread).
    pub fn run(self) -> ! {
        self.server.run_forever()
    }
}
```

---

## 6. Rate Limiting (per App)

Add per-app rate limiting using a token bucket inside the `request_filter` hook.

```rust
// crates/proxy/src/rate_limiter.rs
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

struct Bucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl Bucket {
    fn consume(&mut self, tokens: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= tokens {
            self.tokens -= tokens;
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    requests_per_second: f64,
    burst_size: f64,
}

impl RateLimiter {
    pub fn new(rps: f64, burst: f64) -> Self {
        RateLimiter {
            buckets: Default::default(),
            requests_per_second: rps,
            burst_size: burst,
        }
    }

    /// Returns true if the request should be allowed through.
    pub async fn allow(&self, app_id: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry(app_id.to_string()).or_insert_with(|| Bucket {
            tokens: self.burst_size,
            max_tokens: self.burst_size,
            refill_rate: self.requests_per_second,
            last_refill: Instant::now(),
        });
        bucket.consume(1.0)
    }
}
```

---

## 7. Admin API (separate port)

A small Axum server on port 9090 that lets operators query the proxy state.

```rust
// crates/proxy/src/admin.rs
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;
use super::upstream::UpstreamRegistry;

#[derive(Clone)]
struct AdminState {
    upstream: Arc<UpstreamRegistry>,
}

pub fn admin_router(upstream: Arc<UpstreamRegistry>) -> Router {
    let state = AdminState { upstream };
    Router::new()
        .route("/upstreams", get(list_upstreams))
        .with_state(state)
}

async fn list_upstreams(State(s): State<AdminState>) -> Json<serde_json::Value> {
    let map = s.upstream.inner.read().await;
    let out: serde_json::Map<_, _> = map.iter()
        .map(|(k, (_, addrs))| {
            let addrs_str: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            (k.clone(), serde_json::json!(addrs_str))
        })
        .collect();
    Json(serde_json::Value::Object(out))
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Upstream Registry
- [ ] `add(app_id, addr)` stores the address; `next(app_id)` returns it
- [ ] `remove(app_id, addr)` removes the address; subsequent `next()` returns `None` if pool is empty
- [ ] `next()` cycles through multiple addresses in round-robin order (add 3, call next 6 times = each appears twice)
- [ ] `add()`, `remove()`, and `next()` are safe to call from multiple threads simultaneously

### Host Router
- [ ] `add_route(host, app_id)` makes `resolve(host)` return that `AppId`
- [ ] `resolve()` for an unknown host returns `None`
- [ ] `resolve()` strips the `www.` prefix as a fallback match
- [ ] `remove_route(host)` removes the mapping; subsequent `resolve()` returns `None`
- [ ] `load_from_store(store)` populates all routes from redb on startup

### ProxyHttp Implementation
- [ ] `request_filter()` correctly extracts the `Host` header and resolves the app
- [ ] `upstream_peer()` returns an address from the registry when instances are running
- [ ] `upstream_peer()` triggers the `cold_start` callback when no instances are available
- [ ] `upstream_peer()` returns a Pingora error (not a panic) when cold start fails
- [ ] `upstream_request_filter()` injects the `X-App-Id` header into forwarded requests
- [ ] `logging()` records status code and latency after each response

### TLS
- [ ] The proxy starts and accepts HTTPS connections when cert and key paths are provided
- [ ] HTTP and HTTPS listeners can run simultaneously on different ports
- [ ] An invalid cert/key path causes a clear startup error — not a silent failure

### Rate Limiter
- [ ] Requests within the burst limit pass through
- [ ] Requests exceeding the rate are denied (token bucket correctly depleted)
- [ ] The bucket refills over time; requests are accepted again after the refill window

### Admin API
- [ ] `GET /upstreams` returns a JSON map of `app_id → [addr, ...]`
- [ ] The admin API is reachable on its configured port independently of the proxy port

### Tests
- [ ] A test adds 3 upstreams and verifies round-robin over 6 calls
- [ ] A test verifies that a request to an unknown host returns 502
- [ ] A test verifies cold-start is triggered when the upstream pool is empty
