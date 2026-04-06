# Step 07 — Supervisor Core (Instance Lifecycle Manager)

## Goal
The Supervisor is the beating heart of the node. It:

---

## Context & Rationale

### The Problem This Solves

Without a supervisor, spawning a Wasm instance would be a one-shot operation: compile,
run, done. But for a cloud platform we need:

- **Persistence**: instances stay up and accept many HTTP requests over their lifetime
- **Pooling**: multiple instances of the same app can run concurrently for parallelism
- **Recovery**: crashed instances are detected and replaced
- **Efficiency**: idle instances are cleaned up to free memory
- **Coordination**: Pingora must know which instances are alive and ready

The Supervisor is the component that owns this entire lifecycle. It is the only place
in the codebase that creates or destroys Wasm instances.

### Why Wasm Instances Run on a Blocking Thread

A Wasm module compiled to `wasm32-wasip2` with Tokio internally runs its own async runtime
inside the Wasm sandbox. This async runtime blocks the OS thread it runs on (Tokio's
`#[tokio::main]` calls `Runtime::block_on()` which parks the thread).

If this blocking call were made on a Tokio async task directly, it would starve the
executor — one blocked task occupies a worker thread forever. `tokio::task::spawn_blocking`
moves it to a dedicated thread pool that Tokio sets aside exactly for blocking work.

```
Tokio executor threads (non-blocking):
  Thread 1: NATS messages, HTTP connections, health loop ticks
  Thread 2: Pingora proxy routing, metrics aggregation
  Thread 3: Supervisor coordination, port allocation

Tokio blocking thread pool:
  Blocking-1: Wasm instance for "api-users:v2" (running Axum, blocking)
  Blocking-2: Wasm instance for "api-users:v2" (second instance)
  Blocking-3: Wasm instance for "payments:v1"
  Blocking-4: AOT compilation of new .wasm binary
```

This design lets the node handle thousands of events per second on its async threads
while dozens of Wasm instances run concurrently on blocking threads.

### Why a Two-Phase Spawn (spawn → wait_for_ready)?

After `spawn_blocking` starts the Wasm instance, there is a window where the Tokio
runtime inside Wasm is booting up and the TCP listener is not yet accepting connections.
If Pingora receives a request during this window and tries to connect, it will get a
`Connection refused` error.

`wait_for_ready` (step 04) polls the TCP port every 5ms and only returns when a connection
succeeds. Only after this point does the Supervisor register the instance in the upstream
table. This guarantees Pingora will never route to an instance that isn't ready.

```
spawn_blocking called
        │
        ▼  (< 1ms: artifact deserialization + WASI env setup)
Wasm Tokio runtime starts inside Wasm module
        │
        ▼  (< 5ms: Axum binds TCP port)
wait_for_ready polls port every 5ms
        │
        ▼  (port accepts connection)
upstream_registry.add() called  ← Pingora can now route here
event_tx.send(InstanceReady)    ← Other nodes learn about this instance
```

### The Hot-Standby Pool vs On-Demand Spawning

The `InstancePool` can maintain **hot-standby instances** — Wasm modules that are already
running and waiting for their first request. This eliminates cold-start latency entirely
for apps that need it.

The default behavior is **on-demand**: spawn only when a request arrives with no available
instances. This is the right default because:

- Most apps receive bursty traffic; keeping warm instances for quiet apps wastes memory
- Cold start is < 10ms, which is acceptable for most use cases
- Operators can override with `min_instances > 0` in AppConfig for latency-sensitive apps

### Why the Health Loop Runs Every 5 Seconds

The health loop serves two purposes:
1. **Detect dead instances**: A Wasm module that crashed (OOM, fuel exhaustion, panic)
   will no longer accept TCP connections. The health loop discovers this by probing the port.
2. **Prune idle instances**: An instance that has not received a request in `idle_timeout_secs`
   is consuming memory and a port for no benefit. The prune removes it.

5 seconds is the right interval because:
- Faster polling wastes CPU (a TCP probe per instance per second adds up at scale)
- Slower polling means a dead instance sits in the upstream table for too long,
  causing 502 errors for users whose requests hit the dead instance

### State Restoration on Node Restart

When the node restarts (e.g. after a deploy of the node binary itself), all previously
deployed Wasm apps are still in redb. The Supervisor calls `restore_from_storage()` which:

1. Reads all app configs from redb
2. Loads and deserializes the compiled artifact for each (but does NOT spawn instances)
3. The apps move to IDLE state — ready to cold-start on the first request

This means:
- The node is ready to serve traffic within seconds of restart (no re-compilation needed)
- No instances are wasted on apps that may receive no traffic after restart
- The NATS JetStream replay (step 19) handles the case where new deployments happened
  while the node was down

---
1. Maintains a pool of **hot-standby** instances (compiled, ready to run)
2. **Spawns** new instances in response to demand from Pingora
3. **Monitors** health via TCP probes
4. **Prunes** idle instances to reclaim memory
5. **Scales** horizontally by reporting load to NATS
6. Communicates ready/dead instances to Pingora's upstream table

---

## 1. Core Data Structures

```rust
// crates/supervisor/src/instance.rs
use common::types::{AppConfig, AppId, FuelQuota, InstanceId, InstanceState};
use runtime::executor::{ExecutionStats, PreparedModule, RunningInstance};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio::sync::oneshot;

/// A live Wasm instance managed by the Supervisor.
pub struct ManagedInstance {
    pub id: InstanceId,
    pub app_id: AppId,
    pub addr: SocketAddr,
    pub state: InstanceState,
    pub spawned_at: Instant,
    pub last_request_at: Instant,
    pub request_count: u64,

    /// Handle to the Tokio task running the Wasm module.
    pub task: JoinHandle<ExecutionStats>,

    /// Send a signal to this handle to begin graceful shutdown.
    pub shutdown_tx: oneshot::Sender<()>,
}
```

---

## 2. Instance Pool

Each app gets a `InstancePool` that manages its instances.

```rust
// crates/supervisor/src/pool.rs
use crate::instance::ManagedInstance;
use common::types::{AppConfig, AppId, InstanceId};
use runtime::executor::PreparedModule;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

pub struct InstancePool {
    pub config: AppConfig,
    pub prepared: Arc<PreparedModule>,
    pub instances: Vec<ManagedInstance>,
}

impl InstancePool {
    pub fn active_count(&self) -> usize {
        self.instances.iter()
            .filter(|i| matches!(i.state, common::types::InstanceState::Ready { .. }))
            .count()
    }

    pub fn ready_addrs(&self) -> Vec<SocketAddr> {
        self.instances.iter()
            .filter_map(|i| match &i.state {
                common::types::InstanceState::Ready { addr } => Some(*addr),
                _ => None,
            })
            .collect()
    }

    pub fn idle_instance_ids(&self, idle_secs: u64) -> Vec<InstanceId> {
        let now = Instant::now();
        self.instances.iter()
            .filter(|i| {
                matches!(i.state, common::types::InstanceState::Ready { .. })
                    && now.duration_since(i.last_request_at).as_secs() > idle_secs
            })
            .map(|i| i.id.clone())
            .collect()
    }
}
```

---

## 3. Supervisor Main Structure

```rust
// crates/supervisor/src/lib.rs
pub mod instance;
pub mod pool;
pub mod port_alloc;
pub mod env_resolver;
pub mod network;

use crate::{
    pool::InstancePool,
    port_alloc::PortAllocator,
    network::LocalServiceRegistry,
    env_resolver::EnvResolver,
};
use common::{error::PlatformError, types::{AppId, AppConfig, InstanceId}};
use runtime::{WasmRuntime, executor::PreparedModule};
use storage::Store;
use proxy::upstream::UpstreamRegistry;
use messaging::events::Event;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn, error};

pub struct Supervisor {
    store: Store,
    runtime: WasmRuntime,
    port_alloc: Arc<PortAllocator>,
    upstream_registry: Arc<UpstreamRegistry>,
    service_registry: Arc<LocalServiceRegistry>,
    env_resolver: Arc<dyn Fn(&AppConfig, u16) -> Vec<(String, String)> + Send + Sync>,

    /// Map of app_id → instance pool
    pools: Arc<RwLock<HashMap<String, InstancePool>>>,

    /// Channel to publish events to NATS
    event_tx: mpsc::Sender<Event>,
}
```

---

## 4. Spawning an Instance

```rust
// crates/supervisor/src/lib.rs (impl Supervisor)
impl Supervisor {
    /// Spawn a new instance for the given app.
    /// Returns the SocketAddr where the instance is listening.
    pub async fn spawn(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        let config = self.store.load_config(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

        // 1. Load or compile the artifact
        let artifact = self.store.load_artifact(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(
                format!("no artifact for {}", app_id.0)
            ))?;

        // 2. Prepare the module (near-instant, just a deserialization)
        let prepared = self.runtime.prepare(&artifact, config.clone())?;

        // 3. Allocate a host port
        let host_port = self.port_alloc.allocate()?;
        let addr = self.port_alloc.socket_addr(host_port);

        // 4. Resolve env vars (static config + secrets)
        let env_vars = (self.env_resolver)(&config, host_port);

        // 5. Spawn the Wasm instance on a dedicated thread
        //    (Wasm runs a blocking Tokio runtime internally)
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let port_alloc = self.port_alloc.clone();
        let upstream = self.upstream_registry.clone();
        let svc_reg = self.service_registry.clone();
        let app_id_clone = app_id.clone();

        let task = tokio::task::spawn_blocking(move || {
            let mut instance = prepared
                .spawn_instance(env_vars, config.wasm_bind_port)
                .expect("failed to spawn instance");

            // The run() call blocks until the Wasm module exits or is killed
            let stats = instance.run();
            tracing::info!(
                app = %app_id_clone.0,
                fuel_consumed = stats.fuel_consumed,
                ram_bytes = stats.ram_bytes,
                "instance exited"
            );
            stats
        });

        // 6. Wait for the TCP port to be ready (up to 500ms)
        crate::instance::wait_for_ready(addr, Duration::from_millis(500)).await?;

        // 7. Register with the proxy upstream table
        self.upstream_registry.add(app_id, addr).await;

        // 8. Register with local service registry (for East-West routing)
        self.service_registry.register(app_id, addr).await;

        // 9. Publish READY event to NATS
        self.event_tx.send(Event::InstanceReady {
            app_id: app_id.clone(),
            addr,
            node_id: self.node_id(),
        }).await.ok();

        info!(app = %app_id.0, %addr, "instance ready");
        Ok(addr)
    }

    fn node_id(&self) -> String {
        // Read from config or environment
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }
}
```

---

## 5. Health Loop (Prune + Scale Signal)

A background Tokio task that runs every 5 seconds.

```rust
// crates/supervisor/src/lib.rs (health loop)
impl Supervisor {
    /// Start the background health monitoring loop.
    pub fn start_health_loop(self: Arc<Self>) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                if let Err(e) = supervisor.health_tick().await {
                    error!(error = %e, "health tick failed");
                }
            }
        });
    }

    async fn health_tick(&self) -> Result<(), PlatformError> {
        let pools = self.pools.read().await;
        for (app_id_str, pool) in pools.iter() {
            let app_id = AppId(app_id_str.clone());

            // 1. Probe each instance's TCP port
            for inst in &pool.instances {
                if let InstanceState::Ready { addr } = &inst.state {
                    let alive = tokio::net::TcpStream::connect(addr).await.is_ok();
                    if !alive {
                        warn!(app = app_id_str, %addr, "instance not responding, marking dead");
                        // Will be removed in next prune cycle
                    }
                }
            }

            // 2. HTTP-level health check (optional, per-app)
            //    TCP probes detect dead sockets. HTTP probes detect deadlocked or
            //    stuck instances — the TCP socket is still open but the app is not
            //    serving responses. If the app config defines a health_check_path,
            //    the Supervisor sends an HTTP GET and expects a 2xx within 2 seconds.
            if let Some(ref health_path) = pool.config.health_check_path {
                for inst in &pool.instances {
                    if let InstanceState::Ready { addr } = &inst.state {
                        let healthy = http_health_check(addr, health_path).await;
                        if !healthy {
                            warn!(
                                app = app_id_str, %addr,
                                "HTTP health check failed, marking unhealthy"
                            );
                        }
                    }
                }
            }

            // 3. Prune idle instances
            let idle_ids = pool.idle_instance_ids(pool.config.idle_timeout_secs);
            for id in idle_ids {
                self.kill_instance(&app_id, &id).await.ok();
            }
        }
        Ok(())
    }
}

/// HTTP-level health check. Sends a GET request to the instance's health endpoint.
/// Returns true if the response is 2xx within the timeout.
/// Falls back to true (healthy) on HTTP client errors to avoid false positives.
async fn http_health_check(addr: &std::net::SocketAddr, path: &str) -> bool {
    let url = format!("http://{}{}", addr, path);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();

    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            // Timeout or connection error = unhealthy
            tracing::debug!(url = %url, error = %e, "HTTP health check failed");
            false
        }
    }
}

    /// Gracefully stop an instance.
    pub async fn kill_instance(&self, app_id: &AppId, id: &InstanceId) -> Result<(), PlatformError> {
        let mut pools = self.pools.write().await;
        let pool = pools.get_mut(&app_id.0)
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

        if let Some(pos) = pool.instances.iter().position(|i| i.id == *id) {
            let inst = pool.instances.remove(pos);
            // Deregister from proxy and service registry
            if let InstanceState::Ready { addr } = &inst.state {
                self.upstream_registry.remove(app_id, addr).await;
                self.service_registry.deregister(app_id, addr).await;
                self.port_alloc.release(addr.port());
            }
            // Signal the Wasm task to stop
            inst.shutdown_tx.send(()).ok();
            tracing::info!(app = %app_id.0, instance = %id.0, "instance killed");
        }
        Ok(())
    }
}
```

---

## 6. Demand-Driven Spawn (Scale-up)

Pingora calls this when it receives a request for an app with no available instances.

```rust
impl Supervisor {
    /// Ensure at least one instance is available for the given app.
    /// Returns immediately if an instance is already running.
    pub async fn ensure_instance(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
        {
            // Fast path: check if an instance is already ready
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&app_id.0) {
                let addrs = pool.ready_addrs();
                if !addrs.is_empty() {
                    // Simple round-robin: pick the first one
                    return Ok(addrs[0]);
                }
            }
        }
        // Slow path: no instance running, spawn one now
        info!(app = %app_id.0, "cold start: spawning new instance");
        self.spawn(app_id).await
    }

    /// Proactively scale up: spawn an additional instance if load is high.
    pub async fn maybe_scale_up(&self, app_id: &AppId) -> Result<(), PlatformError> {
        let pool_info = {
            let pools = self.pools.read().await;
            pools.get(&app_id.0).map(|p| (p.active_count(), p.config.max_instances))
        };
        if let Some((active, max)) = pool_info {
            if active < max as usize {
                self.spawn(app_id).await?;
            }
        }
        Ok(())
    }
}
```

---

## 7. Startup: Restore State from redb

On node restart, the Supervisor re-prepares all deployed apps (but doesn't spawn them;
they start on first request = true Serverless cold start).

```rust
impl Supervisor {
    pub async fn restore_from_storage(&self) -> Result<(), PlatformError> {
        let app_ids = self.store.list_apps()?;
        let mut pools = self.pools.write().await;

        for app_id in app_ids {
            let config = self.store.load_config(&app_id)?
                .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;

            if self.store.artifact_exists(&app_id)? {
                info!(app = %app_id.0, "restored app from storage (waiting for first request)");
                pools.insert(app_id.0.clone(), InstancePool {
                    config,
                    prepared: Arc::new(self.get_prepared(&app_id).await?),
                    instances: Vec::new(),
                });
            } else {
                warn!(app = %app_id.0, "no compiled artifact found, skipping");
            }
        }
        Ok(())
    }

    async fn get_prepared(&self, app_id: &AppId) -> Result<PreparedModule, PlatformError> {
        let config = self.store.load_config(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        let artifact = self.store.load_artifact(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(format!("no artifact: {}", app_id.0)))?;
        self.runtime.prepare(&artifact, config)
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Instance Lifecycle
- [ ] `spawn(app_id)` loads the artifact from redb, prepares the module, allocates a port, and returns a `SocketAddr`
- [ ] `spawn()` fails with `AppNotFound` when the app has no artifact in redb
- [ ] The allocated port is released back to the pool if `spawn()` fails at any point after allocation
- [ ] `wait_for_ready()` is called inside `spawn()` — the returned address is guaranteed to accept connections
- [ ] `kill_instance()` removes the instance from the upstream registry, releases the port, and signals shutdown

### Instance Pool
- [ ] `active_count()` returns the number of instances in `Ready` state
- [ ] `ready_addrs()` returns only addresses of `Ready` instances
- [ ] `idle_instance_ids()` returns instances that have not received a request in `idle_timeout_secs`

### Health Loop
- [ ] `start_health_loop()` runs as a background Tokio task that never panics
- [ ] Instances that fail the TCP health probe are removed from the pool
- [ ] Instances that fail the HTTP health check (when `health_check_path` is configured) are marked unhealthy
- [ ] HTTP health check times out after 2 seconds and treats timeout as unhealthy
- [ ] Apps without `health_check_path` fall back to TCP-only probes (backward compatible)
- [ ] Idle instances beyond the timeout are killed automatically
- [ ] The health loop runs every 5 seconds without drifting or accumulating

### Scaling
- [ ] `ensure_instance()` returns an existing address if one is already ready (fast path, no spawn)
- [ ] `ensure_instance()` spawns a new instance and returns its address if none are ready (cold start)
- [ ] `maybe_scale_up()` spawns an additional instance only if `active_count < max_instances`
- [ ] `maybe_scale_up()` does nothing if already at `max_instances`

### State Restore
- [ ] `restore_from_storage()` reads all app IDs from redb and pre-loads `PreparedModule` for each
- [ ] Apps with missing artifacts are skipped with a warning (not a crash)
- [ ] After restore, `ensure_instance()` can cold-start any restored app within 10ms

### NATS Publishing
- [ ] `spawn()` publishes `Event::InstanceReady` to NATS after the port is confirmed ready
- [ ] `kill_instance()` publishes `Event::InstanceDead` to NATS

### Tests
- [ ] A test spawns an instance, verifies the TCP port is open, then kills it and verifies the port is closed
- [ ] A test verifies that `idle_instance_ids()` correctly identifies instances past the timeout
- [ ] A test verifies that `ensure_instance()` returns immediately when an instance is already ready
