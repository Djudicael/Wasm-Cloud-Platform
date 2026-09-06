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

### Routing and transport

- Local upstream selection runs before overload steering. If a healthy local endpoint exists, the request stays local even when the node is above the remote-steering threshold.
- `HostRouter::resolve()` takes an asynchronous read lock on each request. This is correct but remains a potential contention point at very high route lookup rates.
- `ProxyTimeouts` records the intended header, body, keepalive, and connection limits, but Pingora's public configuration does not expose all corresponding transport controls. The server logs these values and otherwise uses Pingora's transport defaults.

### Request limits

- The proxy rejects a request when a valid `Content-Length` exceeds `max_body_size_bytes` (10 MiB in node startup). It does not currently count streamed bytes, so a chunked request without `Content-Length` is not bounded by this check.
- The local rate limiter prunes idle per-IP buckets every five minutes. Per-app buckets and success counters remain until application removal calls the cleanup path.
- `CircuitBreakerManager::prune_removed_apps()` exists, but lifecycle callers must invoke it with the active application set; the manager has no independent background pruning loop.

### Gateway lifecycle

- OIDC uses provider discovery by default and accepts an explicit private JWKS override. Issuer, audience, signature, expiry, and role/scope authorization are enforced in the request filter.
- API-key validation hashes the presented key with SHA-256 and compares the resulting map key. Keys therefore need high entropy; the current CLI does not expose an API-key creation or rotation lifecycle.
- Gateway route configuration is held in asynchronous locks to support live updates. Large, frequent configuration updates can briefly contend with request reads.

## Security Considerations

### Forwarded client addresses

`X-Forwarded-For` and `X-Real-IP` are used only when the direct peer belongs to `auth.trusted_proxies`. Otherwise the proxy uses the socket peer and ignores forwarded headers. Keep the trusted proxy CIDRs narrow.

### Bearer and API keys

Admin bearer tokens use `common::crypto::constant_time_eq`. API keys are stored and looked up as unsalted SHA-256 digests, so operators must generate random high-entropy keys and protect the storage database.

### Request bodies

The configured body limit currently relies on `Content-Length`. An upstream load balancer should also enforce a byte-counted body limit, especially for chunked requests.

### CORS

When credentials are enabled with a wildcard origin, the proxy echoes the validated request origin instead of returning the invalid `Access-Control-Allow-Origin: *` combination. Restrict `allowed_origins` rather than using a wildcard for sensitive routes.

### Internal identity

The proxy removes caller-supplied platform identity and trace headers before adding trusted values. Node-local `.internal` routing still depends on correct namespace-qualified application IDs and the supervisor's local placement contract.
