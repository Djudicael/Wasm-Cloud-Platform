# Getting Started with the Wasm Cloud Platform

This guide walks you through building, installing, and running the Wasm Cloud Platform from source.

For deployment posture after your first successful startup, use [`docs/deployment-levels.md`](deployment-levels.md). It now acts as an index to one guide per deployment level, so you can follow the path that matches your environment instead of reading one mixed document.

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Prerequisites](#prerequisites)
3. [Building from Source](#building-from-source)
4. [Running a Single Node](#running-a-single-node)
5. [Platform Components](#platform-components)
6. [Next Steps](#next-steps)

---

## Architecture Overview

The Wasm Cloud Platform is a multi-tenant, Wasm-native application platform. Instead of containers, it runs WebAssembly modules with deterministic resource metering, sub-10ms cold starts, and high-density multi-tenancy.

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              Cluster / Platform                               │
│                                                                              │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐                  │
│  │   Node 1     │◄──►│   Node 2     │◄──►│   Node N     │                  │
│  │              │    │              │    │              │                  │
│  │ ┌──────────┐ │    │ ┌──────────┐ │    │ ┌──────────┐ │                  │
│  │ │ Pingora  │ │    │ │ Pingora  │ │    │ │ Pingora  │ │  ← HTTP ingress  │
│  │ │ Proxy    │ │    │ │ Proxy    │ │    │ │ Proxy    │ │    (TLS/80/443)  │
│  │ └────┬─────┘ │    │ └────┬─────┘ │    │ └────┬─────┘ │                  │
│  │      │       │    │      │       │    │      │       │                  │
│  │ ┌────▼─────┐ │    │ ┌────▼─────┐ │    │ ┌────▼─────┐ │                  │
│  │ │Supervisor│ │    │ │Supervisor│ │    │ │Supervisor│ │  ← Instance mgmt │
│  │ │ + Registry│ │   │ │ + Registry│ │   │ │ + Registry│ │                  │
│  │ └────┬─────┘ │    │ └────┬─────┘ │    │ └────┬─────┘ │                  │
│  │      │       │    │      │       │    │      │       │                  │
│  │ ┌────▼─────┐ │    │ ┌────▼─────┐ │    │ ┌────▼─────┐ │                  │
│  │ │ Wasmtime │ │    │ │ Wasmtime │ │    │ │ Wasmtime │ │  ← Wasm runtime  │
│  │ │ Runtime  │ │    │ │ Runtime  │ │    │ │ Runtime  │ │                  │
│ │ │ (AOT)    │ │    │ │ (AOT)    │ │    │ │ (AOT)    │ │                  │
│  │ └──────────┘ │    │ └──────────┘ │    │ └──────────┘ │                  │
│  │      ▲       │    │      ▲       │    │      ▲       │                  │
│  │      │       │    │      │       │    │      │       │                  │
│  │ ┌────┴─────┐ │    │ ┌────┴─────┐ │    │ ┌────┴─────┐ │                  │
│  │ │   redb   │ │    │ │   redb   │ │    │ │   redb   │ │  ← Local store   │
│  │ │ (local)  │ │    │ │ (local)  │ │    │ │ (local)  │ │                  │
│  │ └──────────┘ │    │ └──────────┘ │    │ └──────────┘ │                  │
│  └──────────────┘    └──────────────┘    └──────────────┘                  │
│         ▲                   ▲                   ▲                           │
│         └───────────────────┴───────────────────┘                           │
│                         NATS (JetStream)                                    │
│                   Control plane + events                                    │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

| Aspect | Traditional Container Platform | Wasm Cloud Platform |
|--------|-------------------------------|---------------------|
| Isolation | Linux cgroups/namespaces | Wasm sandbox + fuel metering |
| Cold start | 100ms–2s | <10ms (AOT compiled) |
| Image format | OCI/container images | `.wasm` + AOT artifacts |
| CPU accounting | Wall-clock time / cgroups | Wasm instruction fuel |
| Density | 10s–100s per node | 1000s per node |
| Networking | Direct TCP/HTTP | WASI Preview 2 sockets + proxy |

---

## Prerequisites

### Required

- **Rust** 1.80+ with `wasm32-wasip2` target
- **NATS Server** 2.10+ (with JetStream enabled)
- **PostgreSQL** 14+ (for apps that need databases)
- **Linux** with kernel 5.8+ (for eBPF monitoring; optional)

Production note:

- Linux is the intended production target.
- Windows is not a production target for this project.

### Install Rust toolchain

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Add the WASI Preview 2 target
rustup target add wasm32-wasip2

# Verify
rustc --version
cargo --version
```

### Install NATS Server

```bash
# Option 1: Official binary
curl -sf https://get-nats.io | sh

# Option 2: Docker
docker run -d --name nats -p 4222:4222 -p 8222:8222 nats:2.10 \
  --js --store_dir /data/jetstream

# Option 3: Package manager (Ubuntu/Debian)
sudo apt-get install nats-server
```

### Install PostgreSQL (optional, for database-backed apps)

```bash
# Ubuntu/Debian
sudo apt-get install postgresql postgresql-contrib

# macOS
brew install postgresql
brew services start postgresql
```

---

## Building from Source

### 1. Clone the repository

```bash
git clone https://github.com/your-org/wasm-cloud-platform.git
cd wasm-cloud-platform
```

### 2. Build the workspace

```bash
# Build everything (takes 5–15 minutes on first run)
cargo build --release

# The release binaries will be at:
# target/release/wasm-node   ← The platform node
# target/release/wasm-ctl    ← The operator CLI
```

### 3. Install binaries to PATH

```bash
# Option 1: Copy to a local bin directory
cp target/release/wasm-node ~/.local/bin/
cp target/release/wasm-ctl ~/.local/bin/

# Option 2: Use cargo install
cargo install --path crates/node
cargo install --path crates/ctl

# Verify
wasm-node --version
wasm-ctl --version
```

---

## Running a Single Node

### Step 1: Start NATS with JetStream

```bash
# Create a NATS config file
cat > /tmp/nats-server.conf << 'EOF'
jetstream {
    store_dir = "/tmp/nats-jetstream"
    max_memory_store = 1GB
    max_file_store = 10GB
}

# Enable authentication (production)
# authorization {
#     users = [
#         { user: "platform", password: "strong-password-here" }
#     ]
# }
EOF

# Start NATS
nats-server -c /tmp/nats-server.conf

# Verify NATS is running
nats --server nats://localhost:4222 server info
```

### Step 2: Generate a node config

```bash
# Generate a default config
wasm-node --generate-config > /tmp/wasm-node-config.toml

# The default config looks like this:
cat /tmp/wasm-node-config.toml
```

A minimal starting config:

```toml
[node]
node_id = "node-1"

[storage]
db_path = "/var/lib/wasm-node/state.redb"

[nats]
url = "nats://127.0.0.1:4222"
# creds_file = "/etc/wasm-node/nats.creds"  # For production auth

[proxy]
http_port = 8080
https_port = 8443
tls_cert = "/etc/wasm-node/tls/cert.pem"
tls_key = "/etc/wasm-node/tls/key.pem"

[admin]
port = 9090
artifact_port = 9091

[auth]
enabled = true
read_token = "your-read-token-here"
write_token = "your-write-token-here"
require_tls = true

[runtime]
port_start = 10000
port_end = 19999

[database]
default_url = "postgres://127.0.0.1:5432"
pgbouncer_addr = "127.0.0.1:5432"

[logging]
level = "info"
format = "json"

[dns]
stub_enabled = true
stub_port = 15353

[gateway]
[gateway.oidc]
issuer_url = "https://keycloak.example.com/realms/my-realm"
audience = "my-platform-api"
```

### Step 3: Start the node

```bash
# Create the data directory
sudo mkdir -p /var/lib/wasm-node
sudo chown $USER:$USER /var/lib/wasm-node

# Run the node
wasm-node --config /tmp/wasm-node-config.toml

# Or run with inline flags for quick testing
wasm-node \
  --db-path /tmp/wasm-node.redb \
  --nats-url nats://127.0.0.1:4222 \
  --proxy-port 8080 \
  --admin-port 9090 \
  --node-id node-1
```

You should see logs like:

```json
{"timestamp":"2026-04-25T10:00:00Z","level":"INFO","message":"wasm-node starting","node_id":"node-1"}
{"timestamp":"2026-04-25T10:00:00Z","level":"INFO","message":"NATS connected","url":"nats://127.0.0.1:4222"}
{"timestamp":"2026-04-25T10:00:00Z","level":"INFO","message":"storage opened","path":"/var/lib/wasm-node/state.redb"}
{"timestamp":"2026-04-25T10:00:00Z","level":"INFO","message":"proxy listening","http_port":8080}
{"timestamp":"2026-04-25T10:00:00Z","level":"INFO","message":"admin API listening","port":9090}
```

### Step 4: Verify the node is healthy

```bash
# In another terminal
wasm-ctl node health
# → Startup probe: PASS (startup complete)

wasm-ctl node liveness
# → Liveness probe: PASS

wasm-ctl node readiness
# → Readiness probe: PASS

# Or use curl
curl http://127.0.0.1:9090/healthz
# → {"status":"healthy"}
```

---

## Platform Components

### wasm-node

The platform node is the daemon that runs on every server. It contains:

| Component | Responsibility |
|-----------|---------------|
| **Pingora Proxy** | HTTP ingress, TLS termination, routing, load balancing |
| **Supervisor** | Instance lifecycle (spawn, health-check, kill, scale) |
| **Wasm Runtime** | AOT compilation, fuel metering, memory limits |
| **Storage (redb)** | Local persistence of artifacts, configs, secrets, metrics |
| **NATS Client** | Control plane events, cluster coordination |
| **eBPF Monitor** | Kernel-level observability (Linux only) |
| **Admin API** | REST API for operators and monitoring tools |
| **Embedded DNS Stub** | Resolves `*.internal` hostnames without external DNS |
| **Internal Gateway** | East-West traffic proxy with endpoint-level policies |

### wasm-ctl

The CLI tool for operators:

| Command | Purpose |
|---------|---------|
| `deploy` | Deploy a Wasm application |
| `remove` | Remove an application |
| `app list` | List deployed applications |
| `app manifest` | View an app's effective manifest |
| `gateway` | Manage API gateway config |
| `routes` | Manage HTTP routes |
| `secrets` | Manage application secrets |
| `node` | Node health checks and config |
| `status` | Cluster health overview |

### NATS (Control Plane)

NATS with JetStream is the messaging backbone:

| Stream | Purpose |
|--------|---------|
| `DEPLOY` | App deployment and removal events |
| `CONTROL` | Instance lifecycle, config updates, secrets |
| `NODE` | Load reporting, health snapshots |
| `CLUSTER` | Bootstrap, state snapshots, upgrades |

---

## Next Steps

- **[Deployment Levels](deployment-levels.md)** — Choose the right Linux posture, from local development to high-assurance production
- **[Deploying Applications](deploying-applications.md)** — Deploy your first Wasm app with manifests
- **[Internal Service Mesh](internal-mesh.md)** — Enable East-West communication between apps
- **[NATS Setup & Security](nats-setup.md)** — Production NATS clustering and auth
- **[Full Stack Example](full-stack-example.md)** — End-to-end: 2 apps + database + auth
