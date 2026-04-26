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
- **Deployment**: Supports hot-swap deployments and rollback policies for zero-downtime updates.
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
| `RollbackPolicy` | Defines conditions and behavior for automatic rollback of deployments. |
| `EnvResolver` | Resolves environment variable templates for instance configuration. |
| `FuelAdmissionController` | Limits total fuel consumption per application to prevent resource exhaustion. |
| `ConcurrencyController` | Limits concurrent instance count per application using a semaphore. |
| `NodeStats` | Tracks node-level statistics (active instances, total fuel, etc.). |

## Known Issues & Improvements

### Panic & Reliability

- **`spawn()` uses `expect()` which panics on instance spawn failure** — If the runtime fails to spawn an instance, the supervisor panics instead of returning an error. This also leaks the previously allocated port.
- **`kill_instance_internal()` doesn't await `JoinHandle`** — The Wasm task continues running after the supervisor considers it killed, leading to zombie instances consuming resources.
- **`kill_instance_internal()` doesn't record billing** — Instances killed due to health check failure or idle timeout are never billed, resulting in lost revenue tracking.
- **`health_tick()` holds write lock during TCP connects** — The health check acquires a write lock on the instance map before performing blocking TCP connections, preventing all other operations (spawns, kills, lookups) from proceeding.

### Race Conditions

- **`ensure_instance()` TOCTOU between read lock and spawn** — The check-then-spawn pattern has a time-of-check-to-time-of-use gap. Under concurrent load, multiple callers can observe "no instance" and each spawn one, leading to over-provisioning.
- **`drain_app()` + `kill_all_instances()` race condition in `shutdown_all()`** — These two operations can interfere with each other, potentially causing double-kills or missed instances during shutdown.

### Resource Leaks

- **`ConcurrencyController::acquire()` semaphore grows unboundedly** — Permits are added but never removed, so the semaphore grows without bound over the lifetime of the supervisor.
- **`virtual_dns` built but never used** — A `VirtualDns` instance is constructed and immediately dropped, wasting initialization cost.

### Configuration & Hardcoding

- **`resolve_app_by_port()` hardcodes version "v1"** — The version string should be configurable or derived from the application configuration.
- **Audit log path hardcoded to `/var/log/wasm-node/audit.jsonl`** — Should be configurable via environment variable or config file.
- **`node_id()` reads env var on every call** — The `NODE_ID` environment variable is read on every invocation instead of being cached at startup.
- **`rollback()` uses `AppConfig::default_for()` instead of loading from storage** — Rollback restores a default configuration rather than the previously deployed configuration, which may not match the intended rollback state.
- **`shutdown_all()` ignores its timeout parameter** — The timeout is accepted as a parameter but never enforced, so shutdown can hang indefinitely.

### Dead Code

- **`RollbackPolicy` defined but never used** — Auto-rollback based on policy conditions is not implemented.
- **`EnvResolver` is dead code** — The supervisor uses a closure-based approach instead of the `EnvResolver` type.
- **`shutdown_rx` created but never read** — The graceful shutdown signal channel is created but never consumed, so graceful shutdown has no effect.

### Networking

- **`NetworkInterceptor` allows all non-loopback connections** — There is no egress filtering for external connections, allowing Wasm instances to reach any external host.
- **Unknown loopback connections allowed** — Connections to loopback addresses that don't match known services are permitted, potentially allowing access to other local services on the host.

## Security Considerations

### Network Isolation

The `NetworkInterceptor` does not filter egress traffic to non-loopback addresses. Wasm instances can initiate outbound connections to any external host, which may violate security policies in multi-tenant deployments. Additionally, allowing unknown loopback connections enables instances to access other services running on the host.

### Audit Logging

Audit logging uses synchronous file I/O (`std::fs::write`), which blocks the async Tokio runtime. This not only impacts performance but also creates a denial-of-service vector: if the audit log filesystem is slow or unresponsive, the entire supervisor will stall. Consider using `tokio::fs` or a dedicated logging thread.

### Instance Cleanup

`kill_instance_internal()` does not await the task `JoinHandle`, meaning Wasm instances may continue executing after the supervisor considers them terminated. This can lead to resource leaks and potential security issues if the instance retains access to network or filesystem resources.

### Billing Integrity

Instances killed via health checks or idle timeouts are not billed. This creates an incentive for malicious users to trigger kills before billing records are written, effectively receiving free compute time.

### Port Leaks

When `spawn()` fails after port allocation, the port is leaked and never returned to the `PortAllocator`. Over time, repeated spawn failures can exhaust the available port range, causing a denial-of-service condition.
````
