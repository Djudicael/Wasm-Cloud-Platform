# NATS Setup & Security

NATS is the control plane backbone of the Wasm Cloud Platform. This guide covers deploying NATS for development, staging, and production, with a focus on security, clustering, and observability.

## Table of Contents

1. [Why NATS?](#why-nats)
2. [Single-Node Development Setup](#single-node-development-setup)
3. [Production Cluster Setup](#production-cluster-setup)
4. [Authentication & Authorization](#authentication--authorization)
5. [JetStream Configuration](#jetstream-configuration)
6. [TLS / mTLS](#tls--mtls)
7. [Monitoring & Alerting](#monitoring--alerting)
8. [Disaster Recovery](#disaster-recovery)
9. [Platform Integration](#platform-integration)

---

## Why NATS?

The platform uses NATS for:

| Use Case | NATS Feature |
|----------|-------------|
| App deployment events | Core NATS pub/sub |
| Instance lifecycle | Core NATS pub/sub |
| Gateway config updates | Core NATS pub/sub |
| Secrets replication | Core NATS pub/sub |
| State snapshots | Core NATS request/reply |
| Distributed rate limits | JetStream KV |
| Cluster bootstrap | JetStream Streams |
| Rolling upgrades | Core NATS pub/sub |

NATS was chosen because it is:
- **Lightweight** — single binary, no external dependencies
- **Fast** — millions of messages per second
- **Resilient** — built-in clustering, automatic failover
- **Persistent** — JetStream provides exactly-once delivery
- **Multi-tenant** — accounts and streams isolate workloads

---

## Single-Node Development Setup

### Quick start

```bash
# Start NATS with JetStream enabled
nats-server -js -m 8222

# -js  = enable JetStream
# -m    = enable HTTP monitoring on port 8222
```

### With a config file

```bash
cat > /tmp/nats-dev.conf << 'EOF'
port: 4222
http_port: 8222

jetstream {
    store_dir: "/tmp/nats-jetstream"
    max_memory_store: 256MB
    max_file_store: 1GB
}

# Disable authentication for local dev
authorization {
    users: [
        { user: "dev", password: "dev" }
    ]
}
EOF

nats-server -c /tmp/nats-dev.conf
```

### Connect the platform node

```bash
wasm-node \
  --nats-url nats://dev:dev@127.0.0.1:4222 \
  --proxy-port 8080 \
  --admin-port 9090
```

Or in `config.toml`:

```toml
[nats]
url = "nats://dev:dev@127.0.0.1:4222"
```

---

## Production Cluster Setup

A production NATS cluster should have **at least 3 nodes** for quorum.

### Node 1

```bash
cat > /etc/nats/nats-server-1.conf << 'EOF'
server_name: nats-1
port: 4222
http_port: 8222

# Clustering
cluster {
    name: wasm-platform
    port: 6222
    routes: [
        "nats-route://nats-2:6222",
        "nats-route://nats-3:6222"
    ]
}

# JetStream
jetstream {
    store_dir: "/var/lib/nats/jetstream"
    max_memory_store: 4GB
    max_file_store: 100GB
}

# Limits
max_connections: 10000
max_payload: 8MB
max_pending: 64MB

# Logging
debug: false
trace: false
logfile: "/var/log/nats/nats-server.log"
EOF

nats-server -c /etc/nats/nats-server-1.conf
```

### Node 2

```bash
cat > /etc/nats/nats-server-2.conf << 'EOF'
server_name: nats-2
port: 4222
http_port: 8222

cluster {
    name: wasm-platform
    port: 6222
    routes: [
        "nats-route://nats-1:6222",
        "nats-route://nats-3:6222"
    ]
}

jetstream {
    store_dir: "/var/lib/nats/jetstream"
    max_memory_store: 4GB
    max_file_store: 100GB
}
EOF

nats-server -c /etc/nats/nats-server-2.conf
```

### Node 3

```bash
cat > /etc/nats/nats-server-3.conf << 'EOF'
server_name: nats-3
port: 4222
http_port: 8222

cluster {
    name: wasm-platform
    port: 6222
    routes: [
        "nats-route://nats-1:6222",
        "nats-route://nats-2:6222"
    ]
}

jetstream {
    store_dir: "/var/lib/nats/jetstream"
    max_memory_store: 4GB
    max_file_store: 100GB
}
EOF

nats-server -c /etc/nats/nats-server-3.conf
```

### Verify cluster health

```bash
# Check cluster membership
nats --server nats://nats-1:4222 server list

# Check JetStream cluster status
nats --server nats://nats-1:4222 server report jetstream

# Expected: all 3 nodes show "Current" status
```

---

## Authentication & Authorization

### Option 1: Username / Password (simple)

```bash
cat > /etc/nats/auth.conf << 'EOF'
authorization {
    users: [
        { user: "platform", password: "$2a$11$..." }  # bcrypt hash
    ]
}
EOF
```

Generate the bcrypt hash:

```bash
nats --server nats://localhost:4222 server passwd
```

### Option 2: NATS Credentials (recommended)

```bash
# Create an operator
nsc add operator --generate-signing-key --sys --name platform-operator

# Create an account for the Wasm platform
nsc add account --name wasm-platform

# Create a user for the nodes
nsc add user --name wasm-node --account wasm-platform

# Generate the credentials file
nsc generate creds --name wasm-node > /etc/wasm-node/nats.creds
```

Platform node config:

```toml
[nats]
url = "nats://nats-1:4222,nats://nats-2:4222,nats://nats-3:4222"
creds_file = "/etc/wasm-node/nats.creds"
```

### Option 3: mTLS (highest security)

See the [TLS / mTLS](#tls--mtls) section below.

---

## JetStream Configuration

The platform uses JetStream for durable messaging and KV storage.

### Streams the platform creates automatically

| Stream | Subjects | Retention | Purpose |
|--------|----------|-----------|---------|
| `DEPLOY` | `deploy.app.>` | WorkQueue | App deployment events |
| `CONTROL` | `instance.>`, `secrets.>`, `config.>` | Limits | Instance lifecycle, config |
| `NODE` | `node.load.>`, `node.health.>` | Limits | Load reporting |
| `CLUSTER` | `cluster.>` | Limits | Bootstrap, upgrades |

### KV buckets the platform creates automatically

| Bucket | Purpose |
|--------|---------|
| `rate_limits` | Distributed rate limit counters |

### Manual stream creation (optional)

If you want to pre-create streams with custom settings:

```bash
# Deploy stream
nats --server nats://nats-1:4222 stream create DEPLOY \
  --subjects "deploy.app.>" \
  --retention work \
  --replicas 3 \
  --max-msgs 10000 \
  --max-age 24h

# Control stream
nats --server nats://nats-1:4222 stream create CONTROL \
  --subjects "instance.>,secrets.>,config.>" \
  --retention limits \
  --replicas 3 \
  --max-msgs 100000 \
  --max-age 7d

# Rate limit KV bucket
nats --server nats://nats-1:4222 kv create rate_limits \
  --replicas 3 \
  --ttl 10s \
  --history 1
```

---

## TLS / mTLS

### Server-side TLS (encrypt NATS traffic)

```bash
cat > /etc/nats/tls.conf << 'EOF'
port: 4222
tls {
    cert_file: "/etc/nats/tls/server.crt"
    key_file: "/etc/nats/tls/server.key"
    ca_file: "/etc/nats/tls/ca.crt"
    verify: true
    verify_and_map: true
}
EOF
```

Generate certificates (using mkcert or your CA):

```bash
# Generate CA
openssl req -x509 -newkey rsa:4096 -keyout ca.key -out ca.crt -days 365 -nodes

# Generate server cert
openssl req -newkey rsa:4096 -keyout server.key -out server.csr -nodes
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -out server.crt -days 365

# Generate client cert for the platform node
openssl req -newkey rsa:4096 -keyout client.key -out client.csr -nodes
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -out client.crt -days 365
```

Platform node config:

```toml
[nats]
url = "tls://nats-1:4222"
creds_file = "/etc/wasm-node/nats.creds"
```

### Full mTLS (mutual authentication)

With `verify_and_map: true`, NATS verifies the client certificate and maps it to a user identity. This is the most secure configuration.

---

## Monitoring & Alerting

### NATS built-in monitoring

```bash
# Server info
curl http://nats-1:8222/varz | jq .

# Connection info
curl http://nats-1:8222/connz | jq .

# JetStream info
curl http://nats-1:8222/jsz | jq .

# Route info
curl http://nats-1:8222/routez | jq .
```

### Key metrics to monitor

| Metric | Warning Threshold | Critical Threshold |
|--------|-------------------|-------------------|
| `connections` | > 5000 | > 9000 |
| `jetstream.store` | > 80% disk | > 95% disk |
| `jetstream.memory` | > 80% limit | > 95% limit |
| `routes.num_routes` | < 2 (3-node cluster) | < 1 |
| `slow_consumers` | > 0 | > 10 |
| `mem` | > 80% | > 95% |

### Prometheus integration

NATS exposes a `/metrics` endpoint when started with `-m`:

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'nats'
    static_configs:
      - targets: ['nats-1:8222', 'nats-2:8222', 'nats-3:8222']
```

### Alerting rules (Prometheus)

```yaml
groups:
  - name: nats-alerts
    rules:
      - alert: NATSNodeDown
        expr: up{job="nats"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "NATS node {{ $labels.instance }} is down"

      - alert: NATSJetStreamDiskHigh
        expr: nats_jetstream_store_used_bytes / nats_jetstream_store_max_bytes > 0.9
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "NATS JetStream disk usage is high"

      - alert: NATSClusterUnhealthy
        expr: nats_cluster_size < 3
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "NATS cluster has fewer than 3 nodes"
```

---

## Disaster Recovery

### Backup JetStream

```bash
# Snapshot all streams
nats --server nats://nats-1:4222 stream backup /backup/nats-streams

# Snapshot all KV buckets
nats --server nats://nats-1:4222 kv status rate_limits
```

### Restore from backup

```bash
# Restore streams
nats --server nats://nats-1:4222 stream restore /backup/nats-streams
```

### Platform recovery without NATS data

The platform is designed to be **stateless relative to NATS**. If NATS data is lost:

1. Redeploy apps via `wasm-ctl deploy` — configs are stored in `redb` on each node
2. Re-add routes via `wasm-ctl routes add`
3. Re-set secrets via `wasm-ctl secrets set`

The platform will rebuild its state from the node-local `redb` databases. NATS is used for events, not as the source of truth.

---

## Platform Integration

### How the platform uses NATS

```
┌─────────────────────────────────────────────────────────────────┐
│                         Platform Node                            │
│                                                                  │
│  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐      │
│  │  Deploy     │────►│  NATS Bus   │◄────│  Other      │      │
│  │  Event      │     │  (pub/sub)  │     │  Nodes      │      │
│  └─────────────┘     └──────┬──────┘     └─────────────┘      │
│                             │                                    │
│                    ┌────────┴────────┐                          │
│                    │  JetStream KV   │                          │
│                    │  rate_limits    │                          │
│                    └─────────────────┘                          │
│                                                                  │
│  Streams:                                                        │
│  - DEPLOY   → durable consumer per node                          │
│  - CONTROL  → durable consumer per node                          │
│  - NODE     → durable consumer per node                          │
│  - CLUSTER  → ephemeral subscriptions                            │
└─────────────────────────────────────────────────────────────────┘
```

### NATS connection resilience

The platform handles NATS disconnections gracefully:

- **Deploy events** are queued locally if NATS is down; retried on reconnect
- **Instance lifecycle** continues operating (NATS is control plane, not data plane)
- **Rate limit KV** falls back to local-only mode if NATS KV is unavailable
- **Health checks** mark NATS as a dependency; node becomes "degraded" if disconnected

### Configuring multiple NATS servers

```toml
[nats]
url = "nats://nats-1:4222,nats://nats-2:4222,nats://nats-3:4222"
creds_file = "/etc/wasm-node/nats.creds"
```

The NATS client automatically handles failover between servers.

---

## Quick Reference

| Task | Command |
|------|---------|
| Start NATS (dev) | `nats-server -js -m 8222` |
| Start NATS (prod) | `nats-server -c /etc/nats/nats-server.conf` |
| Check server info | `nats server info` |
| List streams | `nats stream list` |
| List KV buckets | `nats kv list` |
| Check cluster health | `nats server report jetstream` |
| Create creds | `nsc add user --name wasm-node` |
| Backup streams | `nats stream backup /backup/dir` |
| Monitor connections | `curl http://nats:8222/connz` |
