# Internal Service Mesh (East-West Communication)

This guide explains how applications communicate with each other **inside the platform** — the East-West traffic path. The key principle is **transparency**: the Wasm application writes normal HTTP code, and the platform handles routing, security, and policy enforcement automatically.

The platform is **self-sufficient**: it includes an embedded DNS stub so `<app>.<namespace>.internal` hostnames resolve without any external DNS server (CoreDNS) or `/etc/hosts` manipulation.

## Table of Contents

1. [The Transparency Principle](#the-transparency-principle)
2. [Namespaces](#namespaces)
3. [Embedded DNS Stub](#embedded-dns-stub)
4. [Service Discovery](#service-discovery)
5. [Network Interception](#network-interception)
6. [Internal HTTP Gateway](#internal-http-gateway)
7. [Cross-Namespace Rules](#cross-namespace-rules)
8. [Example: Two Apps Talking](#example-two-apps-talking)

---

## The Transparency Principle

**What the app writes:**

```rust
// Inside the Wasm app — completely normal HTTP client code
let client = reqwest::Client::new();
let resp = client
    .get("http://payment-service.production.internal:9080/process")
    .header("X-Request-Id", "abc-123")
    .send()
    .await?;
```

**What the platform does:**

1. The embedded DNS stub resolves `payment-service.production.internal` → `127.0.0.1`
2. The app connects to `127.0.0.1:9080` (the internal gateway)
3. The internal gateway reads the `Host: payment-service.production.internal:9080` header
4. It parses the target: namespace="production", app="payment-service"
5. It resolves the target in the NamespaceRegistry
6. It applies endpoint-level policies (auth, rate limit, circuit breaker)
7. It forwards to the target's real loopback port
8. The target app receives the request as if it came directly

**The app never sets `X-Target-App`, `X-Source-App`, or any platform-specific header.**

---

## Namespaces

Every app belongs to a **namespace**. Namespaces provide:

- **Blast-radius isolation** — a runaway app in `staging` cannot directly reach apps in `production`
- **Multi-tenancy boundaries** — `tenant-a` and `tenant-b` can both deploy `api-users` without collision
- **Policy enforcement boundaries** — cross-namespace traffic is isolated by service discovery (apps only learn about same-namespace services)

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

## Embedded DNS Stub

The platform includes a lightweight UDP DNS server that runs inside every node. It resolves `<app>.<namespace>.internal` hostnames to `127.0.0.1` so apps can use normal URLs without installing external DNS software.

### How it works

```
App calls getaddrinfo("payment-service.production.internal")
  │
  ▼
┌────────────────────────────────────────┐
│  Host OS resolver                      │
│  (reads /etc/resolv.conf)              │
│                                        │
│  nameserver 127.0.0.1:15353  ←───┐     │
│  nameserver 8.8.8.8              │     │
└────────────────────────────────────────┘
                                   │
                                   ▼
                         ┌─────────────────┐
                         │  Embedded DNS   │
                         │  Stub (node)    │
                         │                 │
                         │  *.*.internal → │
                         │  127.0.0.1      │
                         └─────────────────┘
```

The node **provides** the DNS service. The operator **configures** the system to use it. The node never modifies system files at runtime.

### Production setup

Choose one of the following based on your environment:

#### Option A: systemd-resolved (recommended for bare-metal and VMs)

```bash
# One-time configuration during node installation
sudo mkdir -p /etc/systemd/resolved.conf.d/
sudo tee /etc/systemd/resolved.conf.d/wasm-platform.conf << 'EOF'
[Resolve]
DNS=127.0.0.1:15353
Domains=~internal
FallbackDNS=8.8.8.8
EOF
sudo systemctl restart systemd-resolved
```

This forwards all `*.*.internal` queries to the embedded stub and everything else to the upstream DNS. The node runs unprivileged.

#### Option B: Bind to port 53 (no existing resolver)

If the host has no local resolver (e.g., a minimal server image), grant the node binary permission to bind to port 53:

```bash
# One-time installation step
sudo setcap cap_net_bind_service=+ep /usr/bin/wasm-node

# Node config
[dns]
stub_enabled = true
stub_port = 53
```

Then configure `/etc/resolv.conf` to use `nameserver 127.0.0.1` as part of your base image or install script.

### Configuration

```toml
# config/dev.toml
[dns]
stub_enabled = true      # default: true
stub_port = 15353        # default: 15353
```

Set `stub_enabled = false` if you prefer to use an external DNS server or `/etc/hosts` instead.

### DNS behavior

| Query | Response |
|-------|----------|
| `A` record for `*.*.internal` | `127.0.0.1` |
| Any other query | `NXDOMAIN` (falls through to next nameserver) |

### Testing without DNS setup

In test environments where DNS is not configured, add entries to `/etc/hosts`:

```bash
127.0.0.1 echo-service.production.internal
127.0.0.1 payment-service.production.internal
```

The e2e test suite does this automatically via `ensure_hosts_entry()`.

---

## Service Discovery

Apps can discover other services in their namespace automatically.

### Automatic env var injection

When the Supervisor spawns an instance, it injects service URLs as environment variables:

```bash
# If "payment-service:v1" is running in the "production" namespace:
#   - If payment-service has endpoint rules:
PAYMENT_SERVICE_URL=http://payment-service.production.internal:9080

#   - If payment-service has NO endpoint rules (fast path):
PAYMENT_SERVICE_URL=http://127.0.0.1:10101
```

The Supervisor uses the **slow path** (`<app>.<namespace>.internal:9080`) when the target app has endpoint-level gateway rules, so that auth, rate limits, and circuit breakers are enforced on East-West traffic. It uses the **fast path** (direct loopback) when there are no endpoint rules.

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

The app can also use `<app>.<namespace>.internal` hostnames directly:

```rust
let resp = client
    .post("http://payment-service.production.internal:9080/process")
    .json(&payload)
    .send()
    .await?;
```

Both approaches work. The `<app>.<namespace>.internal` hostname is more portable (no env var dependency), while the env var is more explicit.

---

## Network Interception

The Supervisor configures a `socket_addr_check` callback on every Wasm instance's WASI context. This callback intercepts **all** outbound TCP connections.

### What is allowed

| Destination | Result |
|-------------|--------|
| Known app in same namespace (loopback port) | ✅ Allowed |
| Internal gateway `127.0.0.1:9080` | ✅ Allowed |
| Unknown loopback port | ❌ Blocked |
| External (non-loopback) | ✅ Allowed (separate CIDR policy applies) |

### Same namespace → allowed

```
App A (payments:v1) in namespace "production"
  │
  │  TcpStream::connect("payment-service.production.internal:9080")
  │
  ▼
┌────────────────────────────────────────┐
│ socket_addr_check                      │
│  1. Dest = 127.0.0.1:9080              │
│  2. Port 9080 is the internal gateway  │
│  3. ✅ Allow                           │
└────────────────────────────────────────┘
  │
  ▼
Internal Gateway (127.0.0.1:9080)
  │  1. Host header: "payment-service.production.internal:9080"
  │  2. Parses target: namespace="production", app="payment-service"
  │  3. Resolves in NamespaceRegistry → 127.0.0.1:10101
  │  4. Applies endpoint policies
  │  5. Forwards to 127.0.0.1:10101
  │
  ▼
App B (payment-service) on port 10101
```

### Cross namespace → direct ports blocked; gateway port open

**Direct app ports are namespace-enforced.** If an app somehow learns the real loopback port of an app in another namespace and connects directly, the `socket_addr_check` blocks it:

```
App A (staging) → 127.0.0.1:10101 (production app)
  │
  ▼
socket_addr_check
  1. Port 10101 belongs to "production/api-users:v1"
  2. Caller is in "staging"
  3. ❌ Cross-namespace — connection refused
```

**The internal gateway port (9080) is currently open to all namespaces.** The `socket_addr_check` explicitly allows connections to port 9080 regardless of the caller's namespace:

```
App A (payments:v1) in namespace "staging"
  │
  │  TcpStream::connect("api-users.production.internal:9080")
  │
  ▼
┌────────────────────────────────────────┐
│ socket_addr_check                      │
│  1. Dest = 127.0.0.1:9080              │
│  2. Port 9080 is the internal gateway  │
│  3. ✅ Allowed (namespace not checked) │
└────────────────────────────────────────┘
  │
  ▼
Internal Gateway (127.0.0.1:9080)
  │  ⚠️ Gateway does not enforce namespace boundaries
  │  Request is forwarded to the target app
  │
  ▼
App B (production/api-users) on port 10101
```

The primary namespace boundary is **service discovery isolation**: the Supervisor only injects `<APP>_SERVICE_URL` environment variables for apps in the same namespace. An app that does not know the hostname of a cross-namespace service cannot reach it through the gateway. However, a malicious app that discovers or guesses a cross-namespace hostname can currently reach it via the gateway port.

For intentional cross-namespace communication, use the external hostname (`https://api-users.production.example.com/`), which flows through the Pingora gateway with full TLS/auth/audit.

---

## Internal HTTP Gateway

For apps with **endpoint-level gateway rules** (per-path auth, per-path rate limits), the internal gateway applies those policies before forwarding.

### Architecture

```
App A calls "http://api-b.production.internal:9080/users"
  │
  ▼
Embedded DNS resolves api-b.production.internal → 127.0.0.1
  │
  ▼
App connects to 127.0.0.1:9080 (internal gateway)
  │
  ▼
socket_addr_check allows 127.0.0.1:9080
  │
  ▼
Internal Gateway (Axum on 127.0.0.1:9080)
  │  1. Reads Host header: "api-b.production.internal:9080"
  │  2. Parses target: namespace="production", app="api-b"
  │  3. Resolves target in NamespaceRegistry
  │  4. Looks up endpoint rules for "/users"
  │  5. Applies auth (JWT / API key / none)
  │  6. Applies rate limit
  │  7. Forwards to api-b's real port (127.0.0.1:10101)
  │
  ▼
App B receives the request
```

### When is the gateway used?

| Target app gateway config | Internal path |
|---------------------------|---------------|
| No endpoint rules | Direct connection (fast path) |
| Has endpoint rules | Via internal gateway on port 9080 |

The app **does not know** which path was taken. Both are transparent.

### Limitations

- The internal gateway does not enforce namespace boundaries. The `socket_addr_check` blocks cross-namespace connections to direct app ports, but the gateway port (9080) is currently open to all namespaces. Namespace isolation relies on service discovery: the Supervisor only injects service URLs for same-namespace apps.
- **Endpoint-level rate limiting** is structural but uses simplified checks; full distributed rate limiting is applied at the external Pingora gateway.

---

## Cross-Namespace Rules

```
┌─────────────────────────────────────────────────────────────┐
│                      Namespace: production                  │
│  ┌──────────────┐         ┌──────────────┐                 │
│  │  payments:v1 │◄───────►│ api-users:v2 │   ← Direct     │
│  └──────────────┘         └──────────────┘     (allowed)   │
│                                                              │
│  payments calls api-users.production.internal → works        │
└─────────────────────────────────────────────────────────────┘
                                │
                                │ Cross-namespace
                                │ direct ports blocked;
                                │ gateway port open
                                ▼
┌─────────────────────────────────────────────────────────────┐
│                      Namespace: staging                     │
│  ┌──────────────┐                                           │
│  │  payments:v1 │ ──► 127.0.0.1:10101 (direct port)        │
│  └──────────────┘     ❌ Connection refused (WASI layer)    │
│                                                              │
│  ┌──────────────┐                                           │
│  │  payments:v1 │ ──► api-users.production.internal:9080    │
│  └──────────────┘     ⚠️ Allowed (gateway port open)       │
│       (but hostname not injected — service discovery        │
│        only provides URLs for same-namespace apps)          │
│                                                              │
│  To reach production intentionally, use external hostname:  │
│  https://api-users.production.example.com/                   │
│  (flows through Pingora with full TLS/auth/audit)           │
└─────────────────────────────────────────────────────────────┘
```

### Summary

| Scenario | Path | Policies applied |
|----------|------|------------------|
| Same namespace, no endpoint rules | Direct TCP | Rate limit, circuit breaker (at external gateway for ingress) |
| Same namespace, endpoint rules | Internal gateway | Auth, rate limit, circuit breaker, transforms |
| Cross namespace (direct port) | Blocked at WASI layer | — |
| Cross namespace (gateway port 9080) | Not enforced | Service discovery isolation only |
| Cross namespace (intentional) | External hostname through Pingora | TLS, OIDC, audit, distributed rate limit |

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

[[gateway.endpoints]]
path = "/process"
methods = ["POST"]
auth = "api_key"
```

```bash
wasm-ctl deploy --manifest ./payment-service.toml
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
    // Call payment-service internally using the injected env var
    let payment_url = std::env::var("PAYMENT_SERVICE_URL")
        .unwrap_or_else(|_| "http://payment-service.production.internal:9080".to_string());

    let payment_resp = state.http
        .post(format!("{}/process", payment_url))
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
# You never see payment-service.production.internal from the outside

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
4. `order-service` reads `PAYMENT_SERVICE_URL=http://payment-service.production.internal:9080` (injected by Supervisor)
5. `order-service` calls `http://payment-service.production.internal:9080/process`
6. Embedded DNS resolves `payment-service.production.internal` → `127.0.0.1`
7. `socket_addr_check` allows `127.0.0.1:9080` (internal gateway port)
8. Internal gateway reads `Host: payment-service.production.internal:9080`
9. Parses target: namespace="production", app="payment-service"
10. Resolves in NamespaceRegistry → `127.0.0.1:10101`
11. Endpoint rule says `auth = "api_key"` — validates `X-Api-Key` header
12. Request reaches `payment-service`, which processes the payment
13. Response flows back through the gateway to `order-service`
14. `order-service` returns the final result to the external client

---

## Security Considerations

### What a malicious app cannot do

| Attack | Defense |
|--------|---------|
| Forge its namespace | Namespace is not exposed to the app |
| Bypass the proxy | All outbound TCP is intercepted by `socket_addr_check` |
| Scan arbitrary ports | Unknown loopback destinations are blocked |
| Reach cross-namespace apps (direct port) | `ECONNREFUSED` at WASI layer — direct app ports are namespace-checked |
| Reach cross-namespace apps (gateway port) | ⚠️ Not currently blocked — gateway port 9080 is open to all namespaces; relies on service discovery isolation |
| Exhaust internal gateway | Rate limits and circuit breakers apply |

### Known limitations

- The internal gateway does not enforce namespace boundaries. The `socket_addr_check` blocks cross-namespace connections to direct app ports, but the gateway port (9080) is currently open to all namespaces. Namespace isolation relies on service discovery: the Supervisor only injects `<APP>_SERVICE_URL` for same-namespace apps. A malicious app that discovers a cross-namespace hostname can currently reach it through the gateway.

### Defense in depth

1. **WASI sandbox** — Wasm cannot access host system calls directly
2. **Network policy** — CIDR restrictions, connection limits, egress bytes
3. **Namespace isolation** — Cross-namespace direct connections blocked at WASI layer; gateway port (9080) relies on service discovery isolation
4. **Rate limiting** — Per-app token buckets
5. **Circuit breaker** — Per-app failure detection
6. **eBPF monitoring** — Kernel-level syscall anomaly detection (Linux)
