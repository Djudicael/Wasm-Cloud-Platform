# wasm-cloud-supervisor

## Overview

The `supervisor` crate is the core orchestration layer of the Wasm Cloud Platform. It manages the complete lifecycle of WebAssembly instances — from spawning and health-checking to scaling, draining, killing, and billing. Beyond instance management, the supervisor also handles service discovery, namespace-aware network interception, port allocation, audit logging, database connection proxying, and deployment operations including hot-swap and rollback.

## Architecture

The supervisor operates as a long-running service that coordinates multiple subsystems:

- **Instance Lifecycle**: Spawns Wasm instances via the `runtime` crate, monitors their health via periodic TCP checks, and manages graceful shutdown through drain/kill sequences.
- **Service Discovery**: Maintains local and namespace-aware registries to route requests to the correct instances.
- **Network Interception**: Intercepts and filters network connections at the namespace level, controlling egress from Wasm instances.
- **Port Allocation**: Dynamically assigns host ports to Wasm instances, tracking allocations to prevent conflicts.
- **Billing**: Records fuel and resource consumption for running instances.
- **Audit Logging**: Records significant events (spawns, kills, deployments) to a structured JSON log.
- **Connection Proxying**: Proxies database connections from Wasm instances to backend databases.
- **Deployment**: Supports hot-swap and manual rollback mechanics. `RollbackPolicy` is a model for automatic rollback thresholds but is not wired to an automatic controller.
- **Admission Control**: Uses fuel-based and concurrency-based controllers to limit resource usage per application.

### Key Dependencies

- `runtime` — Provides the Wasm execution engine and instance management primitives.
- `proxy` — Routes external traffic to supervised instances.

## Public API

### Core Types

| Type | Description |
|------|-------------|
| `Supervisor` | Main orchestrator; manages instance lifecycle, health checks, scaling, and deployment. |
| `ManagedInstance` | Represents a running Wasm instance with associated metadata (port, app, namespace, etc.). |
| `BillingInfo` | Tracks fuel consumption and runtime duration for billing purposes. |
| `PortAllocator` | Allocates and frees host ports for Wasm instance listeners. |
| `NamespaceRegistry` | Maps namespaces to their associated instances and configuration. |
| `LocalServiceRegistry` | Tracks locally running services and their endpoints. |
| `NetworkInterceptor` | Intercepts and filters network connections based on namespace policies. |
| `InstancePool` | Pool of managed instances for a given application, supporting scaling operations. |
| `ConnectionProxy` | Proxies database connections from Wasm instances to backend databases. |
| `RollbackPolicy` | Defines proposed automatic rollback thresholds; currently used as a data model and in tests, not by a monitoring loop. |
| `EnvResolver` | Resolves environment variable templates for instance configuration. |
| `FuelAdmissionController` | Limits total fuel consumption per application to prevent resource exhaustion. |
| `ConcurrencyController` | Limits concurrent instance count per application using a semaphore. |
| `NodeStats` | Tracks node-level statistics (active instances, total fuel, etc.). |

## Known Issues & Improvements

### Spawn and scaling

- `ensure_instance()` checks for a ready instance and then calls `spawn()` without a per-application single-flight guard. Concurrent cold requests can therefore initiate more than one spawn, bounded by the application's instance limits.
- Spawn error paths release the allocated port. If the process starts but fails the 500 ms readiness check, the current path releases the port without first signalling and joining the spawned task.
- `ConcurrencyController` adds semaphore permits after a successful scale-up. Those permits are not reduced when instances later stop, so its concurrency model can drift from the live pool size.

### Shutdown and accounting

Instance shutdown now fences the upstream, signals the runtime, waits for its join handle, records billing, deregisters service state, and releases the port. A timed-out task is reinserted in a fenced state and reaped after it exits.

`shutdown_all(timeout)` applies the supplied duration to each application and instance sequence rather than enforcing one global deadline. Total shutdown time can therefore exceed `timeout` when many applications are running.

### Deployment and identity

- `RollbackPolicy` describes automatic rollback thresholds but is not connected to a monitoring loop.
- `rollback()` verifies that the previous artifact exists but constructs `AppConfig::default_for()` instead of restoring that version's persisted configuration.
- `NamespaceRegistry::resolve_app_by_port()` reconstructs the discovered application with version `v1`. Namespace enforcement uses the namespace correctly, but callers must not treat that reconstructed value as the exact deployed version.

### Audit path

The general platform logging system is configurable. The supervisor's standalone `log_policy_violation()` helper still appends synchronously to `/var/log/wasm-node/audit.jsonl`; callers should avoid placing that blocking file operation on a latency-sensitive executor thread.

## Security Considerations

### Network isolation

The runtime socket policy restricts loopback destinations to the assigned application port, the internal gateway, and registered same-namespace services. External destinations are evaluated by the application's WASI network policy. Keep `allowed_cidrs`, `denied_cidrs`, DNS, and outbound protocol settings explicit for untrusted workloads.

### Instance cleanup

Shutdown waits for the task and retains timed-out instances as fenced state until a later reap. The readiness-failure path described above remains a cleanup gap and should be treated as a resource-leak risk until it explicitly stops the task.

### Billing integrity

Normal, health-triggered, idle, and operator-triggered shutdown paths converge on `finalize_instance_exit()`, which records billing before releasing resources. The billing channel is bounded; a full or closed channel produces a warning and drops that record.

### Audit durability

The supervisor emits structured audit events, but local file durability still depends on filesystem health and rotation. Production validation must exercise the configured collector and the documented collector-outage boundary.
