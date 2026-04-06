# Step 24 — Rate Limiting & DDoS Protection

## Goal
Implement request rate limiting at the Pingora proxy layer. The system must:
- Enforce per-tenant request rate limits before traffic reaches the Wasm layer
- Enforce per-source-IP connection limits
- Apply backpressure from the Supervisor to Pingora when fuel is exhausted
- Protect the platform from volumetric floods and slowloris-style attacks
- Operate without external dependencies (no Redis, no centralized rate limiter)

---

## Context & Rationale

### The Problem This Solves

Fuel metering (step 03) protects individual Wasm instances from consuming too much CPU.
But fuel only applies **after** a request has been routed to a running instance. The
damage has already been done:

1. Pingora has accepted the TCP connection and parsed the HTTP request
2. The Supervisor has potentially cold-started an instance (< 10ms, but still work)
3. The Wasm instance has begun executing before fuel runs out

A volumetric attack — 100,000 requests/second from a botnet — would overwhelm the proxy
and Supervisor long before fuel metering can help. The TCP accept queue fills, legitimate
requests time out, and the node becomes unresponsive.

Rate limiting at the **proxy layer** (before Wasm execution) is the correct defense. It
rejects excess requests immediately with `429 Too Many Requests`, costing only a few
microseconds per rejected request.

### Why Per-Tenant (Not Just Per-IP) Rate Limits?

Per-IP rate limiting alone is insufficient for a multi-tenant platform:

- **Shared IPs**: Corporate NATs, CDNs, and proxies mean thousands of legitimate users
  share a single public IP. Limiting per-IP would block all of Company X because one
  employee is refreshing a page too fast.
- **Distributed attacks**: A botnet uses thousands of unique IPs. Per-IP limits don't help
  when each IP sends only 5 req/s but collectively they send 50,000 req/s.

Per-tenant limits set a **global budget** for each app: "api-users may receive at most
1,000 requests per second across all IPs". Once the budget is exhausted, all requests
to that app are rejected — regardless of source IP.

Per-IP limits are a **secondary defense**: "no single IP may send more than 100 req/s to
any app". This catches the common case (one misbehaving client) without affecting other
legitimate users behind the same NAT.

### Why In-Process (Not External Redis)?

Centralized rate limiters (Redis, Envoy rateLimit service) add a network round-trip on
every request to check the counter. At 50,000 req/s this means 50,000 Redis calls/second
— which itself becomes a bottleneck and a single point of failure.

Since this platform follows the shared-nothing principle (step 00), rate limiting must be
**local to each node**. Each node tracks its own counters. The effective global rate limit
for a tenant is `per_node_limit × number_of_nodes`. This is intentional:

- A 3-node cluster with a per-node limit of 1,000 req/s gives an effective global limit
  of 3,000 req/s
- If one node dies, the limit naturally drops to 2,000 req/s (correct behavior — fewer
  nodes should serve fewer requests)

This is the same approach Cloudflare uses in their edge rate limiter: each PoP enforces
local limits independently.

### The Token Bucket Algorithm

Token bucket is the standard algorithm for rate limiting with burst tolerance:

```
Bucket capacity = max_burst (e.g. 50)
Refill rate = rate_limit_per_second (e.g. 1000)

On each request:
  1. Calculate tokens to add since last refill:
     elapsed = now - last_refill_time
     new_tokens = elapsed * refill_rate
     tokens = min(tokens + new_tokens, max_burst)
  2. If tokens >= 1.0:
     tokens -= 1.0
     ALLOW request
  3. Else:
     DENY request (429 Too Many Requests)
```

The burst capacity is critical: without it, a brief traffic spike (e.g., 50 users clicking
a link simultaneously) would be rejected even if the average rate is well below the limit.
The burst absorbs short spikes; the refill rate enforces the long-term average.

### Backpressure: Supervisor → Pingora

When a node's fuel budget is exhausted (all instances saturated, no room to spawn more),
Pingora should not continue accepting requests that will only queue up. The backpressure
mechanism:

```
Supervisor: fuel headroom = total fuel budget - fuel consumed this second
  │
  ▼ (updated every health tick, 5s)
AtomicBool: node_accepting_requests
  │
  ▼ (read by Pingora on every request)
Pingora: if !node_accepting_requests → 503 Service Unavailable
```

This is a binary signal (accepting / not accepting), not a graduated one. The graduation
happens at the cluster level: the NATS `NodeLoad` event (step 12) tells other nodes that
this node is at capacity, and cross-node request steering redirects traffic to less-loaded
nodes. Locally, the node simply stops accepting when it has nothing to offer.

### Slowloris Protection

A slowloris attack sends HTTP headers very slowly — one byte per second — tying up a
connection slot without completing a request. Pingora's built-in timeout settings handle
this, but explicit configuration is required:

- `read_timeout`: Maximum time to receive the complete HTTP request (default: 10s)
- `idle_timeout`: Maximum time a keep-alive connection can be idle (default: 60s)
- `max_header_size`: Reject requests with excessively large headers (default: 8KB)

These are set at the Pingora `HttpProxy` level and apply to all connections before the
request is routed to any app.

---

---

## 1. Rate Limiter Data Structures

```rust
// crates/proxy/src/rate_limit.rs
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Per-app rate limit configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum sustained requests per second per node for this app.
    pub requests_per_second: u32,

    /// Burst capacity — absorbs short spikes above the sustained rate.
    /// When the bucket is full, this many extra requests can be served instantly.
    pub burst_capacity: u32,

    /// Maximum requests per second from a single IP address.
    /// Applies independently of the app-level limit.
    pub per_ip_limit: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig {
            requests_per_second: 1_000,
            burst_capacity: 50,
            per_ip_limit: 100,
        }
    }
}

/// Token bucket state for a single rate limit counter.
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,      // tokens per second
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate_per_second: u32, burst: u32) -> Self {
        TokenBucket {
            tokens: burst as f64,
            max_tokens: burst as f64,
            refill_rate: rate_per_second as f64,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns true if allowed.
    fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        // Refill tokens based on elapsed time
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Node-local rate limiter. No external dependencies.
pub struct RateLimiter {
    /// Per-app token buckets (app_id → bucket).
    app_buckets: RwLock<HashMap<String, TokenBucket>>,

    /// Per-IP token buckets (ip → bucket).
    /// Pruned periodically to prevent unbounded growth.
    ip_buckets: RwLock<HashMap<IpAddr, TokenBucket>>,

    /// Per-app rate limit configs.
    configs: RwLock<HashMap<String, RateLimitConfig>>,

    /// Default config for apps without explicit limits.
    default_config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(default_config: RateLimitConfig) -> Self {
        RateLimiter {
            app_buckets: RwLock::new(HashMap::new()),
            ip_buckets: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            default_config,
        }
    }

    /// Set a custom rate limit for a specific app.
    pub async fn set_app_config(&self, app_id: &str, config: RateLimitConfig) {
        self.configs.write().await.insert(app_id.to_string(), config);
    }

    /// Check whether a request should be allowed.
    /// Returns Ok(()) if allowed, Err(RateLimitDenied) with reason if rejected.
    pub async fn check_request(
        &self,
        app_id: &str,
        source_ip: IpAddr,
    ) -> Result<(), RateLimitDenied> {
        let config = {
            let configs = self.configs.read().await;
            configs.get(app_id).cloned().unwrap_or(self.default_config.clone())
        };

        // 1. Check per-app limit
        {
            let mut buckets = self.app_buckets.write().await;
            let bucket = buckets.entry(app_id.to_string()).or_insert_with(|| {
                TokenBucket::new(config.requests_per_second, config.burst_capacity)
            });
            if !bucket.try_acquire() {
                return Err(RateLimitDenied::AppLimitExceeded {
                    app_id: app_id.to_string(),
                    limit: config.requests_per_second,
                });
            }
        }

        // 2. Check per-IP limit
        {
            let mut buckets = self.ip_buckets.write().await;
            let bucket = buckets.entry(source_ip).or_insert_with(|| {
                TokenBucket::new(config.per_ip_limit, config.per_ip_limit)
            });
            if !bucket.try_acquire() {
                return Err(RateLimitDenied::IpLimitExceeded {
                    ip: source_ip,
                    limit: config.per_ip_limit,
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum RateLimitDenied {
    AppLimitExceeded { app_id: String, limit: u32 },
    IpLimitExceeded { ip: IpAddr, limit: u32 },
}

impl std::fmt::Display for RateLimitDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RateLimitDenied::AppLimitExceeded { app_id, limit } =>
                write!(f, "app '{}' rate limit exceeded ({}/s)", app_id, limit),
            RateLimitDenied::IpLimitExceeded { ip, limit } =>
                write!(f, "IP {} rate limit exceeded ({}/s)", ip, limit),
        }
    }
}
```

---

## 2. IP Bucket Pruning

Without pruning, the `ip_buckets` map grows unbounded — every unique IP that ever made a
request stays in memory forever. The prune task runs every 60 seconds and removes entries
that haven't been used in 5 minutes.

```rust
// crates/proxy/src/rate_limit.rs (continued)
use std::time::Duration;

impl RateLimiter {
    /// Start a background task that prunes stale IP buckets.
    pub fn start_prune_loop(self: Arc<Self>) {
        let limiter = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                limiter.prune_stale_ip_buckets(Duration::from_secs(300)).await;
            }
        });
    }

    async fn prune_stale_ip_buckets(&self, max_age: Duration) {
        let now = Instant::now();
        let mut buckets = self.ip_buckets.write().await;
        let before = buckets.len();
        buckets.retain(|_, bucket| {
            now.duration_since(bucket.last_refill) < max_age
        });
        let pruned = before - buckets.len();
        if pruned > 0 {
            tracing::debug!(pruned, remaining = buckets.len(), "pruned stale IP rate limit buckets");
        }
    }
}
```

---

## 3. Backpressure Signal

A shared atomic flag that the Supervisor sets when the node is at fuel capacity.
Pingora reads this flag on every request — zero-cost when the node is healthy.

```rust
// crates/proxy/src/backpressure.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared between Supervisor and Pingora.
/// When false, Pingora rejects new requests with 503.
#[derive(Clone)]
pub struct BackpressureSignal {
    accepting: Arc<AtomicBool>,
}

impl BackpressureSignal {
    pub fn new() -> Self {
        BackpressureSignal {
            accepting: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Called by Pingora on every request. Returns true if the node can accept work.
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Relaxed)
    }

    /// Called by the Supervisor when fuel headroom is exhausted.
    pub fn set_rejecting(&self) {
        self.accepting.store(false, Ordering::Relaxed);
        tracing::warn!("backpressure: node is now REJECTING new requests");
    }

    /// Called by the Supervisor when fuel headroom recovers.
    pub fn set_accepting(&self) {
        self.accepting.store(true, Ordering::Relaxed);
        tracing::info!("backpressure: node is now ACCEPTING requests");
    }
}
```

---

## 4. Integration with Pingora Request Flow

The rate limiter is checked **before** upstream resolution. Rejected requests never reach
the Supervisor or any Wasm instance.

```rust
// crates/proxy/src/service.rs (modified upstream_peer — additions shown)
use crate::rate_limit::{RateLimiter, RateLimitDenied};
use crate::backpressure::BackpressureSignal;
use pingora::http::ResponseHeader;
use std::net::IpAddr;

pub struct WasmProxy {
    pub upstream_registry: UpstreamRegistry,
    pub host_router: HostRouter,
    pub rate_limiter: Arc<RateLimiter>,
    pub backpressure: BackpressureSignal,
    pub cold_start: Arc<dyn Fn(AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync>,
}

// Inside the ProxyHttp implementation:
impl WasmProxy {
    /// Called by Pingora for every incoming request, before upstream selection.
    async fn check_rate_limit(
        &self,
        app_id: &str,
        source_ip: IpAddr,
    ) -> Result<(), Box<pingora::Error>> {
        // 1. Node-level backpressure (fuel exhausted)
        if !self.backpressure.is_accepting() {
            return Err(pingora::Error::new_str("node at capacity")
                .set_cause("fuel budget exhausted")
                .set_retry(true));
            // Pingora returns 503 with Retry-After header
        }

        // 2. Per-app and per-IP rate limits
        match self.rate_limiter.check_request(app_id, source_ip).await {
            Ok(()) => Ok(()),
            Err(RateLimitDenied::AppLimitExceeded { app_id, limit }) => {
                tracing::warn!(app = %app_id, limit, "app rate limit exceeded");
                Err(pingora::Error::new_str("rate limit exceeded")
                    .set_cause("app rate limit")
                    .set_retry(true))
                // Pingora returns 429
            }
            Err(RateLimitDenied::IpLimitExceeded { ip, limit }) => {
                tracing::warn!(%ip, limit, "IP rate limit exceeded");
                Err(pingora::Error::new_str("rate limit exceeded")
                    .set_cause("ip rate limit")
                    .set_retry(true))
                // Pingora returns 429
            }
        }
    }
}
```

---

## 5. Pingora Timeout Configuration (Slowloris Defense)

```rust
// crates/proxy/src/config.rs
use std::time::Duration;

/// Timeouts configured at the Pingora HttpProxy level.
/// These apply to all connections before routing.
pub struct ProxyTimeouts {
    /// Maximum time to receive the full HTTP request headers.
    /// Defends against slowloris: if a client sends headers slower than this,
    /// the connection is dropped.
    pub request_header_read_timeout: Duration,

    /// Maximum time to receive the full HTTP request body.
    /// Prevents clients from holding connections open with slow uploads.
    pub request_body_read_timeout: Duration,

    /// Maximum time a keep-alive connection can stand idle between requests.
    pub keepalive_idle_timeout: Duration,

    /// Maximum size of HTTP request headers (bytes).
    /// Prevents memory exhaustion from oversized header attacks.
    pub max_header_size: usize,

    /// Maximum number of concurrent connections per source IP.
    /// Prevents a single source from monopolizing connection slots.
    pub max_connections_per_ip: u32,
}

impl Default for ProxyTimeouts {
    fn default() -> Self {
        ProxyTimeouts {
            request_header_read_timeout: Duration::from_secs(10),
            request_body_read_timeout: Duration::from_secs(30),
            keepalive_idle_timeout: Duration::from_secs(60),
            max_header_size: 8 * 1024,       // 8 KB
            max_connections_per_ip: 256,
        }
    }
}
```

---

## 6. Rate Limit Config in AppConfig

Rate limits are configured per-app via the deploy command and stored alongside the
existing `AppConfig` in redb.

```rust
// Extension to crates/common/src/types.rs (AppConfig)
use serde::{Deserialize, Serialize};

/// Rate limit configuration, stored as part of AppConfig.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppRateLimitConfig {
    /// Maximum sustained requests per second per node.
    pub requests_per_second: u32,

    /// Burst tolerance (number of requests above the sustained rate).
    pub burst_capacity: u32,

    /// Maximum requests per second from a single IP to this app.
    pub per_ip_limit: u32,
}

impl Default for AppRateLimitConfig {
    fn default() -> Self {
        AppRateLimitConfig {
            requests_per_second: 1_000,
            burst_capacity: 50,
            per_ip_limit: 100,
        }
    }
}

// AppConfig gains a new field:
// pub rate_limit: Option<AppRateLimitConfig>,
```

---

## 7. CLI Integration

```
wasm-ctl deploy \
  --app-name  api-users \
  --version   v2 \
  --wasm-file ./target/wasm32-wasip2/release/api-users.wasm \
  --rate-limit 5000 \
  --burst 200 \
  --per-ip-limit 50

wasm-ctl rate-limit set \
  --app api-users \
  --requests-per-second 5000 \
  --burst 200 \
  --per-ip 50

wasm-ctl rate-limit get --app api-users
# Output:
# api-users: 5000 req/s (burst: 200, per-ip: 50/s)
```

Rate limit changes take effect on the next request (no restart needed). The Supervisor
propagates changes via NATS `config.update.<app_id>` events, and each node updates its
local `RateLimiter` config.

---

## 8. Metrics for Rate Limiting

The rate limiter emits its own metrics, separate from Wasm execution metrics:

```rust
// crates/proxy/src/rate_limit_metrics.rs
use prometheus::{IntCounterVec, Opts, Registry};

pub struct RateLimitMetrics {
    /// Counter of rejected requests, labeled by app and reason.
    pub rejected_total: IntCounterVec,
}

impl RateLimitMetrics {
    pub fn new(registry: &Registry) -> Self {
        let rejected_total = IntCounterVec::new(
            Opts::new(
                "proxy_rate_limit_rejected_total",
                "Total requests rejected by rate limiting",
            ),
            &["app", "reason"],  // reason: "app_limit" | "ip_limit" | "backpressure"
        ).unwrap();
        registry.register(Box::new(rejected_total.clone())).unwrap();

        RateLimitMetrics { rejected_total }
    }
}
```

Prometheus queries for dashboards:

```promql
# Rejection rate per app (requests/second being turned away)
rate(proxy_rate_limit_rejected_total{reason="app_limit"}[5m])

# IP-based rejections (indicates abuse from specific sources)
rate(proxy_rate_limit_rejected_total{reason="ip_limit"}[5m])

# Backpressure events (node out of fuel)
rate(proxy_rate_limit_rejected_total{reason="backpressure"}[5m])
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Per-App Rate Limiting
- [ ] A deploy command with `--rate-limit 1000` stores the limit in AppConfig
- [ ] After 1000 requests in 1 second to an app, the 1001st returns HTTP 429
- [ ] Burst capacity allows a short spike beyond the sustained rate without rejection
- [ ] Changing rate limits via `wasm-ctl rate-limit set` takes effect within 5 seconds on all nodes

### Per-IP Rate Limiting
- [ ] A single IP sending more than `per_ip_limit` req/s receives HTTP 429
- [ ] Different IPs are tracked independently (IP A being limited does not affect IP B)
- [ ] Stale IP entries are pruned after 5 minutes of inactivity (no memory leak)

### Backpressure
- [ ] When the Supervisor's fuel headroom reaches 0, `BackpressureSignal` flips to rejecting
- [ ] All new requests receive HTTP 503 with a `Retry-After` header while backpressure is active
- [ ] When fuel headroom recovers, new requests are accepted again within 1 health tick (5s)

### Slowloris Defense
- [ ] A client that sends headers slower than 1 byte/second is disconnected after `request_header_read_timeout`
- [ ] A request with headers larger than `max_header_size` is rejected before body reading begins

### Metrics
- [ ] `proxy_rate_limit_rejected_total` increments for every rejected request
- [ ] Labels correctly distinguish app_limit, ip_limit, and backpressure rejections
- [ ] Grafana dashboard shows rejection rates per app and per reason

### Integration
- [ ] Rate limiting runs before upstream resolution (rejected requests never touch Supervisor)
- [ ] Under sustained load above the limit, latency for allowed requests is not affected
- [ ] Rate limit check adds < 1µs overhead per request (no I/O, no locks on the hot path)
