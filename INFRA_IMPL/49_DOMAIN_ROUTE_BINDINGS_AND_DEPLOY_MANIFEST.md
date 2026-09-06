# Step 49 - Domain Route Bindings and Deploy Manifest Integration

## Goal

Make public ingress routing explicit and deployable from the same application
manifest used to deploy the Wasm artifact.

The proxy already has the core mechanism needed for multi-tenant public
routing:

- one public HTTP/HTTPS listener,
- request selection by `Host` header and path,
- `HostRouter` mapping `(host, path_prefix)` to `app_id`,
- `UpstreamRegistry` mapping `app_id` to live internal instance addresses.

The missing product-level behavior is that deployment does not currently bind a
domain to the deployed app automatically.

---

## Current State

The proxy distinguishes applications using host-based routing.

Request flow:

1. Browser connects to the single proxy port, usually `:80` or `:443`.
2. The HTTP request carries the public hostname in the `Host` header.
3. `WasmProxy::request_filter()` extracts and normalizes the host.
4. `HostRouter::resolve(host, path)` returns the target `app_id`.
5. `UpstreamRegistry::next(app_id)` selects the live internal instance address.
6. Pingora forwards the request to that instance.

Example:

```text
www.domaina.com -> proxy :443 -> HostRouter -> app-a:v1 -> 127.0.0.1:41001
www.domainb.com -> proxy :443 -> HostRouter -> app-b:v1 -> 127.0.0.1:41002
```

The route model already exists:

```rust
pub struct Route {
    pub host: String,
    pub app_id: AppId,
    pub path_prefix: String,
    pub strip_prefix: bool,
    pub created_at: u64,
    pub updated_at: u64,
}
```

Routes can be added through `Event::RouteAdd`, and the node handler persists
the route and inserts it into the live `HostRouter`.

Startup also reloads persisted routes from storage into the router.

---

## Gap

`DeployIntentRequest` currently includes:

- `app_id`,
- `config`,
- optional `gateway_config`,
- optional `api_keys`,
- `artifact`.

It does not include public route bindings.

That means a deploy can make an app available internally, but it does not
answer the question:

```text
Which public hostnames should reach this app?
```

The operator must add routes separately, for example:

```bash
wasm-ctl routes add --host www.domaina.com --app app-a:v1
wasm-ctl routes add --host www.domainb.com --app app-b:v1
```

This works, but it creates an avoidable split between application deployment
and ingress declaration.

---

## Recommended Approach

Treat domain bindings as first-class deploy intent state.

The deploy manifest should declare the public routes that belong to the app.
The deploy path should validate those routes and publish `RouteAdd` events
after the deploy intent is accepted.

Recommended shape:

```toml
[app]
name = "payments"
version = "v1"
namespace = "tenant-a"
wasm_bind_port = 8080

[[gateway.routes]]
host = "www.domaina.com"
path_prefix = "/"
strip_prefix = false

[[gateway.routes]]
host = "api.domaina.com"
path_prefix = "/v1"
strip_prefix = true

[gateway.auth]
policy = "none"
```

Why `gateway.routes` instead of a top-level `domains` section:

- routing is ingress/gateway behavior,
- routes may include path prefix and strip behavior, not only domains,
- route-level auth/rate-limit/circuit-breaker policy already lives under the
  gateway concept,
- it keeps the manifest model aligned with the existing proxy architecture.

For simple product UX, a CLI may also expose a shorthand:

```bash
wasm-ctl deploy --manifest app.toml --domain www.domaina.com
```

That shorthand should compile down to the same route model:

```text
host = "www.domaina.com"
path_prefix = "/"
strip_prefix = false
```

---

## Proposed Data Model Changes

Add route bindings to the manifest schema.

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayManifestSection {
    #[serde(default)]
    pub host: Option<String>, // keep for compatibility, maps to one default route

    #[serde(default)]
    pub routes: Vec<RouteManifestSection>,

    // existing auth/cors/rate-limit/circuit-breaker/transform/endpoints fields
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteManifestSection {
    pub host: String,

    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,

    #[serde(default)]
    pub strip_prefix: bool,
}

fn default_path_prefix() -> String {
    "/".to_string()
}
```

Compatibility rule:

- `gateway.host = "www.domaina.com"` remains valid.
- Internally it is treated as:

```toml
[[gateway.routes]]
host = "www.domaina.com"
path_prefix = "/"
strip_prefix = false
```

This avoids breaking existing manifests while allowing multiple domains and
path-specific routing.

---

## Proposed Deploy Flow

1. Parse manifest.
2. Build `AppConfig`.
3. Build optional `GatewayRouteConfig`.
4. Build `Vec<Route>` from `gateway.host` and `gateway.routes`.
5. Validate route ownership and conflicts before accepting deployment.
6. Publish `DeployApp`.
7. Publish `GatewayConfigUpdate` if present.
8. Publish one `RouteAdd` per route.
9. Return route publication status in the deploy response.

Route creation should be idempotent for the same app:

- same `host + path_prefix + app_id`: update timestamps/config and succeed,
- same `host + path_prefix` owned by another app: reject unless explicitly
  forced by an admin operation,
- malformed host or empty path prefix: reject,
- wildcard hosts: reject initially unless wildcard ownership validation is
  implemented.

---

## Ownership and Security Requirements

Custom domains must not be blindly accepted in a multi-tenant platform.

Before a route for `www.domaina.com` becomes active, the platform should verify
that the tenant controls the domain.

Recommended ownership checks:

- DNS TXT challenge:

```text
_wasmcloud.www.domaina.com TXT "tenant=<tenant-id>; token=<random-token>"
```

- or CNAME target validation:

```text
www.domaina.com CNAME <tenant-or-app>.platform.example.com
```

Minimum production policy:

- exact host match only,
- normalize hosts to lowercase,
- strip any port suffix before lookup,
- reject IP literals for tenant custom domains,
- reject duplicate active bindings across tenants,
- persist verification status separately from route presence,
- only activate proxy routing when verification is valid.

Suggested future table:

```text
domain_bindings
---------------
id
tenant_id
app_id
host
path_prefix
strip_prefix
verification_status
verification_method
verification_token_hash
created_at
updated_at
verified_at
last_checked_at
```

The existing `Route` table can continue to be the fast runtime routing table,
but domain ownership state should be tracked separately.

---

## TLS Requirement

For HTTPS, the proxy needs certificate selection before HTTP routing.

Routing uses:

```text
Host: www.domaina.com
```

TLS certificate selection uses SNI:

```text
SNI = www.domaina.com
```

So the domain binding lifecycle should eventually include:

1. verify domain ownership,
2. issue or import certificate,
3. install certificate into the proxy TLS/SNI store,
4. activate the route.

Until automatic certificate management exists, HTTP routing can work, but HTTPS
custom domains require manual certificate provisioning.

---

## Best Implementation Plan

### Phase 1 - Manifest to RouteAdd

- Add `gateway.routes` to `DeployManifest`.
- Keep `gateway.host` as compatibility shorthand.
- Add `DeployManifest::to_routes(app_id)` returning `Vec<Route>`.
- Extend `wasm-ctl deploy` so manifest deployments publish `RouteAdd` events
  after `DeployApp`.
- Add unit tests for:
  - single `gateway.host`,
  - multiple `gateway.routes`,
  - empty route list,
  - default `path_prefix = "/"`,
  - duplicate host/path in one manifest.

This phase uses the existing `RouteAdd` event and storage behavior.

### Phase 2 - Deploy Intent API Support

- Add optional `routes: Vec<RouteIntent>` to `DeployIntentRequest`.
- Have deploy ingress publish `RouteAdd` events after artifact ingest and
  deploy acceptance.
- Add route status fields to `DeployIntentResponse`.
- Reject route conflicts before publishing deploy events where possible.

This makes CI/CD remote deploys equivalent to local manifest deploys.

### Phase 3 - Domain Ownership Verification

- Add `domain_bindings` storage state.
- Add admin/API commands:
  - create domain binding,
  - show verification challenge,
  - verify domain,
  - remove domain binding.
- Activate routes only when verification succeeds.
- Add tests that one tenant cannot claim another tenant's verified host.

### Phase 4 - TLS Automation

- Add ACME or certificate import flow.
- Add SNI certificate reload support.
- Couple route activation for HTTPS domains to certificate readiness.

---

## Acceptance Criteria

- A manifest can deploy an app and create `www.domaina.com -> app-a:v1` in one
  operation.
- Two manifests can deploy two apps with different hosts through the same proxy
  listener.
- Requests with `Host: www.domaina.com` reach only app A.
- Requests with `Host: www.domainb.com` reach only app B.
- Unknown hosts return a controlled error, not an arbitrary default app.
- Route conflicts are rejected or require explicit admin override.
- The route state survives node restart through storage reload.

---

## Key Design Decision

The platform should not route public traffic by assigned internal port.

The internal port is only an instance detail. The public contract should be:

```text
tenant + app_id + host + path_prefix -> live upstream instance
```

That keeps the proxy compatible with:

- multiple tenants,
- multiple apps per tenant,
- multiple domains per app,
- multiple path prefixes on one domain,
- rolling deploys,
- horizontal scaling,
- remote nodes,
- future certificate automation.
