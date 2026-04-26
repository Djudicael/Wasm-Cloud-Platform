# wasm-node

Main binary crate for the Wasm Cloud Platform node.

## Overview

`wasm-node` is the core runtime of the Wasm Cloud Platform. It connects to NATS for cluster communication, deploys and manages Wasm applications, routes HTTP traffic via an embedded Pingora proxy, provides an Admin API built with Axum, runs an eBPF monitor for observability, handles cluster bootstrap and leader election, orchestrates rolling upgrades, manages database connectivity (PostgreSQL), supports hot-reloadable configuration, and coordinates graceful shutdown.

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

- **`main()`** — Single monolithic entry point (~2100 lines) handling all subsystem initialization and event loops.

## Known Issues & Improvements

### Concurrency & Deadlocks

- **`block_on` inside async runtime in eBPF callbacks** — Calling `block_on` from within a Tokio runtime context can deadlock or panic. eBPF callbacks should schedule work on the runtime instead.
- **`event_tx` channel receiver immediately dropped** — The receiver for the event channel is dropped on creation, meaning all events sent through `event_tx` go nowhere.

### Cluster Coordination

- **Leader election logic inverted** — Multiple nodes respond as leader instead of just the smallest node ID, causing split-brain scenarios.
- **Rolling upgrade cluster node list always returns only `self.node_id`** — The ordering/selection logic is broken, preventing proper upgrade orchestration.
- **`WaitForPredecessor` upgrade action not handled** — The action is logged and dropped, leaving upgrades in an inconsistent state.
- **`bootstrap_keypair` is `None` for existing nodes** — Nodes that rejoin the cluster cannot receive encrypted secrets.

### Shutdown & Lifecycle

- **`begin_graceful_shutdown` is a no-op** — The function just sleeps; it doesn't drain or stop running instances, leaving them running indefinitely.
- **`handle_state_snapshot` uses fixed 100ms sleep for artifact arrival** — This is a race condition; the artifact may not have arrived in 100ms, or the sleep may unnecessarily delay processing.

### NATS & Messaging

- **Duplicate NATS subscriptions with same consumer name** — Multiple subscriptions sharing a consumer name causes unreliable message delivery.
- **Subscription subjects ignored** — The `_subject` parameter is unused; subscriptions receive ALL messages regardless of the intended filter.

### Configuration & Routing

- **`NodeLoad` handler hardcodes supervisor address to `127.0.0.1:9000`** — The supervisor address should be configurable.
- **`SecretUpdate` handler stores encrypted value without decryption** — Secrets are stored in their encrypted form, making them unreadable by applications.
- **DNS stub resolves ALL `*.internal` domains to `127.0.0.1`** — No allowlist is enforced; any internal domain resolves to loopback.

### Code Quality

- **Massive `main()` function (~2100 lines)** — The entry point handles initialization, event loops, and business logic. It should be decomposed into modules.
- **`fetch_artifact` duplicates `upgrade::download_and_verify`** — Shared logic should be extracted into a common function.

### eBPF

- **eBPF threshold updates logged but not propagated to eBPF programs** — Configuration changes have no effect on the running eBPF monitor.

## Security Considerations

- **Admin API tokens sent over plaintext HTTP** — Authentication tokens are transmitted without TLS, allowing network-level token theft. Deploy behind a TLS-terminating proxy or enable TLS on the Admin API.
- **Token rotation endpoint returns new token in response body** — The new token is exposed in the HTTP response, which may be logged or cached by intermediaries. Avoid logging responses and restrict access to the rotation endpoint.
- **`admin/rebuild` endpoint deletes database without confirmation** — A single API call can destroy all persistent state with no safeguard. Add a confirmation mechanism or two-step deletion.
- **`admin/gc/force` kills instances for ALL apps** — Including currently deployed applications, causing unintended downtime. Scope to only orphaned or stopped instances.
- **DNS stub resolves ALL `*.internal` domains to `127.0.0.1`** — Without an allowlist, this can be exploited for DNS rebinding or to redirect traffic to unintended destinations.
- **`SecretUpdate` stores encrypted values without decryption** — This effectively corrupts secrets, making them unusable and potentially causing application failures that mask the real issue.
- **eBPF threshold updates logged but not propagated** — Threshold changes are not applied to running eBPF programs, meaning security-relevant monitoring may be operating with stale thresholds.
- **Missing keypair for existing nodes** — Nodes that have already bootstrapped have `bootstrap_keypair = None`, preventing them from receiving encrypted secrets. Keypair persistence should be implemented.
