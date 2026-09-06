# Internal Service Mesh (East-West Communication)

This guide explains how applications communicate with each other **inside the platform** — the East-West traffic path. The key principle is **transparency**: the Wasm application writes normal HTTP code, and the platform handles routing, security, and policy enforcement automatically.

The mesh is intentionally **node-local**. Every production node runs the same
dependency closure, resolves `<app>.<namespace>.internal` to its own loopback
gateway, and never searches another node when a local dependency is absent.
Cross-host workload-mesh identity is explicitly out of scope, not a missing
platform feature. Public ingress, control-plane replication, and explicitly
configured external service URLs may still cross hosts.

## Table of Contents

1. [The Transparency Principle](#the-transparency-principle)
2. [Namespaces](#namespaces)
3. [Embedded DNS Stub](#embedded-dns-stub)
4. [Placement and Local Dependencies](#placement-and-local-dependencies)
5. [Service Discovery](#service-discovery)
6. [Network Interception](#network-interception)
7. [Internal HTTP Gateway](#internal-http-gateway)
8. [Cross-Namespace Rules](#cross-namespace-rules)
9. [Example: Two Apps Talking](#example-two-apps-talking)

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
│  nameserver 127.0.0.1         ←───┐     │
│  nameserver <operator DNS>        │     │
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

The node **provides** the DNS service. The production image or node installer
**configures** the system to use it; the running node process does not rewrite
resolver files. The repository microVM image binds the stub to UDP port 53 and
places `nameserver 127.0.0.1` first in `/etc/resolv.conf`.

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
| Any other query | `SERVFAIL` (allows the resolver to try the operator nameserver) |

### Installation gate

Do not accept a loopback URL plus a manually supplied `Host` header as
production DNS evidence. From every signed node image, a WASI component must
resolve and call the literal URL:

```bash
http://payment-service.production.internal:9080/health
```

The 2026-08-30 three-node Firecracker rehearsal passed this exact path.

---

## Placement and Local Dependencies

The only supported internal-mesh placement policy is currently `every_node`.
Declare every service that must exist beside the caller:

```toml
[placement]
policy = "every_node"
local_dependencies = ["production/payment-service:v1"]
```

The dependency IDs must be fully qualified, unique, and in the caller's
namespace. Deploy dependencies before dependants. Each node validates that the
dependency configuration and artifact are present locally before admitting the
caller or cold-starting it.

If a required local dependency is removed or cannot start:

- the caller remains deployed, but its call returns HTTP 502;
- retained grace-period artifacts cannot resurrect the removed dependency;
- the node does not query the cluster or forward the call to another host;
- redeploying the dependency on every node restores local service.

This fail-local behavior preserves the shared-nothing data plane. Operators
must alert on the 502/dependency condition and choose whether the public route
should remain available, degrade, or be withdrawn for that application.

---

## Service Discovery

Apps can discover other services in their namespace automatically.

### Automatic env var injection

When the Supervisor spawns an instance, it injects service URLs as environment variables:

```bash
# If "payment-service:v1" is running in the "production" namespace:
#   - If payment-service has endpoint rules:
PAYMENT_SERVICE_SERVICE_URL=http://payment-service.production.internal:9080

#   - If payment-service has NO endpoint rules (fast path):
PAYMENT_SERVICE_SERVICE_URL=http://127.0.0.1:10101
```

The Supervisor uses the **slow path** (`<app>.<namespace>.internal:9080`) when the target app has endpoint-level gateway rules, so that auth, role/scope checks, API-key checks, rate limits, and circuit breakers are enforced on East-West traffic. It uses the **fast path** (direct loopback) when there are no endpoint rules.

For `wasi:http` / gRPC services on the slow path, the internal gateway now preserves the upstream `h2c` path instead of collapsing requests through a buffered HTTP/1 forwarding layer.

**Inside the Wasm app:**

```rust
let payment_url = std::env::var("PAYMENT_SERVICE_SERVICE_URL")
    .expect("PAYMENT_SERVICE_SERVICE_URL not set");

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

### Cross namespace → denied by default

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

The socket policy permits a connection to port 9080 so the internal gateway can
make the authorization decision. This is not an authorization bypass. The eBPF
namespace enforcer observes the established TCP connection, binds its ephemeral
source port to the registered workload TID, and the gateway resolves the caller
identity from that kernel-derived binding. Caller-supplied `X-Namespace`,
`X-Source-App`, and `X-Source-Tid` headers are stripped.

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
│  3. ✅ Pass to the policy gateway      │
└────────────────────────────────────────┘
  │
  ▼
Internal Gateway (127.0.0.1:9080)
  │  1. Resolves source port → registered workload identity
  │  2. Compares caller and target namespaces
  │  3. ❌ Denies unless an explicit cross-namespace rule allows it
```

Identity publication through the eBPF ring buffer is asynchronous. The gateway
waits for at most 50 ms for the source-port binding and then returns 401 if the
identity is still unresolved. It never falls back to a caller-provided identity.
When eBPF enforcement is unavailable, required enforcement also fails closed.

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

- The internal gateway is node-local by design. There is no cross-host
  `.internal` discovery, forwarding fallback, or workload-mesh identity.
- A separate application protocol may use an explicit external URL through the
  public gateway. That is not part of the internal mesh.
- Namespace enforcement depends on active eBPF identity attribution. Mandatory
  enforcement fails closed when the program, map, consumer, or binding is absent.
- Active outbound-connection accounting relies on eBPF TCP-close events. If
  `max_outbound_connections` is an enforcement control, production must set
  eBPF as required rather than accepting degraded monitoring.
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
                                │ gateway policy enforced
                                ▼
┌─────────────────────────────────────────────────────────────┐
│                      Namespace: staging                     │
│  ┌──────────────┐                                           │
│  │  payments:v1 │ ──► 127.0.0.1:10101 (direct port)        │
│  └──────────────┘     ❌ Connection refused (WASI layer)    │
│                                                              │
│  ┌──────────────┐                                           │
│  │  payments:v1 │ ──► api-users.production.internal:9080    │
│  └──────────────┘     ❌ Denied by kernel-derived identity │
│       unless an explicit cross-namespace allow rule exists  │
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
| Cross namespace (gateway port 9080) | Denied by default | eBPF workload identity and gateway allowlist |
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

[policy.network]
allow_inbound = true

[placement]
policy = "every_node"

[[gateway.endpoints]]
path = "/process"
methods = ["POST"]
auth = "none"
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

[policy.network]
allow_inbound = true
allow_outbound_tcp = true
allow_dns = true

[placement]
policy = "every_node"
local_dependencies = ["production/payment-service:v1"]

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
wasm-ctl routes add --host orders.example.com --app production/order-service:v1
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
    let payment_url = std::env::var("PAYMENT_SERVICE_SERVICE_URL")
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
4. `order-service` reads `PAYMENT_SERVICE_SERVICE_URL=http://payment-service.production.internal:9080` (injected by Supervisor)
5. `order-service` calls `http://payment-service.production.internal:9080/process`
6. Embedded DNS resolves `payment-service.production.internal` → `127.0.0.1`
7. `socket_addr_check` allows `127.0.0.1:9080` (internal gateway port)
8. Internal gateway reads `Host: payment-service.production.internal:9080`
9. Parses target: namespace="production", app="payment-service"
10. Resolves in NamespaceRegistry → `127.0.0.1:10101`
11. Endpoint rule says `auth = "none"`; the gateway has already enforced the caller's workload namespace identity
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
| Reach cross-namespace apps (gateway port) | Kernel-derived caller identity is resolved and cross-namespace access is denied unless explicitly allowed |
| Exhaust internal gateway | Rate limits and circuit breakers apply |

### Known limitations

- Enforcement is node-local and requires active eBPF source-port/TID attribution.
  The gateway fails closed when attribution is unavailable.
- Cross-host mesh identity is intentionally out of scope. Applications requiring
  remote traffic must use an explicit external route and its TLS/auth policy.

### Defense in depth

1. **WASI sandbox** — Wasm cannot access host system calls directly
2. **Network policy** — CIDR restrictions, connection limits, egress bytes
3. **Namespace isolation** — Direct ports are checked by the WASI policy; port
   9080 uses kernel-derived workload identity and deny-by-default gateway policy
4. **Rate limiting** — Per-app token buckets
5. **Circuit breaker** — Per-app failure detection
6. **eBPF monitoring** — Kernel-level syscall anomaly detection (Linux)
