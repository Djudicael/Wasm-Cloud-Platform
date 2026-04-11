# Step 04 — WASI Networking & Port Allocation

## Goal
Control the network access granted to each Wasm module.

---

## Context & Rationale

### The Problem This Solves

A Wasm module compiled to `wasm32-wasip2` can open TCP sockets — but by default it has
unrestricted access to the host's network stack. In a multi-tenant platform, this is
unacceptable:

- App A must not be able to bind on port 80 (reserved for the proxy)
- App A must not be able to bind on the same port as App B
- App A should not be able to make outbound calls unless explicitly permitted

This step implements **network isolation**: the Supervisor owns all port allocation, and
each Wasm module gets access to exactly one pre-allocated port.

### Why Pre-Bind the Socket on the Host?

Two approaches exist for giving a Wasm module a TCP socket:

**Option A: Grant socket-creation rights**
Let the Wasm module call `bind("0.0.0.0:8080")` with WASI's `sock_bind`. The Supervisor
allows this, but limits the port range.

**Option B: Pre-bind the socket on the host, pass the fd**
The Supervisor calls `TcpListener::bind("0.0.0.0:10347")` *before* the Wasm module starts.
It passes the already-bound fd to the Wasm module via WASI preopened sockets.

**Option B is strictly more secure.** With Option A, the Wasm module has `sock_bind` rights
and could (in theory, via a bug in the WASI host) bind to other ports. With Option B, the
module never has socket-creation rights at all — it only has `accept()` rights on one
specific, already-bound fd. A malicious module cannot bind any new sockets.

**This platform uses Option B** as the default, with Option A as a fallback for compatibility.

### The Port Isolation Challenge

The platform runs potentially hundreds of Wasm instances concurrently. Each instance must
listen on a unique port. The challenge:

- Ports cannot be dynamically negotiated with the app (the app just binds `:8080`)
- Ports must be reclaimed when an instance stops, so they can be reused
- Port allocation must be thread-safe (multiple spawns can happen concurrently)

The `PortAllocator` solves this with a `BTreeSet<u16>` of free ports protected by a `Mutex`.
Allocation is O(log n). Release inserts back in O(log n). The port range (10000–19999) is
configurable and must not overlap with well-known system ports.

### East-West Communication: Why Not Through Pingora?

When App A needs to call App B on the same node, routing through Pingora would mean:
1. App A makes HTTP request to `http://api-b.internal/...`
2. Request leaves App A → goes to Pingora (loopback, but still a context switch)
3. Pingora looks up the route → finds App B's port
4. Request forwarded to App B

The `LocalServiceRegistry` enables direct routing: the Supervisor intercepts the WASI
connect call, sees the destination is a known local app, and routes to the local port
directly. This eliminates the proxy hop and reduces latency by ~0.5ms per call.

--- The Supervisor owns a port allocator
that assigns unique ports to each running instance. It then injects this port into the Wasm
module's WASI environment so the Axum app inside can bind a real TCP listener — but only to
the port the Supervisor chose, not any arbitrary port.

---

## 1. Core Concept: How WASI Networking Works

A Wasm module compiled to `wasm32-wasip2` does not access the network directly. All socket
calls go through **WASI host functions**. When the Supervisor builds the `WasiCtx`, it configures
which network operations are permitted and on which addresses.

```
┌─────────────────────────────────────────────────────┐
│                    Wasm Module (Axum)               │
│                                                     │
│   tokio::net::TcpListener::bind("0.0.0.0:8080")    │
│              │                                      │
│              ▼   (WASI syscall: sock_listen)        │
└─────────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────┐
│              WASMER WASI HOST (Supervisor)          │
│                                                     │
│   Maps virtual "0.0.0.0:8080" inside Wasm to        │
│   real host port 9347 (allocated by Supervisor)     │
└─────────────────────────────────────────────────────┘
                           │
                           ▼
                  Host TCP port 9347  ◄── Pingora proxies here
```

---

## 2. Port Allocator

The port allocator hands out unique ports from a configured range and reclaims them when instances stop.

```rust
// crates/supervisor/src/port_alloc.rs
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Mutex;
use common::error::PlatformError;

/// Allocates TCP ports from a fixed range for Wasm instances.
pub struct PortAllocator {
    free: Mutex<BTreeSet<u16>>,
    bind_addr: std::net::IpAddr,
}

impl PortAllocator {
    /// Create allocator covering [start, end] on the given bind address.
    pub fn new(bind_addr: std::net::IpAddr, start: u16, end: u16) -> Self {
        let free = (start..=end).collect();
        PortAllocator {
            free: Mutex::new(free),
            bind_addr,
        }
    }

    /// Allocate the next available port. Returns Err if the pool is exhausted.
    pub fn allocate(&self) -> Result<u16, PlatformError> {
        let mut free = self.free.lock().unwrap();
        let port = free.iter().next().copied()
            .ok_or_else(|| PlatformError::Runtime("port pool exhausted".into()))?;
        free.remove(&port);
        tracing::debug!(port, "allocated port");
        Ok(port)
    }

    /// Return a port to the pool after an instance stops.
    pub fn release(&self, port: u16) {
        let mut free = self.free.lock().unwrap();
        free.insert(port);
        tracing::debug!(port, "released port");
    }

    /// Get the full SocketAddr for an allocated port.
    pub fn socket_addr(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.bind_addr, port)
    }
}
```

---

## 3. WASI Environment with Network Access

`wasmtime-wasi` allows configuring a virtual TCP socket environment.
The key API is `WasiCtx::builder()` with `.net()` to specify a virtual network.

```rust
// crates/runtime/src/wasi.rs
use wasmtime_wasix::{WasiCtx, WasiCtxBuilder, WasiNetworkingExt};
use wasmtime::Store;
use common::error::PlatformError;

pub struct WasiConfig {
    pub app_name: String,
    pub env_vars: Vec<(String, String)>,
    /// The actual host port the Wasm module may bind on.
    pub host_port: u16,
    /// The port the Wasm code *believes* it is binding on (usually 8080 or env PORT).
    pub wasm_port: u16,
}

/// Build a WasiCtx that:
///   1. Grants network access (TCP listen + connect).
///   2. Exposes env vars (including PORT) to the Wasm module.
///   3. Restricts the module to the allocated host_port.
pub fn build_wasi_env(
    store: &mut Store,
    cfg: &WasiConfig,
) -> Result<WasiCtx, PlatformError> {
    let mut builder = WasiCtx::builder(&cfg.app_name);

    // Inject all env vars
    for (k, v) in &cfg.env_vars {
        builder = builder.env(k, v);
    }

    // Tell the app which port to listen on (the wasm_port is what the app sees)
    builder = builder.env("PORT", &cfg.wasm_port.to_string());

    // Enable TCP networking with the virtual IP stack
    // wasmtime-wasi will route the app's "bind 0.0.0.0:wasm_port" call
    // to the host listener on host_port.
    //
    // Note: exact API depends on wasmtime-wasi version.
    // In wasmtime-wasi 0.19+, use the virtual-net feature:
    builder = builder
        .sandbox_fs(Default::default())
        // Allow outbound TCP connections (for DB calls etc.)
        .allow_connect(true)
        // Allow listening on exactly one port
        .socket_addr_check(custom_port_mapper);

    let wasi_env = builder
        .finalize(store)
        .map_err(|e| PlatformError::Runtime(format!("WasiCtx finalize: {e}")))?;

    Ok(wasi_env)
}
```

### Alternative: Pass pre-bound listener via WASI preopened socket

For maximum security, the Supervisor pre-binds the TCP socket *before* launching the Wasm module,
then passes the already-bound socket fd to the Wasm instance via WASI. The Wasm module never
needs raw socket-creation rights — it just calls `accept()` on a pre-opened fd.

```rust
// Supervisor side (before calling build_wasi_env)
use std::net::TcpListener;
use wasmtime_wasix::WasiCtxBuilder;

pub fn build_wasi_env_prebound(
    store: &mut Store,
    cfg: &WasiConfig,
) -> Result<WasiCtx, PlatformError> {
    // 1. Supervisor binds the port on the host OS
    let listener = TcpListener::bind(format!("0.0.0.0:{}", cfg.host_port))
        .map_err(|e| PlatformError::Runtime(format!("bind failed: {e}")))?;
    listener.set_nonblocking(true).ok();

    // 2. Pass the bound listener to the Wasm module as a preopened socket
    let mut builder = WasiCtx::builder(&cfg.app_name);
    for (k, v) in &cfg.env_vars {
        builder = builder.env(k, v);
    }
    builder = builder
        .env("PORT", &cfg.wasm_port.to_string())
        .socket_addr_check(listener, cfg.wasm_port)  // maps fd to wasm_port
        .sandbox_fs(Default::default());

    builder.finalize(store)
        .map_err(|e| PlatformError::Runtime(format!("WasiCtx finalize: {e}")))
}
```

---

## 4. Network Isolation Policy

| Traffic Direction | Allowed | Mechanism |
|-------------------|---------|-----------|
| Inbound from Pingora | Yes | Pre-bound TCP listener on allocated port |
| Outbound to database | Yes (configurable) | `allow_connect(true)` in WasiCtx |
| Outbound to arbitrary internet | Configurable | Can be blocked by setting `allow_connect(false)` and routing through a sidecar proxy |
| App A → App B (same node) | Via Supervisor | Supervisor routes NATS "local call" directly |
| App A → App B (remote node) | Via direct TCP | Service discovery via NATS, then direct connection |
| Bind on arbitrary port | No | Port pre-bound by Supervisor; app gets a single fd |

---

## 5. East-West Communication (App to App)

When App A calls App B **on the same node**, the Supervisor intercepts and routes internally.

```rust
// crates/supervisor/src/network.rs

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use common::types::AppId;

/// Registry of all running instances and their host addresses.
#[derive(Clone, Default)]
pub struct LocalServiceRegistry {
    /// app_id → list of (instance_id, socket_addr)
    entries: Arc<RwLock<HashMap<String, Vec<SocketAddr>>>>,
}

impl LocalServiceRegistry {
    pub async fn register(&self, app_id: &AppId, addr: SocketAddr) {
        let mut map = self.entries.write().await;
        map.entry(app_id.0.clone()).or_default().push(addr);
    }

    pub async fn deregister(&self, app_id: &AppId, addr: &SocketAddr) {
        let mut map = self.entries.write().await;
        if let Some(addrs) = map.get_mut(&app_id.0) {
            addrs.retain(|a| a != addr);
        }
    }

    /// Get the best address for an app (round-robin or least-loaded).
    pub async fn resolve(&self, app_id: &AppId) -> Option<SocketAddr> {
        let map = self.entries.read().await;
        map.get(&app_id.0)?.first().copied()
    }
}
```

For East-West, the Wasm app sends a normal HTTP request. The Supervisor intercepts at the
WASI network layer and, if the destination is a known local app, connects to the local
port directly without going through Pingora.

---

## 6. Health Check via TCP Probe

The Supervisor confirms an instance is ready by probing the port before telling Pingora about it.

```rust
// crates/supervisor/src/instance.rs (health probe)
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration, timeout};
use std::net::SocketAddr;
use common::error::PlatformError;

/// Wait until the TCP port is accepting connections.
/// Polls every 5ms, gives up after `max_wait`.
pub async fn wait_for_ready(
    addr: SocketAddr,
    max_wait: Duration,
) -> Result<(), PlatformError> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(PlatformError::Runtime(
                format!("instance at {addr} did not become ready in time")
            ));
        }
        match timeout(Duration::from_millis(5), TcpStream::connect(addr)).await {
            Ok(Ok(_)) => {
                tracing::info!(%addr, "instance is ready");
                return Ok(());
            }
            _ => sleep(Duration::from_millis(5)).await,
        }
    }
}

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Port Allocator
- [ ] `PortAllocator::new(addr, start, end)` initializes a pool of `(end - start + 1)` ports
- [ ] `allocate()` returns a different port on each call
- [ ] `allocate()` returns `Err` when the pool is exhausted (not a panic)
- [ ] `release(port)` returns the port to the pool so it can be re-allocated
- [ ] A port released by one instance can be re-allocated to a new instance
- [ ] `allocate()` and `release()` are safe to call from multiple threads simultaneously

### WASI Environment
- [ ] `build_wasi_env()` completes without error for a valid config
- [ ] Injected env vars are readable inside the Wasm module via `std::env::var()`
- [ ] The `PORT` env var injected matches the `wasm_port` in the config
- [ ] A Wasm module cannot read host env vars that were not explicitly injected
- [ ] Two instances of the same app on different ports get different `PORT` values

### Health Probe
- [ ] `wait_for_ready()` returns `Ok` within 500ms when the port opens
- [ ] `wait_for_ready()` returns `Err` after the timeout when the port never opens
- [ ] The function does not consume excessive CPU while polling

### Network Isolation
- [ ] An app given `allow_connect(false)` cannot open an outbound TCP connection
- [ ] An app cannot bind on a port other than the one the Supervisor assigned

### East-West Registry
- [ ] `LocalServiceRegistry::register()` stores an address for an app
- [ ] `LocalServiceRegistry::resolve()` returns the stored address
- [ ] `LocalServiceRegistry::deregister()` removes the address
- [ ] Resolving an unknown app returns `None` (not an error)
```
