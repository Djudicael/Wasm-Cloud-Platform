# Deploying Applications

This guide covers everything you need to deploy, configure, and manage Wasm applications on the platform.

## Table of Contents

1. [Deploying Your First App](#deploying-your-first-app)
2. [Deployment Manifests](#deployment-manifests)
3. [Remote Artifact Deploys](#remote-artifact-deploys)
4. [Per-Endpoint Security](#per-endpoint-security)
5. [Environment Variables & Secrets](#environment-variables--secrets)
6. [Managing Deployments](#managing-deployments)
7. [Zero-Downtime Updates](#zero-downtime-updates)

---

## Deploying Your First App

### Build a Wasm app

The platform expects a Wasm module compiled for the `wasm32-wasip2` target. Here's a minimal Axum app:

```bash
# Create a new Rust project
cargo new --bin hello-api
cd hello-api

# Add dependencies to Cargo.toml
# [dependencies]
# axum = "0.8"
# tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# Edit src/main.rs
```

```rust
use axum::{routing::get, Router};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { "Hello from Wasm!" }))
        .route("/health", get(|| async { "OK" }));

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .unwrap();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

```bash
# Compile to wasm32-wasip2
cargo build --release --target wasm32-wasip2

# The artifact is at:
# target/wasm32-wasip2/release/hello-api.wasm
```

### Deploy via CLI

```bash
# Simple deploy
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --wasm ./target/wasm32-wasip2/release/hello-api.wasm \
  --fuel 500000000 \
  --memory-mb 128 \
  --max-instances 5

# Deploy with environment variables
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --wasm ./hello-api.wasm \
  --env LOG_LEVEL=info \
  --env RUST_LOG=debug

# Deploy into a specific namespace
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --namespace production \
  --wasm ./hello-api.wasm
```

## Remote Artifact Deploys

You can deploy without uploading a local `.wasm` from the CLI host. In that mode, the CLI sends deploy intent to the platform deploy ingress endpoint, and the platform fetches the artifact directly, verifies the digest, stores it locally, and then the normal `DeployApp` flow continues.

Remote deploys use the deploy ingress URL, which can be configured separately from the node admin URL:

```bash
export WASM_CTL_DEPLOY_API=https://deploy.example.com
```

or per command:

```bash
wasm-ctl deploy \
  --deploy-api https://deploy.example.com \
  ...
```

The expected runtime shape is:

1. run one or more `wasm-deploy-ingress` processes as the control-plane ingress
2. point CI at `WASM_CTL_DEPLOY_API`
3. let nodes fetch artifacts from the deploy-ingress artifact server via signed transfer manifests

### Running deploy ingress

The deploy ingress is configured by environment variables and CLI flags, not by the node TOML config files.

Operator-facing examples are in:

- [`config/deploy-ingress.env.example`](../config/deploy-ingress.env.example)
- [`systemd/wasm-deploy-ingress.service`](../systemd/wasm-deploy-ingress.service)

A typical production setup is:

1. install `wasm-deploy-ingress` on a control-plane host
2. create `/etc/wasm-cloud/deploy-ingress.env` from the example
3. provide a stable KEK via `WASM_DEPLOY_INGRESS_KEY_SOURCE`
4. expose the deploy API and artifact port behind your ingress or load balancer as needed
5. point CI at `WASM_CTL_DEPLOY_API`

Do not leave `WASM_DEPLOY_INGRESS_KEY_SOURCE=generate` in production. That mode is only suitable for ephemeral test environments because stored artifact credentials become undecryptable after restart.

If you run more than one deploy-ingress instance:

- keep `WASM_DEPLOY_INGRESS_HA_ENABLED=true`
- give every instance the same KEK source
- point them at the same NATS / JetStream cluster
- put the deploy API and artifact port behind a stable ingress or load balancer

Only the current lease holder accepts mutating deploy requests. Followers stay hot, replicate artifacts, and take over when they acquire the leader lease.

### HTTP(S) artifact URL

```bash
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello-api.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

### Private artifact with platform-managed credential

First store the fetch credential in the platform:

```bash
export WASM_CTL_DEPLOY_API=https://deploy.example.com
wasm-ctl secrets set-artifact-credential --key ghcr-reader
```

`set-artifact-credential` writes to the deploy-ingress credential store, not to node-local runtime secret storage.

By default the stored value is used as a bearer token. If you need an exact `Authorization` header, store it with the `authorization:` prefix, for example:

```text
authorization:Basic <base64(user:token)>
```

Then deploy:

```bash
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello-api.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-credential ghcr-reader
```

Remote artifact fetch is capped at `64 MiB` per artifact. Oversized remote payloads fail closed before deploy publication.

### OCI / GHCR reference

The platform accepts:

- digest-pinned refs such as `oci://ghcr.io/org/app@sha256:...`
- tag refs such as `oci://ghcr.io/org/app:v1`

Resolution happens on the node during deploy ingress. For tag refs, the node fetches the OCI manifest from the registry, resolves the blob digest, then downloads and verifies the blob.

Example:

```bash
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello-api:v1 \
  --artifact-credential ghcr-reader
```

Digest-pinned form is also supported:

```bash
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello-api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-credential ghcr-reader
```

### Optional signed artifact metadata

Deploy ingress can require a signed metadata envelope for remote artifacts. The current implementation verifies an Ed25519 signature over:

- `sha256`
- `issuer`
- `repository`
- `namespace`

Example:

```bash
wasm-ctl deploy \
  --app hello-api \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello-api@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-issuer https://token.actions.githubusercontent.com \
  --artifact-repository example-org/hello-api \
  --artifact-namespace production \
  --artifact-credential ghcr-reader
```

Deploy-ingress policy knobs:

- `WASM_DEPLOY_INGRESS_REQUIRE_SIGNATURE=true`
- `WASM_DEPLOY_INGRESS_ALLOWED_ISSUERS=...`
- `WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES=...`
- `WASM_DEPLOY_INGRESS_ALLOWED_NAMESPACES=...`

Verification records are stored by artifact digest and can be queried from deploy ingress with:

```text
GET /artifacts/{sha256}/verification
```

### Manifest-driven remote deploy

HTTP(S) source:

```toml
[app]
name = "hello-api"
version = "v1"
namespace = "production"
wasm_bind_port = 8080

[artifact]
url = "https://artifacts.example.com/hello-api.wasm"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
credential_ref = "ghcr-reader"
```

OCI source:

```toml
[app]
name = "hello-api"
version = "v1"
namespace = "production"
wasm_bind_port = 8080

[artifact]
reference = "oci://ghcr.io/example-org/hello-api:v1"
credential_ref = "ghcr-reader"
```

### GitHub Actions example

This shape works with GitHub-hosted runners because CI only talks to deploy ingress:

```yaml
name: Deploy Wasm App

on:
  workflow_dispatch:

jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-wasip2
      - run: cargo build --release --target wasm32-wasip2 -p hello-axum
      - run: |
          sha256sum target/wasm32-wasip2/release/hello_axum.wasm | tee artifact.sha256
      - uses: actions/upload-artifact@v4
        with:
          name: hello-axum-wasm
          path: |
            target/wasm32-wasip2/release/hello_axum.wasm
            artifact.sha256

  deploy:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: hello-axum-wasm
          path: dist
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build -p ctl
      - name: Deploy through ingress
        env:
          WASM_CTL_DEPLOY_API: ${{ secrets.WCP_DEPLOY_API }}
          WASM_CTL_NATS_URL: ${{ secrets.WCP_NATS_URL }}
        run: |
          DIGEST="$(cut -d' ' -f1 dist/artifact.sha256)"
          target/debug/wasm-ctl deploy \
            --app hello-axum \
            --version v1 \
            --artifact-url https://artifacts.example.com/hello_axum.wasm \
            --sha256 "$DIGEST" \
            --artifact-credential ghcr-reader
```

Required secrets:

- `WCP_DEPLOY_API`
- `WCP_NATS_URL`

Optional deploy-time setup, done once on the platform:

- `wasm-ctl secrets set-artifact-credential --key ghcr-reader`

Runtime app secrets stay separate. CI should not send plaintext `DATABASE_URL`, JWT keys, or similar values in the deploy request.

### Add a public route

```bash
# Route external traffic to the app
wasm-ctl routes add \
  --host api.example.com \
  --app hello-api:v1 \
  --path-prefix /

# The app is now reachable at:
curl http://api.example.com/
# → Hello from Wasm!
```

---

## Deployment Manifests

For production deployments, use a **deployment manifest** — a single TOML file that describes everything about your application.

### Minimal manifest

```toml
# hello-api.toml
[app]
name = "hello-api"
version = "v1"
namespace = "default"
wasm_artifact = "./target/wasm32-wasip2/release/hello-api.wasm"
wasm_bind_port = 8080

[fuel]
quota = 500_000_000        # ~500ms CPU per request
memory_pages = 2048        # 128 MB
max_instances = 10
idle_timeout_secs = 300

[policy]
profile = "http_api"
```

### Deploy from manifest

```bash
wasm-ctl deploy --manifest ./hello-api.toml
```

### Full manifest with gateway config

```toml
# api-users.toml
[app]
name = "api-users"
version = "v2"
namespace = "production"
wasm_artifact = "./target/wasm32-wasip2/release/api-users.wasm"
wasm_bind_port = 8080

[fuel]
quota = 1_000_000_000
memory_pages = 4096
max_instances = 20
idle_timeout_secs = 300

[policy]
profile = "http_api"

[policy.network]
allow_outbound_tcp = true
allow_dns = true
max_outbound_connections = 50
allowed_cidrs = ["10.0.0.0/8"]

# External hostname
[gateway]
host = "api-users.example.com"

# Auth: JWT + roles
[gateway.auth]
policy = "roles"
allowed_roles = ["user", "admin"]
client_id = "api-users"

# CORS
[gateway.cors]
allowed_origins = ["https://app.example.com", "https://admin.example.com"]
allow_credentials = true
max_age_secs = 3600

# Rate limiting
[gateway.rate_limit]
requests_per_second = 500
burst_capacity = 100
distributed = true

# Circuit breaker
[gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30

# Request transformation
[gateway.transform]
add_headers = [
    ["X-Api-Version", "2"],
    ["X-Platform-Region", "eu-west-1"],
]
remove_headers = ["X-Internal-Token"]

# Per-endpoint security overrides
[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"                     # Public health check

[[gateway.endpoints]]
path = "/api/public"
methods = ["GET"]
auth = "api_key"                  # Requires X-Api-Key header
rate_limit = { requests_per_second = 200, burst_capacity = 40 }

[[gateway.endpoints]]
path = "/api/users"
methods = ["GET", "POST", "PUT"]
auth = "roles"
allowed_roles = ["user", "admin"]
required_scopes = ["read:users"]
rate_limit = { requests_per_second = 100, burst_capacity = 20 }

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST", "DELETE"]
auth = "roles"
allowed_roles = ["admin"]         # Stricter than route default
required_scopes = ["admin:users"]
rate_limit = { requests_per_second = 20, burst_capacity = 5 }

# Environment variables (non-secret)
[env]
LOG_LEVEL = "info"
DATABASE_POOL_SIZE = "10"

# Secrets (references — actual values injected by platform)
[secrets]
DATABASE_URL = { ref = "prod-postgres-url" }
JWT_SECRET = { ref = "api-users-jwt-secret" }

# API Keys for endpoint-level auth
[[api_keys]]
name = "mobile-client"
key_hash = "sha256$abc123..."     # Hashed key stored in redb
scopes = ["/api/public"]

[[api_keys]]
name = "partner-integration"
key_hash = "sha256$def456..."
scopes = ["/api/public", "/api/users"]
```

### Manifest overrides

CLI flags override manifest values:

```bash
# Use manifest but override namespace and memory
wasm-ctl deploy \
  --manifest ./api-users.toml \
  --namespace staging \
  --memory-mb 256
```

---

## Per-Endpoint Security

The platform supports fine-grained security rules at the endpoint level. Rules are evaluated **top-to-bottom**, first match wins.

### Auth policies

| Policy | Description |
|--------|-------------|
| `none` | No authentication required |
| `authenticated` | Valid JWT required |
| `roles` | Valid JWT + user must have one of the allowed roles |
| `api_key` | Valid `X-Api-Key` header required |
| `inherit` | Use the route-level default (useful for explicit fallback) |

Endpoint rules may also declare `required_scopes = ["..."]` when the JWT must carry specific scopes in `scope` or `scp`.

### Example: Public health check + protected admin

```toml
[gateway.auth]
policy = "authenticated"          # Default: require JWT

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"                     # Override: no JWT needed

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST", "DELETE"]
auth = "roles"
allowed_roles = ["admin"]
required_scopes = ["admin:users"]
```

**Behavior:**
- `GET /health` → **No auth** (passes immediately)
- `GET /api/users` → **JWT required** (inherits route default)
- `POST /api/admin` → **JWT + admin role + admin:users scope** required

### Example: Multiple endpoint rules

Use one `[[gateway.endpoints]]` block per endpoint rule. Rules are evaluated top-to-bottom and the first match wins.

```toml
[gateway.auth]
policy = "authenticated"

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/api/users"
methods = ["GET"]
auth = "authenticated"
required_scopes = ["read:users"]

[[gateway.endpoints]]
path = "/api/users"
methods = ["POST"]
auth = "roles"
allowed_roles = ["admin", "editor"]
required_scopes = ["write:users"]

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST", "DELETE"]
auth = "roles"
allowed_roles = ["admin"]
required_scopes = ["admin:users"]

[[gateway.endpoints]]
path = "/api/public"
methods = ["GET"]
auth = "api_key"
```

In that example:

- `GET /health` is public
- `GET /api/users` requires a valid JWT with `read:users`
- `POST /api/users` requires a valid JWT, one of `admin|editor`, and `write:users`
- `POST` or `DELETE /api/admin` requires `admin` plus `admin:users`
- `GET /api/public` requires an API key

### Managing API keys

```bash
# Add an API key to an app
wasm-ctl gateway api-key add api-users:v2 \
  --namespace production \
  --name "mobile-client" \
  --scopes "/api/public" \
  --key "ak_live_xxxxxxxx"

# The CLI hashes the key immediately and discards the plaintext.
# Only the SHA-256 hash is stored in redb and replicated via NATS.
```

---

## Environment Variables & Secrets

### Static environment variables

Set in the manifest `[env]` section or via CLI:

```bash
wasm-ctl deploy \
  --app my-app \
  --wasm ./my-app.wasm \
  --env LOG_LEVEL=debug \
  --env FEATURE_FLAG_X=true
```

### Secrets

Secrets are **never** stored in the manifest. They are referenced by name and injected by the platform at runtime.

```toml
[secrets]
DATABASE_URL = { ref = "prod-postgres-url" }
STRIPE_KEY = { ref = "stripe-live-key" }
```

The actual values are stored encrypted in the platform's `redb` database and injected as environment variables when the Wasm instance starts.

**Set a secret:**

```bash
wasm-ctl secrets set api-users:v2 DATABASE_URL "postgres://..."
wasm-ctl secrets set api-users:v2 STRIPE_KEY "sk_live_..."
```

**List secrets for an app:**

```bash
wasm-ctl secrets list api-users:v2
```

---

## Managing Deployments

### List all apps

```bash
# All apps
wasm-ctl app list

# Apps in a specific namespace
wasm-ctl app list --namespace production

# Output:
# Apps in namespace 'production':
#   api-users:v2 (5 instances)
#   payments:v1 (3 instances)
```

### View an app's effective manifest

```bash
wasm-ctl app manifest api-users:v2 --namespace production
```

This reconstructs the full manifest from the live config stored in redb, including any runtime changes.

### Remove an app

```bash
wasm-ctl remove api-users:v2
```

This sends a `RemoveApp` event to all nodes, which gracefully drain and delete the app's instances.

### View instances

```bash
wasm-ctl instances
# → Shows running instances across the cluster
```

---

## Zero-Downtime Updates

The platform supports rolling updates with graceful drain:

### Update an app (new version)

```bash
# Deploy v2 alongside v1
wasm-ctl deploy \
  --manifest ./api-users-v2.toml \
  --version v2

# Gradually shift traffic by updating the route
wasm-ctl routes update \
  --host api-users.example.com \
  --app api-users:v2

# Once confirmed, remove v1
wasm-ctl remove api-users:v1
```

### How it works

1. **New version deployed** — Nodes compile the new Wasm artifact
2. **Instances spawn** — New instances start with the new config
3. **Route updated** — New requests go to v2 instances
4. **Old instances drain** — Existing requests finish, then old instances are killed
5. **Cleanup** — v1 artifact is garbage collected after the grace period

This is all handled automatically by the deployment protocol. No manual intervention required.

---

## Policy Profiles

The platform provides pre-defined WASI policy profiles for common use cases:

| Profile | Use case | Network | Filesystem |
|---------|----------|---------|------------|
| `http_api` | HTTP API servers | Outbound TCP, DNS | Minimal |
| `background_worker` | Background jobs | Outbound TCP, DNS | Moderate |
| `static_site` | Static file serving | No outbound | Minimal |
| `database_proxy` | DB connection proxies | Outbound TCP, DNS | Minimal |
| `unrestricted` | Development only | Everything | Everything |

```bash
wasm-ctl deploy \
  --app my-app \
  --wasm ./my-app.wasm \
  --policy-profile http_api
```

You can override individual policy fields:

```bash
wasm-ctl deploy \
  --app my-app \
  --wasm ./my-app.wasm \
  --policy-profile http_api \
  --policy-network-max-outbound-connections 100 \
  --policy-fs-max-open-fds 128
```
