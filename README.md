# Wasm Cloud Platform

A multi-tenant, HTTP-serving cloud platform built on WebAssembly and WASI. Replaces container-based architectures with lightweight Wasm isolation, deterministic resource metering, and low cold-start latency.

## What this project is

- A Linux-targeted runtime for stateless, HTTP-serving Wasm applications, with
  production deployment controlled by environment-specific validation gates
- A replacement architecture for container-based cloud platforms, optimized for low cold start latency, high density, and deterministic resource metering
- A platform runtime with a built-in API Gateway, node-local service mesh, telemetry integrations, and eBPF kernel monitoring

This project targets Linux production environments. Windows is not a production target because the platform depends on Linux kernel capabilities, including eBPF.

## Core platform goals

- **Low cold-start latency** via AOT-compiled Wasm artifacts and runtime caching
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
- **Internal mesh gateway**: East-West traffic between services is transparently proxied with service-discovery-based namespace isolation and an embedded DNS stub — no external DNS required.
- **eBPF kernel monitoring**: Sub-millisecond detection of failures, anomalies, and security incidents.

## Quick Start

```bash
# Run Rust commands in Linux or WSL2. This checkout is under /mnt/d.
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

# Build the platform
cargo build --release

# Start a single node
$CARGO_TARGET_DIR/release/wasm-node --config config/dev.toml

# Deploy your first app
wasm-ctl deploy \
  --app hello-world \
  --version v1 \
  --wasm hello-world.wasm

# Check node health
wasm-ctl node health

# View metrics
curl http://localhost:9090/metrics
```

## Deployment Levels

The deployment levels organize progressively stronger Linux operating controls.
They do not certify a particular environment. Level 2 is the minimum production
control profile; Levels 3 and 4 add multi-node and high-assurance gates.

- local development: Level 0
- internal single-node service: Level 1
- minimum production control profile: Level 2
- hardened multi-node control profile: Level 3
- high-assurance control profile: Level 4

Start with:

- [`docs/deployment-levels.md`](docs/deployment-levels.md)
- [`docs/deployment-level-0-local-development.md`](docs/deployment-level-0-local-development.md)
- [`docs/deployment-level-1-single-node-private.md`](docs/deployment-level-1-single-node-private.md)
- [`docs/deployment-level-2-production-baseline.md`](docs/deployment-level-2-production-baseline.md)
- [`docs/deployment-level-3-hardened-production.md`](docs/deployment-level-3-hardened-production.md)
- [`docs/deployment-level-4-high-assurance.md`](docs/deployment-level-4-high-assurance.md)

The index routes to one operator guide per level, so each audience gets a concrete path from installation to configuration without being forced through controls they do not need yet.

## Documentation

- [`docs/README.md`](docs/README.md) is the complete user and operator manual index
- [`docs/performance-benchmarks.md`](docs/performance-benchmarks.md) for the supported measurement paths, including the real platform cold-start benchmark

### Getting Started

| Guide | Description |
|-------|-------------|
| [`docs/getting-started.md`](docs/getting-started.md) | Build, install, and run your first node |
| [`docs/deployment-levels.md`](docs/deployment-levels.md) | Index for the graduated Linux deployment guides |
| [`docs/deploying-applications.md`](docs/deploying-applications.md) | Deploy apps with manifests, public routes, security, and secrets |
| [`docs/grpc-compatibility.md`](docs/grpc-compatibility.md) | Current gRPC compatibility boundary: `wasi:http` components are validated for unary and streaming gRPC |
| [`docs/internal-mesh.md`](docs/internal-mesh.md) | East-West communication, namespaces, and service discovery |
| [`docs/nats-setup.md`](docs/nats-setup.md) | NATS deployment, clustering, security, and monitoring |
| [`docs/full-stack-example.md`](docs/full-stack-example.md) | End-to-end example: 2 apps + database + auth |

### Observability & Operations

| Guide | Description |
|-------|-------------|
| [`docs/observability.md`](docs/observability.md) | Metrics, logging, tracing, health checks, alerting, and SRE playbooks |
| [`docs/ebpf.md`](docs/ebpf.md) | eBPF kernel monitoring, security incident detection, and automated recovery |

### Gateway Configuration

| Guide | Description |
|-------|-------------|
| [`docs/gateway/oidc-setup.md`](docs/gateway/oidc-setup.md) | Configuring OIDC providers (Keycloak, etc.) |
| [`docs/gateway/cors-examples.md`](docs/gateway/cors-examples.md) | CORS configuration examples |
| [`docs/gateway/circuit-breaker-tuning.md`](docs/gateway/circuit-breaker-tuning.md) | Tuning circuit breaker thresholds |

### Design Documents

The original implementation design documents are preserved in the `INFRA_IMPL/` folder for reference. These describe the architecture decisions and rationale behind each subsystem.

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
- `config/production.toml` — Production-oriented starting config; replace its
  placeholders and validate it against the target environment

Suggested mapping:

- `config/dev.toml` -> Level 0
- `config/staging.toml` -> Level 1
- `config/production.toml` -> Levels 2 through 4

Use [`docs/deployment-levels.md`](docs/deployment-levels.md) to choose the root-of-trust mode, admin/TLS posture, and peer-transfer posture that match your environment.

### Environment variables

Supported config values can be overridden with the `WASM_NODE_<SECTION>_<KEY>` convention (uppercase, underscores). For example:

- `WASM_NODE_NODE_ID=node-1`
- `WASM_NODE_NATS_URL=tls://nats.prod:4222`
- `WASM_NODE_NATS_CA_CERT=/etc/wasm-node/nats/ca.crt`
- `WASM_NODE_NATS_CLIENT_CERT=/run/credentials/wasm-node/nats-client.crt`
- `WASM_NODE_NATS_CLIENT_KEY=/run/credentials/wasm-node/nats-client.key`
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

The platform includes a Pingora-based API Gateway. Operators can configure authentication, authorization, rate limiting, CORS, circuit breaking, and request transformation for application routes. Distributed rate limiting requires its external coordination service and explicit configuration.

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

## Project Structure

```
.
├── Cargo.toml                 # Workspace root
├── config/                    # Example configurations
│   ├── dev.toml
│   ├── staging.toml
│   └── production.toml
├── crates/                    # Rust workspace crates
│   ├── common/               # Shared types and constants
│   ├── ctl/                  # CLI tool (wasm-ctl)
│   ├── ebpf-monitor/         # eBPF kernel monitoring
│   ├── internal_gateway/     # East-West service mesh proxy
│   ├── messaging/            # NATS integration
│   ├── metrics/              # Prometheus metrics and OpenTelemetry
│   ├── node/                 # Node entrypoint (wasm-node)
│   ├── proxy/                # Pingora HTTP proxy + API Gateway
│   ├── runtime/              # Wasmtime integration, virtual DNS
│   ├── storage/              # redb persistence layer
│   └── supervisor/           # Instance lifecycle management
├── docs/                      # Platform documentation
│   ├── getting-started.md
│   ├── deploying-applications.md
│   ├── internal-mesh.md
│   ├── nats-setup.md
│   ├── full-stack-example.md
│   ├── observability.md
│   ├── ebpf.md
│   └── gateway/
├── INFRA_IMPL/                # Design documents (reference)
└── README.md                  # This file
```

## Notes

- The focus is on **stateless, HTTP-serving applications**.
- The platform is optimized for **high-density multi-tenancy** and **fast startup**.
- The NATS bus is the primary control plane; data plane traffic is handled by Pingora and the Supervisor.
- On supported Linux hosts, eBPF supplies kernel-level process, network, and resource signals. The userspace fallback exposes reduced monitoring and does not satisfy the production kernel-identity contract; see [eBPF monitoring](docs/ebpf.md).

## License

This project is dual-licensed under:

- `MIT`
- `Apache-2.0`

You may use it under either license at your option. See [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).

