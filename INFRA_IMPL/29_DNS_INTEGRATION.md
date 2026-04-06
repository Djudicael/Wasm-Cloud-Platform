# Step 29 — DNS Integration & Service Discovery

## Goal
Define how external DNS records are managed for apps running on the platform. The system must:
- Document the DNS prerequisites operators must set up before deploying apps
- Provide a webhook/API for automatic DNS record management (optional)
- Handle wildcard domains for multi-tenant platforms
- Support both dedicated domains and platform subdomains
- Integrate with the route management system (step 15)

---

## Context & Rationale

### The Problem This Solves

Step 15 (Route Management) maps HTTP `Host` headers to apps:
```
HostRouter["api.myapp.com"] → AppId("api-users:v2")
```

But before `api.myapp.com` reaches Pingora, the DNS query `api.myapp.com` must resolve
to the Pingora node's IP address. Without correct DNS, the browser sends the request to
the wrong server (or fails with `NXDOMAIN`).

This is the gap: the platform manages routing **after** the request arrives, but DNS
determines **where** the request is sent in the first place.

### Why DNS Is Not Managed by the Platform

DNS management is intentionally external to the platform for three reasons:

1. **TTL propagation**: DNS changes take time to propagate (TTL-dependent, often 60s–3600s).
   An incorrect DNS update cannot be undone quickly. The blast radius of a bug in platform
   DNS management is much larger than a bug in route management.

2. **Provider diversity**: Operators use Cloudflare, Route 53, Google Cloud DNS, or their
   own BIND servers. Supporting every DNS provider as a first-class integration would
   multiply the platform's complexity without improving the core value proposition.

3. **Security boundary**: DNS credentials (API tokens for Cloudflare, IAM credentials for
   Route 53) are some of the most sensitive credentials an operator holds. A DNS API token
   can redirect all traffic for a domain. Storing these in the platform increases the attack
   surface.

Instead, the platform provides:
- **Documentation** of the exact DNS records operators must create
- **An optional webhook** that fires when routes are added/removed, allowing operators to
  plug in their own DNS automation
- **Health-check endpoints** that DNS-level load balancers (Cloudflare, Route 53) can use

### The Two DNS Models

**Model A: Platform Subdomain (Simple)**

The operator owns `*.platform.example.com` and creates a wildcard DNS record pointing
to the Pingora nodes. Every app gets a subdomain automatically:

```
DNS: *.platform.example.com → <Pingora IP(s)>

Deploy: wasm-ctl deploy --app api-users --host api-users.platform.example.com
Result: api-users.platform.example.com routes to api-users app
```

This is the zero-configuration model. New apps are reachable immediately without any
DNS changes (the wildcard record was set up once during initial platform setup).

**Model B: Custom Domain (Production)**

The tenant owns `api.myapp.com` and creates a CNAME or A record pointing to Pingora:

```
DNS: api.myapp.com → CNAME platform.example.com
         (or)
     api.myapp.com → A <Pingora IP>

Deploy: wasm-ctl deploy --app api-users --host api.myapp.com
Result: api.myapp.com routes to api-users app
```

This requires the tenant to configure DNS themselves. The platform cannot do this
automatically because it doesn't have access to the tenant's DNS provider.

### Multi-Node DNS: Load Balancer or Round-Robin

With multiple Pingora nodes, the DNS record must resolve to **all** node IPs. There are
two approaches:

```
Approach 1: DNS Round-Robin
  api.myapp.com → A 10.0.0.1   (node-0)
  api.myapp.com → A 10.0.0.2   (node-1)
  api.myapp.com → A 10.0.0.3   (node-2)

  Pros: Simple, no additional infrastructure
  Cons: DNS doesn't know if a node is dead — clients may connect to a failed node

Approach 2: External Load Balancer
  api.myapp.com → A 10.0.0.100  (load balancer VIP)
  Load balancer → [ 10.0.0.1, 10.0.0.2, 10.0.0.3 ]

  Pros: Health-aware routing, no client-side retry needed
  Cons: Additional infrastructure (HAProxy, cloud LB, Cloudflare proxy)

Approach 3: Cloudflare Proxy (Recommended for production)
  api.myapp.com → CNAME proxy.example.com (orange cloud = proxied)
  Cloudflare → [ 10.0.0.1, 10.0.0.2, 10.0.0.3 ] (configured in Cloudflare dashboard)

  Pros: Free DDoS protection, automatic failover, CDN caching
  Cons: Depends on Cloudflare (vendor lock-in for the proxy layer, not the platform)
```

---

---

## 1. DNS Prerequisites (Operator Playbook)

### Single-Node Setup

```
# 1. Get the node's public IP
PUBLIC_IP=$(curl -s ifconfig.me)

# 2. Create a wildcard A record for platform subdomains
#    (In your DNS provider's dashboard or CLI)
*.myplatform.com  A  $PUBLIC_IP  TTL=60

# 3. Create an A record for the admin API
admin.myplatform.com  A  $PUBLIC_IP  TTL=60

# 4. Deploy your first app
wasm-ctl deploy \
  --app api-users \
  --host api-users.myplatform.com \
  --wasm-file ./api-users.wasm
```

### Multi-Node Setup

```
# Nodes: 10.0.0.1, 10.0.0.2, 10.0.0.3

# Option A: DNS round-robin
*.myplatform.com  A  10.0.0.1  TTL=60
*.myplatform.com  A  10.0.0.2  TTL=60
*.myplatform.com  A  10.0.0.3  TTL=60

# Option B: External load balancer
# Deploy HAProxy or use cloud LB with VIP 10.0.0.100
*.myplatform.com  A  10.0.0.100  TTL=60

# Option C: Cloudflare proxy
*.myplatform.com  CNAME  origin.myplatform.com  (proxied)
origin.myplatform.com  A  10.0.0.1  TTL=60
origin.myplatform.com  A  10.0.0.2  TTL=60
origin.myplatform.com  A  10.0.0.3  TTL=60
```

---

## 2. Route Webhook (Optional DNS Automation)

When a route is added or removed (step 15), the platform can optionally fire a webhook
to an external service that manages DNS records.

```rust
// crates/proxy/src/dns_webhook.rs
use common::error::PlatformError;
use serde::{Deserialize, Serialize};

/// Webhook payload sent when a route changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteChangeWebhook {
    /// "add" or "remove"
    pub action: String,

    /// The hostname being routed (e.g. "api-users.myplatform.com")
    pub hostname: String,

    /// The app this hostname points to (e.g. "api-users:v2")
    pub app_id: String,

    /// List of node IPs that can serve this app.
    /// The webhook receiver can use this to create DNS A records.
    pub node_ips: Vec<String>,
}

pub struct DnsWebhookClient {
    endpoint: String,
    auth_token: String,
    client: reqwest::Client,
}

impl DnsWebhookClient {
    pub fn new(endpoint: String, auth_token: String) -> Self {
        DnsWebhookClient {
            endpoint,
            auth_token,
            client: reqwest::Client::new(),
        }
    }

    /// Fire the webhook. Best-effort — failure is logged but does not block the route change.
    pub async fn notify(&self, payload: &RouteChangeWebhook) {
        match self.client
            .post(&self.endpoint)
            .bearer_auth(&self.auth_token)
            .json(payload)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(
                    action = %payload.action,
                    host = %payload.hostname,
                    "DNS webhook delivered successfully"
                );
            }
            Ok(resp) => {
                tracing::warn!(
                    action = %payload.action,
                    host = %payload.hostname,
                    status = %resp.status(),
                    "DNS webhook returned non-success status"
                );
            }
            Err(e) => {
                tracing::warn!(
                    action = %payload.action,
                    host = %payload.hostname,
                    error = %e,
                    "DNS webhook delivery failed"
                );
            }
        }
    }
}
```

---

## 3. Health Check Endpoint for DNS/LB

External load balancers and DNS providers (Cloudflare, Route 53) need a health check
endpoint to know which nodes are alive. This endpoint is exposed on the admin port,
separate from the app traffic port.

```rust
// crates/node/src/admin.rs (extension)
use axum::{routing::get, Router, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: String,
    pub nats_connected: bool,
    pub apps_loaded: u32,
    pub active_instances: u32,
    pub accepting_requests: bool,
}

/// GET /health — used by external LBs and DNS providers.
/// Returns 200 if the node is healthy and accepting traffic.
/// Returns 503 if the node is draining, overloaded, or unhealthy.
pub async fn health_check(
    state: axum::extract::State<Arc<NodeState>>,
) -> impl IntoResponse {
    let healthy = state.nats_health.is_connected()
        && state.backpressure.is_accepting();

    let response = HealthResponse {
        status: if healthy { "ok".into() } else { "degraded".into() },
        node_id: state.node_id.clone(),
        nats_connected: state.nats_health.is_connected(),
        apps_loaded: state.supervisor.app_count().await,
        active_instances: state.supervisor.instance_count().await,
        accepting_requests: state.backpressure.is_accepting(),
    };

    if healthy {
        (StatusCode::OK, Json(response))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(response))
    }
}

/// Configure DNS health check probes:
///
/// Cloudflare: Health Check > HTTP > GET http://<node-ip>:9090/health
///   - Interval: 30s
///   - Healthy threshold: 2 consecutive successes
///   - Unhealthy threshold: 3 consecutive failures
///
/// AWS Route 53: Health Check > HTTP > GET http://<node-ip>:9090/health
///   - Request interval: 30s
///   - Failure threshold: 3
///
/// HAProxy:
///   server node-0 10.0.0.1:8080 check port 9090 inter 10s fall 3 rise 2
```

---

## 4. TLS Certificate Integration

Step 09 mentions TLS termination at Pingora. This section documents the certificate
management prerequisites.

### Static Certificates (Simple)

```
# Place certificate and key on each node:
/etc/wasm-node/tls/cert.pem    ← fullchain certificate
/etc/wasm-node/tls/key.pem     ← private key

# wasm-node config:
[tls]
cert_path = "/etc/wasm-node/tls/cert.pem"
key_path = "/etc/wasm-node/tls/key.pem"

# For wildcard domains, use a wildcard certificate:
# *.myplatform.com certificate covers all app subdomains
```

### ACME / Let's Encrypt (Automated)

For custom domains, each domain needs its own certificate. The platform can integrate
with an ACME client that runs alongside the node:

```
Architecture:
  certbot/acme.sh (external)
      │
      ├── Obtains/renews certificates for configured domains
      ├── Writes to /etc/wasm-node/tls/<domain>/cert.pem
      └── Sends SIGHUP to wasm-node process (Pingora reloads certificates)

This keeps ACME complexity outside the platform's codebase while enabling automatic
certificate management.
```

### Cloudflare Proxy (Simplest)

When using Cloudflare in proxy mode, TLS termination happens at Cloudflare's edge.
The connection from Cloudflare to Pingora can be:
- **Full (strict)**: Pingora needs a valid certificate (recommended)
- **Full**: Pingora needs any certificate (self-signed OK)
- **Flexible**: Pingora runs plain HTTP (simpler but less secure)

---

## 5. Service Discovery (East-West)

For app-to-app communication (East-West traffic), apps need to discover each other
without DNS. The `LocalServiceRegistry` (step 07) provides this for same-node apps.
For cross-node app-to-app calls, the app connects through Pingora like any external
client:

```
App A (on node-0) wants to call App B:
  1. App A sends HTTP request to "http://app-b.myplatform.com/api/data"
  2. DNS resolves to Pingora (same as external traffic)
  3. Pingora routes to App B (on node-0 or node-1, wherever it's running)
```

This means East-West traffic follows the same path as North-South traffic. The advantage
is simplicity: no separate service mesh, no envoy sidecar, no internal-only DNS. The
cost is an extra Pingora hop for same-node calls — acceptable given Pingora's sub-ms
proxy overhead.

For latency-sensitive same-node calls, the `LocalServiceRegistry` provides a shortcut:

```rust
// Inside a Wasm app's host function (future Component Model integration):
// let addr = service_registry.resolve("app-b");
// Direct TCP connection to addr (bypassing Pingora)
```

---

## 6. Configuration

```rust
// crates/common/src/config.rs (extension)
use serde::{Deserialize, Serialize};

/// DNS and discovery configuration for the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsConfig {
    /// Base domain for platform subdomains (e.g. "myplatform.com").
    /// Apps get subdomains like "api-users.myplatform.com".
    pub platform_domain: Option<String>,

    /// Optional webhook URL for DNS automation.
    /// Called when routes are added or removed.
    pub dns_webhook_url: Option<String>,

    /// Auth token for the DNS webhook (sent as Bearer token).
    pub dns_webhook_token: Option<String>,

    /// Admin port for the health check endpoint.
    /// Default: 9090.
    pub admin_port: u16,
}

impl Default for DnsConfig {
    fn default() -> Self {
        DnsConfig {
            platform_domain: None,
            dns_webhook_url: None,
            dns_webhook_token: None,
            admin_port: 9090,
        }
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### DNS Prerequisites
- [ ] Documentation clearly describes required DNS records for single-node and multi-node setups
- [ ] Wildcard domain setup is documented (Model A: platform subdomain)
- [ ] Custom domain setup is documented (Model B: CNAME to platform)

### Health Check
- [ ] `GET /health` returns 200 when the node is healthy
- [ ] `GET /health` returns 503 when the node is draining or NATS-disconnected
- [ ] Health response includes node_id, NATS status, and accepting_requests flag
- [ ] External LB (HAProxy, Cloudflare, Route 53) can use `/health` as a probe target

### Webhook
- [ ] When `dns_webhook_url` is configured, adding a route fires a webhook with action=add
- [ ] Removing a route fires a webhook with action=remove
- [ ] Webhook failure does not block the route change (best-effort delivery)
- [ ] Webhook includes all node IPs for the DNS provider to create A records

### TLS
- [ ] Documentation describes static certificate setup
- [ ] Documentation describes ACME/Let's Encrypt integration
- [ ] Documentation describes Cloudflare proxy mode (zero-config TLS)

### Service Discovery
- [ ] East-West traffic (app-to-app) works via Pingora like external traffic
- [ ] LocalServiceRegistry provides a bypass for latency-sensitive same-node calls
- [ ] No separate service mesh or sidecar is needed
