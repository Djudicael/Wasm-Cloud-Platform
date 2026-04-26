# proxy

A reverse proxy / API gateway built on [Pingora](https://github.com/cloudflare/pingora), responsible for routing incoming HTTP/HTTPS requests to upstream Wasm instances.

## Overview

The proxy crate serves as the front-door to the Wasm Cloud Platform. It handles all inbound traffic and provides a full-featured API gateway with:

- **Host + path routing** — route requests to the correct upstream based on `Host` header and URL path
- **Round-robin upstream selection** — distribute requests across healthy instances
- **Cold-start support** — trigger instance spawn on first request to an idle app
- **Cross-node load balancing** — forward requests to less-loaded nodes when the local node is saturated
- **Rate limiting** — local token-bucket rate limiter plus a distributed rate limiter for cluster-wide limits
- **Authentication** — OIDC/JWT validation via `OidcProvider`, API key validation via `ApiKeyValidator`, and bearer token checks
- **CORS** — configurable Cross-Origin Resource Sharing headers
- **Circuit breaker** — per-upstream circuit breaking to prevent cascading failures
- **Request transforms** — header injection/modification before forwarding
- **Health probes** — periodic upstream health checking
- **DNS webhooks** — notify external DNS providers of instance changes
- **Admin API** — internal management endpoints with their own rate limiting
- **Prometheus metrics** — request counters, latency histograms, error rates
- **TLS** — TLS termination for HTTPS listeners

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      ProxyServer                        │
│  (Pingora server bootstrap, TLS config, listener setup) │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│                      WasmProxy                          │
│  (Pingora ProxyHttp trait impl — request lifecycle)     │
│                                                         │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐  │
│  │ HostRouter  │  │  Gateway     │  │ RateLimiter   │  │
│  │ (routing)   │  │  (auth/CORS) │  │ (local)       │  │
│  └─────────────┘  └──────────────┘  └───────────────┘  │
│                                                         │
│  ┌──────────────────────┐  ┌────────────────────────┐  │
│  │ UpstreamRegistry     │  │ CircuitBreakerManager  │  │
│  │ (upstream selection) │  │ (fault tolerance)      │  │
│  └──────────────────────┘  └────────────────────────┘  │
│                                                         │
│  ┌──────────────────────┐  ┌────────────────────────┐  │
│  │ NodeLoadTable        │  │ DistributedRateLimiter │  │
│  │ (cross-node routing) │  │ (cluster-wide limits)  │  │
│  └──────────────────────┘  └────────────────────────┘  │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────┐
│                   Upstream Wasm Instance                │
└─────────────────────────────────────────────────────────┘
```

### Request Flow

1. **Incoming request** arrives at `WasmProxy::request_filter()`
2. **HostRouter::resolve()** matches the `Host` header to a configured route
3. **Gateway::authenticate()** enforces the route's auth policy (None / ApiKey / Oidc)
4. **RateLimiter::check_request()** enforces per-route rate limits
5. **CircuitBreakerManager** checks if the upstream circuit is open
6. **UpstreamRegistry** selects a healthy upstream via round-robin
7. If no local upstream is available or the node is overloaded, **NodeLoadTable** selects a remote node
8. Request is forwarded to the upstream; response is streamed back to the client
9. Metrics are recorded and the circuit breaker state is updated based on the response

## Public API

### Core Types

| Type | Description |
|------|-------------|
| `ProxyServer` | Top-level Pingora server wrapper. Handles configuration, TLS setup, and server lifecycle. |
| `WasmProxy` | Implements Pingora's `ProxyHttp` trait. Orchestrates the full request lifecycle including routing, auth, rate limiting, and upstream selection. |
| `Gateway` | Handles authentication (OIDC, API key, bearer token) and CORS policy enforcement. |
| `HostRouter` | Resolves incoming requests to upstream configurations based on `Host` header and path. |
| `UpstreamRegistry` | Manages the set of upstream Wasm instances for each route. Provides round-robin selection of healthy backends. |
| `NodeLoadTable` | Tracks load metrics across cluster nodes for cross-node load balancing decisions. |
| `RateLimiter` | Local token-bucket rate limiter for per-route and per-IP request throttling. |
| `DistributedRateLimiter` | Cluster-wide rate limiting backed by a shared store. |
| `CircuitBreakerManager` | Per-upstream circuit breaker state machine (closed → open → half-open → closed). |
| `OidcProvider` | OpenID Connect / JWT validation. Fetches JWKS from the identity provider and verifies tokens. |
| `ApiKeyValidator` | Validates API keys by comparing SHA-256 hashes against stored values. |
| `BackpressureSignal` | Communicates upstream overload state back to the proxy layer. |
| `HealthState` | Tracks upstream health check results. |
| `DnsWebhookManager` | Calls external DNS webhooks when instances are added or removed. |
| `AuthState` | Shared authentication state including OIDC configuration and API key store. |

### Configuration

| Type | Description |
|------|-------------|
| `ProxyTimeouts` | Configurable timeouts for connect, read, and write operations. |

## Known Issues & Improvements

### Dead Code / Unfinished Features

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `node_is_overloaded()` always returns `false` | Cross-node load balancing is non-functional; all traffic stays on the local node regardless of load | Implement actual load-based overload detection using `NodeLoadTable` metrics |
| `UpstreamRegistry::next_healthy()` and `UpstreamHealthChecker` never wired in | Health checking exists in code but doesn't influence upstream selection | Connect the health checker to the upstream selection pipeline |
| Gateway metrics use a throwaway `Registry` | Prometheus scrape endpoint returns no gateway metrics | Use the shared Prometheus registry instead of creating a local one |
| `ProxyTimeouts` logged but not applied to Pingora | Configured timeouts have no effect; requests can hang indefinitely | Pass timeout values to Pingora's `ProxySession` configuration |

### Correctness Bugs

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `partial_cmp().unwrap()` on floats can panic on NaN | If a float metric is NaN, the proxy will panic and crash | Handle the `None` case from `partial_cmp()` with a sensible default |
| `Gateway::authenticate()` returns `Err` for `AuthPolicy::None` | Routes with no auth policy return an authentication error | Return `Ok(())` immediately when `AuthPolicy::None` is encountered |
| CORS wildcard + credentials is invalid per spec | Browsers reject `Access-Control-Allow-Origin: *` when `Access-Control-Allow-Credentials: true` | Echo the requesting origin instead of using `*` when credentials are allowed |
| No request body size limits | A malicious client can send an arbitrarily large request body, exhausting memory | Add configurable max body size limits in the proxy pipeline |

### Performance Issues

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `HostRouter::resolve()` acquires `RwLock` on every request | Contention under high concurrency reduces throughput | Replace `RwLock` with `ArcSwap` for lock-free reads |
| `RateLimiter::check_request()` clones config on every request | Unnecessary allocation on the hot path | Store config in `Arc` and clone only the `Arc` pointer |
| No pruning of `AdminRateLimiter` buckets | Memory grows without bound as new client IPs are seen | Add periodic cleanup of stale rate limiter entries |
| `CircuitBreakerManager` never prunes stale circuits | Circuits for removed routes persist indefinitely | Add a pruning mechanism triggered on route removal or periodically |

### Configuration Hardcoding

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `OidcProvider` JWKS URL hardcoded to Keycloak path | Only works with Keycloak at a specific path; breaks with other OIDC providers | Use OIDC discovery endpoint (`/.well-known/openid-configuration`) to fetch the JWKS URI dynamically |

## Security Considerations

### Timing Attack on Bearer Token Comparison

Bearer token comparison uses `==`, which short-circuits on the first differing byte. An attacker can measure response times to progressively guess the token one character at a time.

**Mitigation:** Use a constant-time comparison function such as `subtle::ConstantTimeEq` or `ring::constant_time::verify_slices_are_equal`.

### IP Spoofing via X-Forwarded-For

The `X-Forwarded-For` header is trusted without validation for rate limiting. A malicious client can spoof this header to bypass per-IP rate limits by rotating fake IP addresses.

**Mitigation:** Only trust `X-Forwarded-For` from known, trusted reverse proxies (validate against a allowlist of proxy IPs), or use the direct socket address for rate limiting when no trusted proxy is in front.

### Unsalted API Key Hashing

API keys are hashed with SHA-256 without a salt. While SHA-256 is fast, the lack of salt means an attacker with access to the hash store can use precomputed rainbow tables to recover common API keys.

**Mitigation:** Use a keyed HMAC (e.g., HMAC-SHA256 with a server-side secret) or a slow hash function like Argon2id for API key storage.

### No Request Body Size Limits

The proxy pipeline does not enforce maximum request body sizes. A malicious client can upload arbitrarily large payloads, potentially causing out-of-memory conditions on the proxy or upstream instances.

**Mitigation:** Add configurable `max_request_body_size` and reject requests exceeding the limit with `413 Payload Too Large` before forwarding.

### CORS Misconfiguration

The current CORS implementation sets `Access-Control-Allow-Origin: *` alongside `Access-Control-Allow-Credentials: true`. This combination is explicitly forbidden by the CORS specification and browsers will reject it, meaning credentialed cross-origin requests will fail silently.

**Mitigation:** When credentials are allowed, echo the specific requesting `Origin` header value instead of `*`, and validate it against an allowlist.
