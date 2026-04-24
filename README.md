# Wasm Cloud Platform — Implementation Master Plan

This repository is the implementation blueprint for a Wasm-native cloud platform. It is organized as a sequence of detailed design and implementation documents in the `INFRA_IMPL` folder. Each file represents one buildable layer of the platform and is intended to be implemented in order.

## What this project is

- A full technical plan for a multi-tenant, HTTP-serving platform built on WebAssembly and WASI.
- A replacement architecture for container-based cloud platforms, optimized for low cold start latency, high density, and deterministic resource metering.
- A documentation-first repo: the code is guided by the implementation layers defined in `INFRA_IMPL`.

## Core platform goals

- **Cold Start < 10ms** via AOT-compiled Wasm artifacts
- **Fuel-based CPU accounting** for deterministic multi-tenant fairness
- **Shared-nothing node architecture** with local `redb` persistence
- **NATS control plane** for deploys, secrets, and cluster coordination
- **Pingora proxy** for North-South routing, TLS, rate limiting, and health checks
- **Extended resource isolation** beyond fuel and memory, including I/O limits
- **Zero-downtime deploys** with rolling upgrades and graceful drain

## Key concepts

- **Wasm modules instead of containers**: lighter isolation, smaller memory footprint, architecture-neutral binaries.
- **Fuel metering**: CPU usage is measured in Wasm instruction units, not wall-clock time.
- **WASI environment injection**: app config and secrets are injected as environment variables.
- **Per-node local persistence**: `redb` stores artifacts, configs, secrets, metrics, and routing data locally.
- **Proxy + supervisor split**: Pingora handles incoming HTTP traffic, while the Supervisor manages instance lifecycle.

## Documentation index

The implementation plan is split into ordered chapters in `INFRA_IMPL`:

- `00_OVERVIEW.md` — Master plan and architecture summary
- `01_WORKSPACE_SETUP.md` — Cargo workspace and crate structure
- `02_STORAGE_REDB.md` — Local persistent storage design using `redb`
- `03_WASM_RUNTIME.md` — Wasmtime integration, AOT compile, fuel and memory limits
- `04_WASI_NETWORKING.md` — WASI Preview 2 socket model and port allocation
- `05_ENV_CONFIG.md` — Environment variable and secret injection for Wasm apps
- `06_SECRETS.md` — Encrypted secret storage and rotation
- `07_SUPERVISOR_CORE.md` — Supervisor lifecycle, health loop, spawn/prune
- `08_NATS_MESSAGING.md` — NATS control plane and messaging patterns
- `09_PROXY_PINGORA.md` — Proxy routing, upstream registry, and health checks
- `10_DEPLOYMENT_PROTOCOL.md` — Zero-downtime deploy and rollback strategy
- `11_METRICS_OBSERVABILITY.md` — Prometheus metrics and distributed tracing
- `12_SCALING.md` — Autoscaling, fuel steering, and request load balancing
- `13_SECURITY.md` — Sandbox, network policy, and multi-tenant security
- `14_NODE_ENTRYPOINT.md` — Node startup wiring and CLI integration
- `15_ROUTE_MANAGEMENT.md` — Routing table sync and host-to-app mapping
- `16_ARTIFACT_REGISTRY.md` — Artifact distribution and large binary handling
- `17_WASM_LOGS.md` — Logging capture and streaming
- `18_ADMIN_CLI.md` — Operator CLI and management commands
- `19_CLUSTER_BOOTSTRAP.md` — Node join, state sync, and bootstrap processes
- `20_GRACEFUL_SHUTDOWN.md` — Instance drain and shutdown semantics
- `21_DATABASE_CONNECTIONS.md` — DB pooling and connection limits
- `22_STORAGE_SCHEMA_VERSIONING.md` — `redb` schema migration strategy
- `23_INTEGRATION_TESTING.md` — Test strategy for unit, E2E, and chaos
- `24_RATE_LIMITING.md` — Proxy rate limiting and DDoS protection
- `25_PLATFORM_UPGRADES.md` — Rolling platform upgrades and backward compatibility
- `26_ARTIFACT_GC.md` — Artifact and metrics garbage collection
- `27_DISASTER_RECOVERY.md` — Failure classification and recovery playbooks
- `28_BILLING_ACCOUNTING.md` — Tamper-evident fuel billing and accounting
- `29_DNS_INTEGRATION.md` — DNS routing, custom domains, and TLS integration
- `39_API_GATEWAY.md` — Built-in API Gateway: auth, CORS, circuit breaker, transforms

## How to use this repo

1. Start with `INFRA_IMPL/00_OVERVIEW.md` to understand the architecture and lifecycle.
2. Follow the files in numeric order to build the platform layer by layer.
3. Use the completion checklists at the end of each document to verify each feature.
4. Refer to the architecture diagrams and rationale sections when implementing the runtime and node components.

## What this README is not

This repo is not a finished product repository of executable application code. It is a design and implementation plan for a Wasm-native cloud runtime.

## Configuration Management

The node supports layered configuration from multiple sources with a clear merge priority:

```
defaults < TOML file < environment variables < CLI flags
```

### Quick start

```bash
# Run with a config file
wasm-node --config /etc/wasm-node/config.toml

# Generate a default config file
wasm-node --generate-config > /etc/wasm-node/config.toml

# Validate a config file without starting
wasm-node --validate-config /etc/wasm-node/config.toml

# View / change hot-reloadable config at runtime
wasm-ctl node config
wasm-ctl node config --set rate_limit_default_rps=5000 --set logging_level=debug
wasm-ctl node config --reset
wasm-ctl node config --json
```

### Config file locations

Example configuration files are provided in the `config/` directory:

- `config/dev.toml` — Minimal config for local development
- `config/staging.toml` — Staging environment with moderate thresholds
- `config/production.toml` — Production-ready config with security hardening

### Environment variables

All config values can be overridden with the `WASM_NODE_<SECTION>_<KEY>` convention (uppercase, underscores). For example:

- `WASM_NODE_NODE_ID=node-1`
- `WASM_NODE_NATS_URL=nats://nats.prod:4222`
- `WASM_NODE_LOGGING_LEVEL=debug`
- `WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND=5000`

### Hot-reloadable parameters

Selected parameters can be changed at runtime without restarting the node:

- Rate limits (per-app RPS, burst, per-IP)
- eBPF thresholds (FD limits, memory pressure, disk I/O, TCP limits, syscall rate)
- GC interval and disk warning threshold
- Health check interval and idle timeout
- Log level

Non-reloadable parameters (require restart): NATS URL, proxy ports, TLS certs, port range, database path, node ID, key source.

See `INFRA_IMPL/32_CONFIGURATION_MANAGEMENT.md` for the full specification.

## API Gateway

The platform includes a built-in API Gateway that transforms the existing Pingora proxy into a full-featured gateway. Every Wasm application automatically gets authentication, authorization, distributed rate limiting, CORS, circuit breaking, and request transformation — without any external system.

### Gateway Features

- **JWT/OIDC Authentication** — Validate JWTs from Keycloak or any OIDC-compliant IdP
- **Role-based Authorization** — Check realm and client roles from JWT claims
- **Distributed Rate Limiting** — Share rate-limit counters across nodes via NATS KV
- **CORS** — Per-route CORS policies with preflight handling
- **Circuit Breaker** — Per-app circuit breaker with Closed → Open → HalfOpen states
- **Request Transformation** — Header injection, removal, path rewriting, query param stripping
- **Gateway Metrics** — Prometheus metrics for auth, rate limiting, circuit breaker, CORS

### Quick Start

```bash
# Configure OIDC provider in node config.toml
[gateway.oidc]
issuer_url = "https://keycloak.example.com/realms/my-realm"
audience = "my-platform-api"

# Deploy with authentication
wasm-ctl deploy \
  --app api-users \
  --version v2 \
  --wasm api-users.wasm \
  --gateway-auth roles \
  --gateway-roles admin,user \
  --gateway-oidc-client api-users \
  --gateway-cors-origins "https://app.example.com" \
  --gateway-rps 500

# Set gateway config via CLI
wasm-ctl gateway set-auth api-users:v2 --policy roles --roles admin,user
wasm-ctl gateway set-cors api-users:v2 --origins "https://app.example.com" --credentials
wasm-ctl gateway set-rate-limit api-users:v2 --rps 500 --burst 100 --distributed
wasm-ctl gateway show api-users:v2
wasm-ctl gateway reset api-users:v2

# Admin API endpoints
GET  /admin/gateway              # List all gateway configs
GET  /admin/gateway/{app_id}     # Get config for an app
POST /admin/gateway/{app_id}     # Set config for an app
DELETE /admin/gateway/{app_id}   # Remove config for an app
```

### Deploy Manifest Format

```toml
[app]
id = "api-users:v2"
wasm_bind_port = 8080

# Authentication
[app.gateway.auth]
policy = "roles"
allowed_roles = ["admin", "user"]
client_id = "api-users"

# CORS
[app.gateway.cors]
allowed_origins = ["https://app.example.com"]
allow_credentials = true
max_age_secs = 3600

# Rate limiting
[app.gateway.rate_limit]
requests_per_second = 500
burst_capacity = 100
distributed = true

# Circuit breaker
[app.gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30

# Request transformation
[app.gateway.transform]
add_headers = [["X-Api-Version", "2"]]
remove_headers = ["X-Internal-Token"]
```

### Architecture

```
Incoming Request
     │
     ▼
┌─────────────────────────┐
│ 1. Route Resolution     │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 2. CORS Preflight       │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 3. Authentication       │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 4. Authorization        │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 5. Rate Limiting        │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 6. Circuit Breaker      │
└────────┬────────────────┘
         ▼
┌─────────────────────────┐
│ 7. Request Transform    │
└────────┬────────────────┘
         ▼
    Upstream (Wasm App)
```

### Security Considerations

- JWT validation happens **before** traffic reaches the Wasm app
- The gateway does **not** forward the raw `Authorization` header upstream by default
- User identity is injected as `X-User-Id`, `X-User-Email`, `X-User-Roles`
- When `allow_credentials: true`, origins must be specific (not `*`)
- JWKS is fetched over HTTPS; refresh failures are logged and alerted

See `INFRA_IMPL/39_API_GATEWAY.md` for the full specification.

## Notes

- The focus is on **stateless, HTTP-serving applications**.
- The platform is optimized for **high-density multi-tenancy** and **fast startup**.
- The NATS bus is the primary control plane; data plane traffic is handled by Pingora and the Supervisor.

## Contact

See the `INFRA_IMPL` folder for the exact design docs and implementation priorities.
