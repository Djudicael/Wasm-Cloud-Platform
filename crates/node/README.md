# wasm-node

Main binary crate for the Wasm Cloud Platform node.

## Overview

`wasm-node` is the core runtime of the Wasm Cloud Platform. It connects to NATS for cluster communication, deploys and manages Wasm applications, routes HTTP traffic via an embedded Pingora proxy, provides an Admin API built with Axum, runs an eBPF monitor for observability, handles cluster bootstrap and leader election, orchestrates rolling upgrades, manages database connectivity (PostgreSQL), supports hot-reloadable configuration, and coordinates graceful shutdown.

This binary targets Linux production environments. Windows is not a production target for the platform.

## Current Deployment Guidance

For the current audited deployment posture, use the graded operator guide index:

- [`docs/deployment-levels.md`](../../docs/deployment-levels.md)

That index points to one separate operator file per level:

- local development: Level 0
- internal single-node deployment: Level 1
- first real Linux production rollout: Level 2
- serious multi-node production: Level 3
- strongest currently supported posture: Level 4

For current production guidance, prefer that document over older historical notes in this file.

### Vault Transit seal root

Production nodes may use a pinned non-exportable Vault Transit HMAC root. A
private Vault PKI must be configured with `runtime.key_vault_ca_cert`,
`WASM_NODE_RUNTIME_KEY_VAULT_CA_CERT`, or `--key-vault-ca-cert`; certificate
hostname/SAN validation remains enabled. Vault's real
`vault:vN:<base64>` HMAC response and legacy hex fixtures are accepted only
when they decode to exactly 32 bytes. The node context is HMAC input for domain
separation, not Vault's `derived=true` key feature. See the
[production lifecycle](../../INFRA_IMPL/process/PRODUCTION_SECRET_LIFECYCLE.md)
and [real Vault microVM runbook](../../INFRA_IMPL/process/VAULT_TRANSIT_MICROVM_VALIDATION.md).

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    wasm-node                         │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌───────────────────┐ │
│  │  NATS    │  │  Admin   │  │  Pingora Proxy    │ │
│  │  Client  │  │  API     │  │  (HTTP routing)   │ │
│  └────┬─────┘  └────┬─────┘  └────────┬──────────┘ │
│       │             │                 │             │
│  ┌────▼─────────────▼─────────────────▼──────────┐  │
│  │              App Supervisor                    │  │
│  │  (Wasm instance lifecycle, scaling, routing)   │  │
│  └───────────────────┬───────────────────────────┘  │
│                      │                              │
│  ┌───────────┐  ┌────▼─────┐  ┌──────────────────┐ │
│  │  eBPF     │  │ Database │  │  Config          │ │
│  │  Monitor  │  │ (PG)     │  │  (hot-reload)    │ │
│  └───────────┘  └──────────┘  └──────────────────┘ │
│                                                     │
│  ┌──────────────────────────────────────────────┐   │
│  │  Cluster: Bootstrap, Leader Election,        │   │
│  │  Rolling Upgrades, State Snapshots           │   │
│  └──────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

### Subsystems

| Subsystem | Responsibility |
|-----------|---------------|
| NATS Client | Cluster communication, event streaming, subscriptions |
| Admin API | Management endpoints (Axum), authentication, token rotation |
| Pingora Proxy | HTTP traffic routing to Wasm application instances |
| App Supervisor | Wasm instance lifecycle, scaling, health checks |
| eBPF Monitor | Kernel-level observability and anomaly detection |
| Database | PostgreSQL connectivity for persistent state |
| Config | Hot-reloadable configuration from files/NATS |
| Cluster | Bootstrap, leader election, rolling upgrades, state snapshots |
| DNS Stub | Resolves `*.internal` domains for inter-app communication |
| Secret Management | Receives and stores encrypted secrets from the cluster |

## Public API

This crate produces the `wasm-node` binary. It is not intended to be used as a library.

### Key Entry Points

- **`main()`** — Binary entry point that assembles configuration, security admission, storage, messaging, supervisor, proxy, eBPF, health, and admin services. Event handling and several startup concerns have been extracted into modules, although `main.rs` remains large.

## Current Status and Remaining Improvements

### Cluster coordination

Durable consumers use distinct names and explicit subject filters. Bootstrap snapshots carry a session identifier and nonce, accept only the first matching response, and use signed artifact-fetch authorizations. Rolling upgrades derive the active node list from the persisted cluster registry and handle predecessor ordering.

The upgrade handler records `WaitForPredecessor` and relies on subsequent control-plane progress; operators must validate the complete rolling-upgrade and rollback workflow before production promotion.

### Shutdown and lifecycle

Node drain delegates to the supervisor's fenced, joined shutdown path. The administrative rebuild endpoint quarantines the local database and requires a restart to rebuild from cluster state. It is an authenticated write operation but has no additional interactive confirmation, so write-token custody is critical.

Forced GC purges undeployed applications and kills instances only for applications without active routes. This route-based definition means operators should remove stale routes before expecting GC to reclaim an undeployed workload.

### Internal DNS and mesh

The embedded authoritative stub intentionally maps every A query ending in `.internal` to loopback. It does not prove that an application exists. The internal gateway and supervisor enforce the actual namespace and local-service policy. Applications must use literal `<app>.<namespace>.internal` names, `placement.policy = "every_node"`, and same-namespace `local_dependencies`; there is no remote-node fallback for this mesh.

### eBPF configuration

Hot-reload applies memory and disk thresholds both to the userspace dispatcher and to loaded kernel CONFIG maps. The separate admin `/admin/ebpf/config` JSON endpoint still logs an arbitrary `thresholds` object without applying it; use the supported hot-config keys or `wasm-ctl node ebpf-config` actions described in the operator guide.

### Maintainability

`main.rs` still coordinates a large startup graph and should continue to be decomposed behind tested module boundaries. Artifact download and verification logic exists in both deployment and binary-upgrade paths with different authorization contracts; shared byte-fetch mechanics must not collapse those security boundaries.

## Security Considerations

- Production admission requires admin authentication, TLS material, strong tokens, a durable storage path, secure NATS settings, and the configured artifact-transfer controls.
- The token-rotation endpoint returns the newly generated token to its authenticated caller and is disabled in production mode. Production rotation belongs in the external secret-management workflow.
- `/admin/rebuild` quarantines local state. Limit the write token, audit its use, and confirm another healthy node can supply authoritative state before invoking it.
- The DNS stub is only name resolution. Namespace isolation is enforced by the internal gateway and runtime socket policy, so bypass paths and local dependency placement remain part of production validation.
- Secret updates are encrypted separately for each node's persisted X25519 transport key before they are published through NATS.
- eBPF fallback is reduced monitoring. When production configuration requires eBPF, node startup or runtime degradation must fail the corresponding readiness contract.
