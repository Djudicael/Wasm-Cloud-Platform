# Step 10 — Deployment Protocol (Hot-Swap & Zero-Downtime)

## Goal
Define the exact sequence for deploying, updating, and removing applications with
**zero downtime**. The deployment protocol covers:
- First deploy
- Version upgrade (hot-swap v1 → v2)
- Rollback
- Graceful shutdown of an instance
- Emergency kill

---

## Context & Rationale

### The Problem This Solves

Deploying a new version of an app naively would mean: stop v1, start v2. During the gap
between stop and start, requests get 502 errors. For a production platform this is
unacceptable — even a 100ms gap during peak hours means lost revenue.

This step defines the exact ordered sequence of operations that achieves zero-downtime
deployment. The key insight is that the proxy's upstream table and the Supervisor's instance
pool are decoupled: you can have v1 and v2 both in the upstream pool simultaneously.

### The Blue-Green Pattern at the Instance Level

Traditional blue-green deployment uses two identical server clusters ("blue" and "green").
Traffic is moved from blue to green at the load balancer level. This step adapts the same
idea at the **instance level** within a single node:

```
Before hot-swap:
  UpstreamRegistry["api-users"] = [127.0.0.1:10100]  ← v1

During hot-swap:
  UpstreamRegistry["api-users"] = [127.0.0.1:10100,   ← v1 (finishing existing)
                                   127.0.0.1:10200]   ← v2 (taking new requests)

After hot-swap:
  UpstreamRegistry["api-users"] = [127.0.0.1:10200]  ← v2 only
```

The Pingora round-robin ensures requests naturally flow to both during the overlap window.
When v1 is removed from the pool, existing connections to v1 are allowed to complete
(HTTP keep-alive connections will drain gracefully).

### Why Versions Are Namespaced (api-users:v1, api-users:v2)

Each version is treated as an independent app in the storage layer:
- `redb["artifacts"]["api-users:v1"]` = compiled v1 artifact
- `redb["artifacts"]["api-users:v2"]` = compiled v2 artifact
- `redb["configs"]["api-users:v1"]` = v1 config
- `redb["configs"]["api-users:v2"]` = v2 config

This means **rollback is instantaneous**: the v1 artifact is still on disk, ready to
cold-start. No re-compilation, no re-download from the registry. Just:
1. Remove v2 from upstream pool
2. Spawn v1 (< 10ms cold start)
3. Traffic flows to v1

Without version namespacing, deploying v2 would overwrite v1's artifact, making rollback
require a full re-deploy from the CLI.

### The Drain Timeout Tradeoff

The drain timeout is the maximum time we wait for in-flight requests to complete before
force-killing the old instance. Choosing the right value:

- **Too short (e.g. 1s)**: Long-running requests (file uploads, slow DB queries) get killed
  mid-execution → 500 errors for users
- **Too long (e.g. 300s)**: A stuck connection (bug in v1, half-open TCP) holds up the
  deployment for 5 minutes

The default is **30 seconds**. This is long enough for typical HTTP requests (even slow
ones) and short enough to keep deployments snappy. Operators can override per-app.

### Emergency Kill vs Graceful Drain

When a Wasm module raises a Trap (out-of-fuel, OOM, illegal instruction), it has already
terminated abnormally. There is nothing to drain — the module is dead. The Supervisor
must:

1. Immediately remove the instance from the upstream pool (prevent further routing to a dead address)
2. Release the port back to the pool
3. Increment the trap counter in metrics
4. If the trap rate exceeds a threshold, **suspend the app** to prevent a rapid crash loop
   from spinning up dozens of instances that immediately die

The suspension is a circuit breaker: after N traps in M seconds, the app goes into a
SUSPENDED state and no new cold-starts are allowed until an operator re-enables it.

---

---

## 1. Deployment States

```
UNKNOWN
    │
    ▼ (deploy command received)
COMPILING
    │
    ▼ (artifact stored in redb)
IDLE            ← code is ready, no instance running (0 CPU, 0 RAM)
    │
    ▼ (first HTTP request arrives)
STARTING        ← Instance::new() called, TCP not yet bound
    │
    ▼ (TCP port responds)
RUNNING         ← receiving traffic
    │
    ├──► DRAINING  ← new version deployed, finishing existing requests
    │         │
    │         ▼
    │     STOPPED   ← instance freed, port released
    │
    └──► KILLED  ← emergency stop (OOM, trap, admin command)
```

---

## 2. First Deploy Flow

```
1. Operator/CLI sends DeployApp event to NATS
        │
        ▼
2. All nodes receive the event via NATS subscription
        │
        ▼
3. Each node: compile .wasm → store artifact in redb
   Each node: store AppConfig in redb
        │
        ▼
4. State = IDLE (no instance running yet)
        │
        ▼
5. First HTTP request hits Pingora
        │
        ▼
6. Pingora: upstream_registry.next(app_id) → None
        │
        ▼
7. Pingora calls cold_start callback
        │
        ▼
8. Supervisor: spawn() → allocate port → build WasiEnv → Instance::new()
        │
        ▼
9. wait_for_ready(addr, 500ms)
        │
        ▼
10. upstream_registry.add(app_id, addr)
11. Publish Event::InstanceReady to NATS
        │
        ▼
12. Pingora routes the (first) request to the new instance
        │
        ▼
13. Response returned to client
```

---

## 3. Hot-Swap (v1 → v2)

```
1. Operator sends DeployApp { app_id: "api-users:v2", ... }
        │
        ▼
2. Node compiles v2 and stores artifact under key "api-users:v2"
   (v1 artifact still present under "api-users:v1")
        │
        ▼
3. Config is updated: default_version = "api-users:v2"
        │
        ▼
4. v1 instances continue running and accepting traffic
        │
        ▼
5. First new request → cold-start v2 (or proactive pre-warm)
        │
        ▼
6. v2 passes health check (TCP probe)
        │
        ▼
7. upstream_registry.add("api-users", v2_addr)
   Now BOTH v1 and v2 are in the upstream pool (blue-green)
        │
        ▼
8. Traffic starts routing to both v1 and v2 (round-robin)
        │
        ▼
9. Supervisor marks v1 instances as DRAINING:
   - They stop accepting new requests (removed from upstream table)
   - They finish all in-flight requests (drain period = 30s max)
        │
        ▼
10. After drain period, v1 instances are killed:
    - shutdown_tx.send(())
    - port released
    - redb artifact for v1 optionally deleted
        │
        ▼
11. Only v2 is live. Deployment complete.
```

### Code: Hot-Swap Trigger

```rust
// crates/supervisor/src/deployment.rs
use crate::Supervisor;
use common::types::{AppId, AppConfig};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub async fn hot_swap(
    supervisor: Arc<Supervisor>,
    old_app_id: AppId,
    new_app_id: AppId,
    new_config: AppConfig,
    drain_timeout: Duration,
) -> Result<(), common::error::PlatformError> {
    info!(
        old = %old_app_id.0,
        new = %new_app_id.0,
        "starting hot-swap"
    );

    // 1. Ensure the new version is running (cold-start or pre-warm)
    let new_addr = supervisor.spawn(&new_app_id).await?;
    info!(new = %new_app_id.0, %new_addr, "new version is ready");

    // 2. Wait a moment to confirm stability (optional pre-flight)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Drain old instances (stop sending new requests to them)
    supervisor.drain_app(&old_app_id, drain_timeout).await?;

    // 4. Kill the drained old instances
    supervisor.kill_all_instances(&old_app_id).await?;

    info!(old = %old_app_id.0, "hot-swap complete");
    Ok(())
}
```

---

## 4. Graceful Drain

```rust
// crates/supervisor/src/lib.rs
impl Supervisor {
    /// Mark all instances of an app as DRAINING.
    /// Removes them from the upstream table (no new requests),
    /// then waits for in-flight requests to complete.
    pub async fn drain_app(
        &self,
        app_id: &AppId,
        timeout: Duration,
    ) -> Result<(), common::error::PlatformError> {
        // Remove from Pingora's upstream table immediately
        // (Pingora will stop routing new requests to this app's old instances)
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                for addr in pool.ready_addrs() {
                    self.upstream_registry.remove(app_id, &addr).await;
                }
            }
        }

        // Wait for in-flight requests to drain
        // Proxy-side: Pingora will wait for existing connections to close naturally.
        // We give a hard deadline.
        tokio::time::sleep(timeout).await;
        Ok(())
    }

    /// Kill all instances of an app immediately.
    pub async fn kill_all_instances(&self, app_id: &AppId) -> Result<(), common::error::PlatformError> {
        let mut pools = self.pools.write().await;
        if let Some(pool) = pools.get_mut(&app_id.0) {
            let instances = std::mem::take(&mut pool.instances);
            for inst in instances {
                if let common::types::InstanceState::Ready { addr } | 
                       common::types::InstanceState::Starting = &inst.state {
                    // Already removed from upstream above in drain_app
                    self.port_alloc.release(inst.addr.port());
                }
                inst.shutdown_tx.send(()).ok();
            }
        }
        Ok(())
    }
}
```

---

## 5. Rollback

A rollback is just a hot-swap in reverse: deploy the previous version.

### When to Rollback

Rollback is triggered in two ways:

**Manual rollback** — The operator observes a problem (error spike, latency regression) and
explicitly rolls back via CLI:

```
wasm-ctl rollback --app api-users --to v1
```

**Automatic rollback** — The Supervisor detects that the new version is unhealthy and
automatically reverts. The trigger conditions:

1. **Trap rate threshold**: If the new version produces > 50% traps within the first 30
   seconds of deploy, it is considered broken. The Supervisor reverts to the previous version
   without operator intervention.
2. **Health check failure**: If the new version's instances fail 3 consecutive TCP health
   probes (15 seconds total), rollback is triggered.
3. **No successful requests**: If the new version serves zero successful (2xx) responses
   within the first 60 seconds, it is presumed broken.

Automatic rollback is a **safety net**, not a replacement for testing. It catches deployments
that crash immediately or fail catastrophically.

```rust
// crates/supervisor/src/deployment.rs
use std::time::{Duration, Instant};

/// Configuration for automatic rollback detection.
pub struct RollbackPolicy {
    /// If trap rate exceeds this fraction within the observation window, rollback.
    /// Default: 0.50 (50% of requests trap).
    pub trap_rate_threshold: f64,

    /// How long to observe the new version before declaring it stable.
    /// Default: 30 seconds.
    pub observation_window: Duration,

    /// Number of consecutive health check failures before rollback.
    /// Default: 3 (= 15 seconds at 5s health tick interval).
    pub health_failure_threshold: u32,

    /// If true, automatic rollback is enabled. Operators can disable per-app.
    pub auto_rollback_enabled: bool,
}

impl Default for RollbackPolicy {
    fn default() -> Self {
        RollbackPolicy {
            trap_rate_threshold: 0.50,
            observation_window: Duration::from_secs(30),
            health_failure_threshold: 3,
            auto_rollback_enabled: true,
        }
    }
}
```

### Rollback Execution

```rust
// CLI / Control Plane
pub async fn rollback(
    supervisor: Arc<Supervisor>,
    current: AppId,
    previous: AppId,
    drain_timeout: Duration,
) -> Result<(), common::error::PlatformError> {
    // Verify the previous artifact still exists in redb
    if !supervisor.store().artifact_exists(&previous)? {
        return Err(common::error::PlatformError::AppNotFound(
            format!("rollback target {} not found — artifact may have been garbage collected", previous.0)
        ));
    }

    // The previous artifact is still in redb (we don't delete old versions immediately)
    hot_swap(supervisor, current, previous, /* config */ Default::default(), drain_timeout).await
}
```

### Artifact Retention Policy

**Retention policy**: Keep the last 3 compiled artifacts per app in `redb`.
When a fourth version is deployed, the oldest is pruned (see step 26 for full GC details).

The retention count of 3 ensures:
- **Current version** (v3) — actively serving traffic
- **Previous version** (v2) — immediate rollback target, artifact is warm in redb
- **Two-back version** (v1) — safety net if v2 was also bad (deployed in error)

Artifacts currently serving traffic (active instances) are **never** garbage collected,
regardless of the retention count. The GC system checks the Supervisor's instance pool
before deleting any artifact.

### Rollback Window

The **rollback window** is the time during which a rollback is possible. It is bounded by
two factors:

1. **Artifact GC**: Once GC removes the old artifact (step 26), rollback requires a full
   re-deploy from the CLI. Default GC keeps 3 versions, so the window is at least 2 deploys.
2. **Config drift**: If the new version's config is incompatible with the old version's code
   (e.g., a new required env var), rolling back will produce runtime errors. Operators must
   ensure config backward compatibility when deploying.

```rust
// crates/storage/src/artifact.rs
impl Store {
    /// Enforce max N versions. Deletes oldest when exceeded.
    pub fn prune_old_versions(&self, app_name: &str, keep: usize) -> Result<(), PlatformError> {
        let prefix = format!("{app_name}:");
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(crate::tables::ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut versions: Vec<String> = table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
            .filter_map(|e| e.ok())
            .filter(|(k, _)| k.value().starts_with(&prefix))
            .map(|(k, _)| k.value().to_string())
            .collect();

        versions.sort(); // Assumes version suffix is lexicographically ordered (v1, v2, v10...)
        let to_delete: Vec<_> = versions.into_iter().rev().skip(keep).collect();

        drop(table);
        drop(tx);

        for key in to_delete {
            let id = AppId(key);
            self.delete_artifact(&id)?;
        }
        Ok(())
    }
}
```

---

## 6. Emergency Kill

Immediately terminates an instance due to an out-of-fuel trap, OOM, or admin command.

```rust
// crates/supervisor/src/lib.rs
impl Supervisor {
    /// Called when Wasmer raises a Trap (OOM / out of fuel / illegal instruction).
    pub async fn handle_trap(
        &self,
        app_id: &AppId,
        instance_id: &InstanceId,
        reason: &str,
    ) {
        tracing::error!(
            app = %app_id.0,
            instance = %instance_id.0,
            reason,
            "Wasm trap — killing instance"
        );

        // 1. Kill the instance
        self.kill_instance(app_id, instance_id).await.ok();

        // 2. Increment trap counter in metrics
        // (handled by metrics module — see step 11)

        // 3. If trap rate exceeds threshold, suspend the app
        // (see step 12: scaling)
    }
}
```

---

## 7. Deploy Command CLI Schema

```
wasm-node deploy \
  --app-name  api-users \
  --version   v2 \
  --wasm-file ./target/wasm32-wasip2/release/api-users.wasm \
  --fuel      500000000 \
  --memory-mb 128 \
  --env       PORT=8080 \
  --env       LOG_LEVEL=info \
  --secret    DATABASE_URL \
  --secret    JWT_SECRET
```

This CLI:
1. Reads the `.wasm` file
2. Constructs an `Event::DeployApp`
3. Publishes it to NATS (`deploy.app.new`)
4. All nodes in the cluster receive and process it

---

## Completion Checklist

**This step is done when all boxes are checked.**

### First Deploy
- [ ] Publishing `Event::DeployApp` causes every node to compile and store the artifact in redb
- [ ] After deploy, `store.artifact_exists(app_id)` returns `true` on every node
- [ ] The first HTTP request to the app triggers a cold start in < 500ms
- [ ] The deploy does not cause any downtime for other apps already running

### Hot-Swap
- [ ] Deploying v2 while v1 is running adds v2 to the upstream pool alongside v1
- [ ] After `drain_app(v1)`, no new requests are routed to v1 instances
- [ ] v1 instances finish their in-flight requests before being killed
- [ ] After hot-swap completes, only v2 addresses are in the upstream pool
- [ ] Zero HTTP 5xx errors are observed during a hot-swap under constant traffic

### Rollback
- [ ] The v1 artifact remains in redb after v2 is deployed (not automatically deleted)
- [ ] A rollback event (re-deploy v1) makes v1 traffic-ready within 500ms cold start
- [ ] `prune_old_versions(app_name, keep=3)` retains the 3 most recent versions and deletes older ones
- [ ] Artifacts with active instances are never garbage collected
- [ ] Automatic rollback triggers when trap rate > 50% within the first 30s of a new version
- [ ] Automatic rollback triggers when 3 consecutive health checks fail for the new version
- [ ] Rollback to a GC'd artifact returns a clear error message (not a panic)
- [ ] `wasm-ctl rollback --app X --to vN` works for any retained version

### Graceful Drain
- [ ] `drain_app(timeout)` removes the app from the upstream registry immediately
- [ ] In-flight requests complete before the timeout expires (under normal load)
- [ ] If in-flight requests are not done by the timeout, `kill_all_instances()` is called anyway

### State Transitions
- [ ] An app moves through states: IDLE → STARTING → RUNNING (no skipped states)
- [ ] A trap or OOM immediately moves the instance to KILLED and the error is logged

### Tests
- [ ] A test deploys an app, sends 10 requests, verifies all return 200
- [ ] A test performs a hot-swap while sending concurrent requests and counts 0 failures
- [ ] A test verifies that `prune_old_versions` deletes exactly the right artifacts
