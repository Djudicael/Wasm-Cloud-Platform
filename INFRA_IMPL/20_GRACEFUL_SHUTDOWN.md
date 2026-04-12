# Step 20 — Graceful Shutdown of Wasm Instances

## Goal
When the Supervisor decides to stop an instance (idle timeout, hot-swap drain, node shutdown),
the Axum server inside the Wasm module must:

---

## Context & Rationale

### The Problem This Solves

Stopping a native Linux process cleanly is straightforward: `SIGTERM` → Tokio catches
it → Axum drains in-flight requests → process exits. This is well-understood behavior.

Stopping a Wasm instance is harder. The Wasm module runs **inside** the node process —
there is no separate process boundary, no signal delivery mechanism, and no OS-level
way to tell it to "stop accepting new connections and finish up."

Without a graceful shutdown mechanism, the only option is `JoinHandle::abort()`, which
is a hard kill. Every in-flight request inside that instance at the moment of abort gets
a broken connection → the user sees a connection reset error → 500 or no response.

This step implements three mechanisms so operators can choose the right trade-off between
implementation complexity and graceful behavior.

### Why TCP Close Works (and Why It's the Default)

When the Supervisor pre-binds a socket and passes it to the Wasm module via WASI preopened
sockets (step 04), it holds the `TcpListener` in the `InstanceHandle`.

Dropping (closing) that `TcpListener` fd causes the accept loop inside the Wasm module
to fail on the next `accept()` call. Axum detects this and initiates its built-in graceful
shutdown — it stops accepting new connections and waits for in-flight requests to complete.

This works with zero changes to the Wasm app code. The Wasm app behaves correctly because
Axum is designed to handle this: its `with_graceful_shutdown()` hook polls for a shutdown
signal, and a closed listener is one such signal.

### Why the HTTP Shutdown Endpoint is Better for State-Ful Apps

Some apps need to do cleanup on shutdown:
- Flush a write-back cache to the database
- Commit a pending transaction
- Save progress to a file

The `POST /_platform/shutdown` endpoint lets the app hook into shutdown and run arbitrary
cleanup code. The Supervisor sends the HTTP request, the app's shutdown handler runs its
cleanup, then `axum::serve` returns.

This is analogous to a `PreStop` hook in Kubernetes. The difference is that here it's
implemented by the app itself, not by the platform.

### Why Three Mechanisms (not One)

No single mechanism works for all cases:
- TCP close: requires pre-bound socket (always available) but gives no cleanup window
- HTTP endpoint: requires the app to opt in (extra code) but allows cleanup
- Hard abort: no cleanup at all, but guaranteed to terminate even a stuck app

The recommended strategy for the platform is to **try mechanisms in order** with a
fallback chain: HTTP shutdown → TCP close → hard abort. This ensures:
1. Apps that implement `/_platform/shutdown` get a clean exit
2. Apps that don't get a TCP close (still graceful for HTTP in-flight requests)
3. Apps that are stuck get hard-killed after the timeout

### The Drain Timeout Design

The drain timeout is the maximum time the Supervisor waits between removing an instance
from the upstream table and force-killing it. During this window:
- Pingora routes no new requests to the instance (it's not in the upstream table)
- Existing in-flight requests continue until they complete or the timeout expires

Setting the timeout too short kills requests mid-flight. Setting it too long holds ports
and memory for longer than necessary. The default of 30 seconds covers 99.9% of real
HTTP requests (even slow DB queries) while keeping deployment speed reasonable.

---
1. Stop accepting new connections
2. Finish all in-flight HTTP requests
3. Exit cleanly (no OOM, no panic, no port left open)

This is non-trivial in Wasm/WASI because there is no `SIGTERM`. This file covers the
three available mechanisms and when to use each.

---

## 1. The Problem

In native Linux: `kill -TERM <pid>` → Tokio catches it → Axum drains → process exits.

In Wasm/WASI:
- There is no `SIGTERM`
- The Supervisor and the Wasm instance share **no process boundary**
- The `shutdown_tx.send(())` in step 07 drops the Tokio task handle, which causes a **hard abort**
  (the Wasm linear memory is freed, all in-flight requests are killed)

---

## 2. Mechanism 1 — TCP Close (Recommended for Axum)

The Supervisor **closes the pre-bound TCP listener** that was passed to the Wasm module.
When the listener fd is closed, `axum::serve()` returns from its accept loop, triggering
Axum's built-in graceful shutdown.

```rust
// crates/supervisor/src/instance.rs
use std::net::TcpListener;
use std::os::unix::io::IntoRawFd;

pub struct InstanceHandle {
    pub id: InstanceId,
    pub addr: SocketAddr,
    /// Drop this to close the pre-bound listener and signal the Wasm app to stop.
    pub listener_guard: Option<TcpListener>,
    pub task: tokio::task::JoinHandle<runtime::executor::ExecutionStats>,
}

impl InstanceHandle {
    /// Initiate graceful shutdown by closing the TCP listener.
    /// The Wasm app's axum::serve() will notice the listener is gone and exit.
    pub fn initiate_shutdown(&mut self) {
        // Dropping the listener closes the fd.
        // The Wasm module has a duplicate fd (from preopen), but the OS will
        // close the accept-side, causing new accept() calls to fail → Tokio runtime exits.
        drop(self.listener_guard.take());
        tracing::info!(instance = %self.id.0, "TCP listener closed — graceful shutdown initiated");
    }

    /// Wait for the Wasm task to finish (with a hard timeout).
    pub async fn wait_for_exit(self, timeout: std::time::Duration)
        -> Option<runtime::executor::ExecutionStats>
    {
        match tokio::time::timeout(timeout, self.task).await {
            Ok(Ok(stats)) => Some(stats),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "instance task panicked");
                None
            }
            Err(_) => {
                tracing::warn!(instance = %self.id.0, "instance did not exit within timeout — aborting");
                // Task is dropped here = hard abort
                None
            }
        }
    }
}
```

### Axum side (inside the Wasm app)

Axum `0.7+` has a built-in graceful shutdown hook that waits for in-flight requests:

```rust
// apps/hello-axum/src/main.rs
use tokio::net::TcpListener;
use axum::Router;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").unwrap_or("8080".into()).parse().unwrap();
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    let app = Router::new().route("/", axum::routing::get(handler));

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    // When the Supervisor closes the listener fd, our TcpListener will error
    // on the next .accept() call. Tokio's runtime will then return from main().
    // No explicit signal handling needed — axum handles this automatically.
    //
    // As a safety net, also handle WASI-level ctrl_c (available in wasmtime-wasi):
    tokio::signal::ctrl_c().await.ok();
}
```

---

## 3. Mechanism 2 — WASI Shared Memory Flag

For apps that need to do cleanup work before exiting (flush caches, commit transactions),
use a **shared flag** in the Wasm linear memory that the Supervisor writes to.

```rust
// crates/runtime/src/executor.rs
// After Instance::new(), read the exported shutdown flag memory address.
// The Supervisor writes 1 to it when it wants the app to stop.

pub struct ShutdownFlag {
    pub memory: wasmtime::Memory,
    pub offset: u32, // byte offset in the Wasm linear memory
}

impl ShutdownFlag {
    /// Signal the Wasm module to shut down.
    pub fn signal(&self, store: &mut wasmtime::Store<()>) {
        // Write 1 at the shutdown offset
        self.memory.write(store, self.offset as usize, &[1u8]).ok();
    }
}
```

The Wasm app exports a well-known memory location:

```rust
// apps/hello-axum/src/main.rs (if using shutdown flag mechanism)
use std::sync::atomic::{AtomicBool, Ordering};

// The Supervisor writes to this address to signal shutdown.
#[no_mangle]
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

async fn shutdown_signal() {
    // Poll the flag every 10ms
    loop {
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
```

---

## 4. Mechanism 3 — HTTP Shutdown Endpoint

The Supervisor sends an HTTP request to a dedicated `/admin/shutdown` endpoint on the
instance before dropping the task. This is the most app-friendly approach.

```rust
// apps/hello-axum/src/main.rs (add admin routes)
use std::sync::Arc;
use tokio::sync::Notify;

async fn main() {
    let shutdown = Arc::new(Notify::new());
    let shutdown_clone = shutdown.clone();

    let app = Router::new()
        .route("/", axum::routing::get(handler))
        .route("/_platform/shutdown", axum::routing::post({
            let s = shutdown_clone.clone();
            move || async move {
                s.notify_one();
                "shutting down"
            }
        }));

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.notified().await })
        .await
        .unwrap();
}
```

```rust
// crates/supervisor/src/instance.rs — initiate_shutdown via HTTP
pub async fn initiate_http_shutdown(addr: SocketAddr) -> Result<(), reqwest::Error> {
    let url = format!("http://{}/_platform/shutdown", addr);
    reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await?;
    Ok(())
}
```

---

## 5. Supervisor Drain Logic (Using All Mechanisms)

```rust
// crates/supervisor/src/lib.rs — kill_instance (revised)
impl Supervisor {
    pub async fn kill_instance_gracefully(
        &self,
        app_id: &AppId,
        id: &InstanceId,
        drain_timeout: std::time::Duration,
    ) -> Result<(), PlatformError> {
        // 1. Remove from upstream table (stop new requests)
        let addr = {
            let pools = self.pools.read().await;
            pools.get(&app_id.0)
                .and_then(|p| p.instances.iter().find(|i| i.id == *id))
                .and_then(|i| match &i.state {
                    InstanceState::Ready { addr } => Some(*addr),
                    _ => None,
                })
        };
        if let Some(addr) = addr {
            self.upstream_registry.remove(app_id, &addr).await;
        }

        // 2. Try HTTP shutdown endpoint first (cleanest)
        if let Some(addr) = addr {
            if initiate_http_shutdown(addr).await.is_ok() {
                tracing::info!(app = %app_id.0, instance = %id.0, "HTTP shutdown sent");
            }
        }

        // 3. Wait for in-flight requests to drain
        tokio::time::sleep(drain_timeout).await;

        // 4. Close TCP listener (causes Axum accept loop to stop)
        // 5. Wait for Tokio task to exit (with hard timeout)
        {
            let mut pools = self.pools.write().await;
            if let Some(pool) = pools.get_mut(&app_id.0) {
                if let Some(pos) = pool.instances.iter().position(|i| i.id == *id) {
                    let inst = pool.instances.remove(pos);
                    inst.handle.initiate_shutdown();
                    // Wait up to 5 more seconds for the task to finish
                    let _ = inst.handle.wait_for_exit(
                        std::time::Duration::from_secs(5)
                    ).await;
                    if let Some(addr) = addr {
                        self.port_alloc.release(addr.port());
                        self.service_registry.deregister(app_id, &addr).await;
                    }
                }
            }
        }

        tracing::info!(app = %app_id.0, instance = %id.0, "instance stopped gracefully");
        Ok(())
    }
}
```

---

## 6. Node Shutdown (SIGTERM on the Node Process)

When the node itself receives SIGTERM (e.g. `systemctl stop wasm-node`), it must
gracefully drain all instances before exiting.

```rust
// crates/node/src/main.rs — add before proxy_server.run()

// Listen for SIGTERM / Ctrl-C
let shutdown_supervisor = supervisor.clone();
tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("SIGTERM received — draining all instances");

    let app_ids = shutdown_supervisor.list_app_ids().await;
    for app_id in app_ids {
        shutdown_supervisor.drain_app(
            &app_id,
            std::time::Duration::from_secs(30),
        ).await.ok();
        shutdown_supervisor.kill_all_instances(&app_id).await.ok();
    }

    tracing::info!("all instances stopped — exiting");
    std::process::exit(0);
});
```

---

## 7. Recommendation: Which Mechanism to Use?

| Scenario | Recommended mechanism |
|----------|----------------------|
| Normal idle timeout pruning | Mechanism 1 (TCP close) — zero app changes needed |
| Hot-swap drain | Mechanism 3 (HTTP `/admin/shutdown`) — allows app to flush state |
| Emergency kill (trap / OOM) | Drop the JoinHandle — hard abort, no drain |
| Node shutdown (SIGTERM) | Mechanism 3 + fallback to 1, then hard abort |

**Default**: implement Mechanism 1 first (it requires no changes in the Wasm app).
Add Mechanism 3 as an optional enhancement for apps that opt in.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Mechanism 1 — TCP Close
- [ ] `InstanceHandle::initiate_shutdown()` closes the pre-bound TCP listener
- [ ] The Wasm Axum server detects the closed listener and exits its accept loop
- [ ] `wait_for_exit(timeout)` returns `Some(stats)` when the Wasm task exits cleanly
- [ ] `wait_for_exit(timeout)` returns `None` and logs a warning when the timeout is exceeded
- [ ] The port is released to the pool after the instance exits regardless of how it exited

### Mechanism 3 — HTTP Shutdown Endpoint (opt-in)
- [ ] `POST /_platform/shutdown` on a running instance causes `axum::serve` to stop accepting new requests
- [ ] In-flight requests complete after the shutdown signal before the server exits
- [ ] `initiate_http_shutdown(addr)` returns `Ok` when the endpoint responds and `Err` on timeout/unreachable

### Graceful Drain Flow
- [ ] `kill_instance_gracefully()` removes the instance from the upstream registry first (no new requests)
- [ ] It then sends the HTTP shutdown signal (if app opts in)
- [ ] It then waits `drain_timeout` for in-flight requests to finish
- [ ] It then calls `initiate_shutdown()` (TCP close) as a fallback
- [ ] Zero requests return 5xx during a graceful kill (verified with concurrent load test)

### Node Shutdown (SIGTERM)
- [ ] `Ctrl-C` or `SIGTERM` triggers the drain of all instances across all apps
- [ ] The drain respects a hard timeout (e.g. 30s) — the process exits even if some instances are stuck
- [ ] The process exits with code 0 on clean shutdown

### Tests
- [ ] A test spawns an instance, sends a graceful kill, and verifies the instance exits without hard abort
- [ ] A test sends 10 concurrent requests, initiates shutdown halfway through, and verifies all 10 complete
- [ ] A test verifies the port is released after shutdown so it can be reallocated immediately
