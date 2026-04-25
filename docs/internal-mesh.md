# Internal Service Mesh (East-West Communication)

This guide explains how applications communicate with each other **inside the platform** — the East-West traffic path. The key principle is **transparency**: the Wasm application writes normal HTTP code, and the platform handles routing, security, and policy enforcement automatically.

## Table of Contents

1. [The Transparency Principle](#the-transparency-principle)
2. [Namespaces](#namespaces)
3. [Virtual DNS](#virtual-dns)
4. [Network Interception](#network-interception)
5. [Internal HTTP Proxy](#internal-http-proxy)
6. [Cross-Namespace Rules](#cross-namespace-rules)
7. [Service Discovery](#service-discovery)
8. [Example: Two Apps Talking](#example-two-apps-talking)

---

## The Transparency Principle

**What the app writes:**

```rust
// Inside the Wasm app — completely normal HTTP client code
let client = reqwest::Client::new();
let resp = client
    .get("http://payment-service.internal/process")
    .header("X-Request-Id", "abc-123")
    .send()
    .await?;
```

**What the platform does:**

1. Virtual DNS resolves `payment-service.internal` → `127.0.0.1`
2. The Supervisor's `socket_addr_check` intercepts the outbound TCP connection
3. It identifies the caller app from the source port
4. It looks up `payment-service` in the caller's namespace
5. It checks rate limits and circuit breaker
6. It rewrites the destination to the target's real loopback port
7. The target app receives the request as if it came directly

**The app never sets `X-Target-App`, `X-Source-App`, or any platform-specific header.**

---

## Namespaces

Every app belongs to a **namespace**. Namespaces provide:

- **Blast-radius isolation** — a runaway app in `staging` cannot directly reach apps in `production`
- **Multi-tenancy boundaries** — `tenant-a` and `tenant-b` can both deploy `api-users` without collision
- **Policy enforcement boundaries** — cross-namespace traffic is blocked at the WASI layer

### Default namespace

Apps without an explicit namespace live in `default`:

```bash
wasm-ctl deploy --app api-users --version v1 --wasm ./api-users.wasm
# → Namespace: default
```

### Explicit namespace

```bash
wasm-ctl deploy \
  --app api-users \
  --version v1 \
  --namespace production \
  --wasm ./api-users.wasm

wasm-ctl deploy \
  --app api-users \
  --version v1 \
  --namespace tenant-a \
  --wasm ./api-users.wasm
```

These are **two different apps** with the same name, isolated from each other.

### Qualified App ID

Inside the platform, an app is uniquely identified as:

```
<namespace>/<name>:<version>

# Examples:
default/api-users:v1
production/api-users:v2
tenant-a/payments:v1
```

The namespace is **never** exposed to the Wasm app via environment variables. The app does not know which namespace it lives in.

---

## Virtual DNS

Each Wasm instance gets a **per-instance virtual DNS resolver** that answers queries for `*.internal` hostnames.

### How it works

When the Supervisor spawns an instance, it builds a `VirtualDns` for that instance containing all known services in the same namespace:

```rust
// Pseudo-code of what the Supervisor does
let mut dns = VirtualDns::new("production");
dns.register_service("payment-service");
dns.register_service("user-service");
dns.register_service("notification-service");
// All *.internal names resolve to 127.0.0.1
```

**Inside the Wasm app:**

```rust
// This DNS query is answered by the virtual resolver
let addrs = lookup("payment-service.internal");
// → [127.0.0.1]
```

The actual target port is determined at **connect time** by the network interceptor, not by DNS.

---

## Network Interception

The Supervisor configures a `socket_addr_check` callback on every Wasm instance's WASI context. This callback intercepts **all** outbound TCP connections.

### Same namespace → allowed

```
App A (payments:v1) in namespace "production"
  │
  │  TcpStream::connect("payment-service.internal:8080")
  │
  ▼
┌────────────────────────────────────────┐
│ NetworkInterceptor                     │
│  1. Caller = payments:v1 (from port)   │
│  2. Target = payment-service           │
│  3. Check: same namespace? ✅          │
│  4. Check: rate limit? ✅              │
│  5. Check: circuit breaker? ✅         │
│  6. Rewrite dest → 127.0.0.1:10101     │
└────────────────────────────────────────┘
  │
  ▼
App B (payment-service) on port 10101
```

### Cross namespace → blocked

```
App A (payments:v1) in namespace "staging"
  │
  │  TcpStream::connect("api-users.internal:8080")
  │
  ▼
┌────────────────────────────────────────┐
│ NetworkInterceptor                     │
│  1. Caller = payments:v1 → "staging"   │
│  2. Target = api-users → "production"  │
│  3. Check: same namespace? ❌          │
│  4. Return ECONNREFUSED                │
└────────────────────────────────────────┘
```

The Wasm module receives a standard `Connection refused` error and can handle it gracefully.

---

## Internal HTTP Proxy

For apps with **endpoint-level gateway rules** (per-path auth, per-path rate limits), the internal proxy applies those policies before forwarding.

### Architecture

```
App A calls "http://api-b.internal/users"
  │
  ▼
Virtual DNS resolves api-b.internal → 127.0.0.1:9080 (internal proxy)
  │
  ▼
socket_addr_check allows 127.0.0.1:9080
  │
  ▼
Internal Proxy (Axum on 127.0.0.1:9080)
  │  1. Reads Host header: "api-b.internal"
  │  2. Determines source app from source port
  │  3. Looks up endpoint rules for "/users"
  │  4. Applies auth (JWT / API key / none)
  │  5. Applies rate limit
  │  6. Applies request transforms
  │  7. Forwards to api-b's real port (127.0.0.1:10101)
  │
  ▼
App B receives the request
```

### When is the proxy used?

| Target app gateway config | Internal path |
|---------------------------|---------------|
| No endpoint rules | Direct connection (fast path) |
| Has endpoint rules | Via internal proxy (policy path) |

The app **does not know** which path was taken. Both are transparent.

---

## Cross-Namespace Rules

```
┌─────────────────────────────────────────────────────────────┐
│                      Namespace: production                  │
│  ┌──────────────┐         ┌──────────────┐                 │
│  │  payments:v1 │◄───────►│ api-users:v2 │   ← Direct     │
│  └──────────────┘         └──────────────┘     (allowed)   │
│                                                              │
│  payments calls api-users.internal → works                   │
└─────────────────────────────────────────────────────────────┘
                               │
                               │ Cross-namespace
                               │ blocked at WASI layer
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                      Namespace: staging                     │
│  ┌──────────────┐                                           │
│  │  payments:v1 │ ──► api-users.internal                    │
│  └──────────────┘     ❌ ECONNREFUSED                       │
│                                                              │
│  To reach production, use external hostname:                 │
│  https://api-users.production.example.com/                   │
│  (flows through Pingora with full TLS/auth/audit)           │
└─────────────────────────────────────────────────────────────┘
```

### Summary

| Scenario | Path | Policies applied |
|----------|------|------------------|
| Same namespace, no endpoint rules | Direct TCP | Rate limit, circuit breaker |
| Same namespace, endpoint rules | Internal proxy | Auth, rate limit, circuit breaker, transforms |
| Cross namespace | Blocked at WASI | — |
| Cross namespace (intentional) | External hostname through Pingora | TLS, OIDC, audit, distributed rate limit |

---

## Service Discovery

Apps can discover other services in their namespace automatically.

### Automatic env var injection

When the Supervisor spawns an instance, it injects service URLs as environment variables:

```bash
# If "payment-service:v1" is running in the same namespace:
PAYMENT_SERVICE_URL=http://127.0.0.1:10101
USER_SERVICE_URL=http://127.0.0.1:10102
NOTIFICATION_SERVICE_URL=http://127.0.0.1:10103
```

**Inside the Wasm app:**

```rust
let payment_url = std::env::var("PAYMENT_SERVICE_URL")
    .expect("PAYMENT_SERVICE_URL not set");

let client = reqwest::Client::new();
let resp = client
    .post(format!("{}/process", payment_url))
    .json(&payload)
    .send()
    .await?;
```

The app can also use `.internal` hostnames directly:

```rust
let resp = client
    .post("http://payment-service.internal/process")
    .json(&payload)
    .send()
    .await?;
```

Both approaches work. The `.internal` hostname is more portable (no env var dependency), while the env var is more explicit.

---

## Example: Two Apps Talking

Let's deploy two apps that communicate internally.

### App 1: Payment Service

```toml
# payment-service.toml
[app]
name = "payment-service"
version = "v1"
namespace = "production"
wasm_artifact = "./payment-service.wasm"

[fuel]
quota = 1_000_000_000
memory_pages = 2048
max_instances = 5

[policy]
profile = "http_api"

[gateway]
host = "payments.example.com"

[[gateway.endpoints]]
path = "/process"
methods = ["POST"]
auth = "api_key"
```

```bash
wasm-ctl deploy --manifest ./payment-service.toml
wasm-ctl routes add --host payments.example.com --app payment-service:v1
```

### App 2: Order Service (calls Payment Service)

```toml
# order-service.toml
[app]
name = "order-service"
version = "v1"
namespace = "production"
wasm_artifact = "./order-service.wasm"

[fuel]
quota = 500_000_000
memory_pages = 2048
max_instances = 10

[policy]
profile = "http_api"

[gateway]
host = "orders.example.com"

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/api/orders"
methods = ["GET", "POST"]
auth = "roles"
allowed_roles = ["user", "admin"]
```

```bash
wasm-ctl deploy --manifest ./order-service.toml
wasm-ctl routes add --host orders.example.com --app order-service:v1
```

### Inside order-service (Rust code)

```rust
use axum::{routing::{get, post}, Router, Json, extract::State};
use serde_json::Value;

#[derive(Clone)]
struct AppState {
    http: reqwest::Client,
}

async fn create_order(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<String, String> {
    // Call payment-service internally
    let payment_resp = state.http
        .post("http://payment-service.internal/process")
        .header("X-Api-Key", std::env::var("PAYMENT_API_KEY").unwrap())
        .json(&json!({
            "amount": payload["amount"],
            "currency": payload["currency"],
        }))
        .send()
        .await
        .map_err(|e| format!("Payment service error: {}", e))?;

    if !payment_resp.status().is_success() {
        return Err("Payment failed".to_string());
    }

    let payment_result: Value = payment_resp.json().await
        .map_err(|e| format!("Invalid payment response: {}", e))?;

    Ok(format!("Order created with payment: {}", payment_result["id"]))
}

#[tokio::main]
async fn main() {
    let state = AppState {
        http: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/api/orders", post(create_order))
        .with_state(state);

    let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Test the flow

```bash
# 1. Create an order (external request through Pingora)
curl -X POST https://orders.example.com/api/orders \
  -H "Authorization: Bearer <jwt-token>" \
  -H "Content-Type: application/json" \
  -d '{"amount": 100, "currency": "USD", "item": "widget"}'

# The order-service internally calls payment-service
# You never see payment-service.internal from the outside

# 2. Check both apps are running
wasm-ctl app list --namespace production
# → Apps in namespace 'production':
#   payment-service:v1 (5 instances)
#   order-service:v1 (10 instances)
```

### What happens under the hood

1. External client calls `POST https://orders.example.com/api/orders`
2. Pingora routes to `order-service:v1`
3. `order-service` spawns, receives the request
4. `order-service` calls `http://payment-service.internal/process`
5. Virtual DNS resolves to `127.0.0.1`
6. `socket_addr_check` identifies caller as `order-service`, target as `payment-service`
7. Both are in `production` namespace → allowed
8. `payment-service` endpoint rule says `auth = "api_key"`
9. The internal proxy validates the `X-Api-Key` header
10. Request reaches `payment-service`, which processes the payment
11. Response flows back through the proxy to `order-service`
12. `order-service` returns the final result to the external client

---

## Security Considerations

### What a malicious app cannot do

| Attack | Defense |
|--------|---------|
| Forge its namespace | Namespace is not exposed to the app |
| Bypass the proxy | All outbound TCP is intercepted by `socket_addr_check` |
| Scan arbitrary ports | Unknown loopback destinations are blocked |
| Reach cross-namespace apps | `ECONNREFUSED` at WASI layer |
| Forge `X-Source-App` | Injected by WASI host, not the app |
| Exhaust internal proxy | Rate limits and circuit breakers apply |

### Defense in depth

1. **WASI sandbox** — Wasm cannot access host system calls directly
2. **Network policy** — CIDR restrictions, connection limits, egress bytes
3. **Namespace isolation** — Cross-namespace traffic blocked at WASI layer
4. **Rate limiting** — Per-app token buckets
5. **Circuit breaker** — Per-app failure detection
6. **eBPF monitoring** — Kernel-level syscall anomaly detection (Linux)
