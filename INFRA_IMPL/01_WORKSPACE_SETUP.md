# Step 01 — Cargo Workspace & Project Structure

## Goal
Create the monorepo layout for the entire platform. Every crate is isolated so it can be tested
and evolved independently. The final node binary is assembled by the `node` crate.

---

## Context & Rationale

### The Problem This Solves

Without a clear crate boundary strategy, a Wasm cloud platform quickly becomes an unmaintainable
monolith where storage code calls proxy code, the runtime imports metrics, and everything is
coupled to everything. Worse, you cannot test the storage layer without starting the proxy or
the Wasm engine.

This step defines the **dependency graph** before any code is written. Getting this right
means every subsequent module can be developed, compiled, and tested in isolation.

### Why a Cargo Workspace (Monorepo)?

A single repository with multiple crates gives us:
- **One `cargo build --release`** produces all binaries with consistent dependency versions
- **Shared `[workspace.dependencies]`** means a single place to pin versions — no crate-to-crate
  version skew (e.g. two crates pulling different `serde` versions)
- **Incremental compilation** — changing `storage` only recompiles `storage` and its dependents,
  not the whole workspace
- **`cargo test --workspace`** runs all unit tests in one command

The alternative (separate git repos per crate) creates version pinning pain: every time
`common` changes you must update and publish it before `supervisor` can consume it.

### Crate Dependency Graph

This diagram shows which crates depend on which. The arrow means "depends on":

```
common ◄──── storage ◄──── runtime ◄──── supervisor ◄──── node
                │                              │              │
                └──── secrets ────────────────►│              │
                                               │              │
                              messaging ───────►│◄────────────┤
                                               │              │
                              metrics ─────────►│             │
                                               │              │
                              proxy ───────────►────────────►─┘
```

- `common` has zero internal dependencies — it defines shared types (`AppId`, `FuelQuota`, etc.)
  and the unified `PlatformError` enum. **Every other crate depends on it.**
- `storage` only knows about `common`. It does not know about Wasm or NATS.
- `runtime` wraps Wasmer. It only knows about `common` and `storage` (to load artifacts).
- `supervisor` is the coordinator: it knows about `runtime`, `storage`, `secrets`, `metrics`.
- `proxy` knows only about `common` and the upstream registry it shares with `supervisor`.
- `node` knows about everything — it is the wiring layer.

### Why Separate `proxy` and `supervisor`?

The proxy and supervisor must share the `UpstreamRegistry` (the live table of instance addresses).
By making this a shared struct in `proxy::upstream`, neither crate owns the other.
The `supervisor` writes to the registry; `proxy` reads from it. This prevents a circular dependency.

### Why a Separate `common` Crate?

Without `common`, if `proxy` needs `AppId` it imports from `supervisor`, which imports from
`storage`, which creates an indirect coupling between `proxy` and `storage`. `common` breaks
this by being the one crate everyone can depend on without creating cycles.

### Why `wasm32-wasip2` (not `wasm32-wasi` or `wasm32-unknown-unknown`)?

- `wasm32-unknown-unknown`: No WASI support. The Wasm module cannot open sockets, files,
  or environment variables. Axum cannot run.
- `wasm32-wasi` (Preview 1): Legacy WASI. Has TCP listener support but Preview 1 is being
  deprecated. Many Tokio APIs do not work.
- `wasm32-wasip2` (Preview 2): The current standard. Full networking support (TCP listen +
  connect), async Tokio compatibility, and is what `wasmer-wasix` targets.

---

---

## 1. Directory Layout

```
my-wasm-cloud-platform/
├── Cargo.toml                  ← workspace root
├── crates/
│   ├── node/                   ← final binary (ties everything together)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs
│   ├── supervisor/             ← Wasm lifecycle manager
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── instance.rs     ← spawn / prune / health
│   │       ├── pool.rs         ← hot-standby pool
│   │       └── port_alloc.rs   ← dynamic port allocation
│   ├── runtime/                ← Wasmer wrapper
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── compiler.rs     ← AOT compile + serialize
│   │       ├── executor.rs     ← instantiate + call + collect metrics
│   │       └── limits.rs       ← fuel + memory tunables
│   ├── proxy/                  ← Pingora integration
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── service.rs      ← ProxyHttp impl
│   │       └── upstream.rs     ← dynamic upstream registry
│   ├── storage/                ← redb abstraction
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── tables.rs       ← table definitions
│   │       ├── artifact.rs     ← compiled Wasm blobs
│   │       ├── config.rs       ← app configs + env vars
│   │       ├── secrets.rs      ← encrypted secrets
│   │       └── metrics.rs      ← telemetry records
│   ├── messaging/              ← NATS integration
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── events.rs       ← event types (deploy, secret_update, …)
│   │       └── handlers.rs     ← NATS subscriber handlers
│   ├── secrets/                ← encryption & secret provider trait
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── crypto.rs       ← AES-GCM-SIV encryption primitives
│   │       ├── provider.rs     ← SecretProvider trait
│   │       └── vault.rs        ← optional Vault HTTP adapter
│   ├── metrics/                ← Prometheus + OpenTelemetry
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── collector.rs    ← mpsc channel + batch writer
│   │       └── exporter.rs     ← /metrics HTTP endpoint
│   └── common/                 ← shared types, errors
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── types.rs        ← AppId, InstanceId, FuelQuota, …
│           └── error.rs        ← platform Error enum
└── apps/                       ← example Axum Wasm apps for testing
    └── hello-axum/
        ├── Cargo.toml
        └── src/
            └── main.rs
```

---

## 2. Workspace `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "crates/node",
    "crates/supervisor",
    "crates/runtime",
    "crates/proxy",
    "crates/storage",
    "crates/messaging",
    "crates/secrets",
    "crates/metrics",
    "crates/common",
]

[workspace.dependencies]
# Async
tokio        = { version = "1", features = ["full"] }
tokio-util   = { version = "0.7" }

# Serialization
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
bincode      = "2"

# Error handling
thiserror    = "1"
anyhow       = "1"

# Logging / Tracing
tracing               = "0.1"
tracing-subscriber    = { version = "0.3", features = ["env-filter", "json"] }

# Wasm runtime
wasmer                    = { version = "4", features = ["cranelift"] }
wasmer-compiler-cranelift = "4"
wasmer-wasix              = "0.19"

# Proxy
pingora               = "0.4"
pingora-proxy         = "0.4"
pingora-load-balancing = "0.4"
pingora-core          = "0.4"

# Storage
redb = "2"

# Messaging
async-nats = "0.36"

# Encryption
aes-gcm          = "0.10"
chacha20poly1305 = "0.10"
rand             = "0.8"
zeroize          = { version = "1", features = ["derive"] }

# Metrics
prometheus      = { version = "0.13", features = ["process"] }
opentelemetry   = "0.23"
opentelemetry-otlp = { version = "0.16", features = ["tonic"] }

# HTTP (admin API)
axum = "0.7"
tower = "0.4"

# CLI
clap = { version = "4", features = ["derive"] }

# Utils
uuid   = { version = "1", features = ["v4"] }
chrono = { version = "0.4", features = ["serde"] }
```

---

## 3. Crate `Cargo.toml` Templates

### `crates/common/Cargo.toml`
```toml
[package]
name    = "common"
version = "0.1.0"
edition = "2021"

[dependencies]
serde       = { workspace = true }
serde_json  = { workspace = true }
thiserror   = { workspace = true }
uuid        = { workspace = true }
chrono      = { workspace = true }
```

### `crates/storage/Cargo.toml`
```toml
[package]
name    = "storage"
version = "0.1.0"
edition = "2021"

[dependencies]
redb       = { workspace = true }
serde      = { workspace = true }
serde_json = { workspace = true }
bincode    = { workspace = true }
thiserror  = { workspace = true }
tracing    = { workspace = true }
common     = { path = "../common" }
```

### `crates/runtime/Cargo.toml`
```toml
[package]
name    = "runtime"
version = "0.1.0"
edition = "2021"

[dependencies]
wasmer                    = { workspace = true }
wasmer-compiler-cranelift = { workspace = true }
wasmer-wasix              = { workspace = true }
tokio                     = { workspace = true }
thiserror                 = { workspace = true }
tracing                   = { workspace = true }
common                    = { path = "../common" }
storage                   = { path = "../storage" }
```

### `crates/supervisor/Cargo.toml`
```toml
[package]
name    = "supervisor"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio     = { workspace = true }
thiserror = { workspace = true }
tracing   = { workspace = true }
common    = { path = "../common" }
runtime   = { path = "../runtime" }
storage   = { path = "../storage" }
secrets   = { path = "../secrets" }
metrics   = { path = "../metrics" }
```

### `crates/proxy/Cargo.toml`
```toml
[package]
name    = "proxy"
version = "0.1.0"
edition = "2021"

[dependencies]
pingora                = { workspace = true }
pingora-proxy          = { workspace = true }
pingora-load-balancing = { workspace = true }
pingora-core           = { workspace = true }
tokio                  = { workspace = true }
tracing                = { workspace = true }
common                 = { path = "../common" }
```

### `crates/node/Cargo.toml`
```toml
[package]
name    = "node"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "wasm-node"
path = "src/main.rs"

[dependencies]
tokio      = { workspace = true }
tracing    = { workspace = true }
tracing-subscriber = { workspace = true }
clap       = { workspace = true }
anyhow     = { workspace = true }
common     = { path = "../common" }
storage    = { path = "../storage" }
runtime    = { path = "../runtime" }
supervisor = { path = "../supervisor" }
proxy      = { path = "../proxy" }
messaging  = { path = "../messaging" }
metrics    = { path = "../metrics" }
secrets    = { path = "../secrets" }
```

---

## 4. `common` Types (Shared Across All Crates)

### `crates/common/src/types.rs`
```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a deployed application.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    pub fn new(name: &str, version: &str) -> Self {
        AppId(format!("{name}:{version}"))
    }
}

/// Unique identifier for a running Wasm instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub Uuid);

impl InstanceId {
    pub fn new() -> Self {
        InstanceId(Uuid::new_v4())
    }
}

/// CPU quota: maximum Fuel units allowed per request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FuelQuota(pub u64);

/// Memory quota: maximum Wasm linear memory pages (1 page = 64 KiB).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemoryPages(pub u32);

impl MemoryPages {
    /// Convert to bytes.
    pub fn to_bytes(self) -> usize {
        self.0 as usize * 64 * 1024
    }
}

/// Full configuration for a deployed application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub id: AppId,
    pub fuel_quota: FuelQuota,
    pub memory_limit: MemoryPages,
    pub env_vars: Vec<(String, String)>,
    pub port: u16,       // the port this app binds inside WASI
}

/// State of a running instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstanceState {
    Starting,
    Ready { addr: std::net::SocketAddr },
    Busy,
    Stopping,
    Stopped,
}
```

### `crates/common/src/error.rs`
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PlatformError {
    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Runtime error: {0}")]
    Runtime(String),

    #[error("Fuel exhausted for app {app_id}")]
    FuelExhausted { app_id: String },

    #[error("Memory limit exceeded for app {app_id}")]
    MemoryLimitExceeded { app_id: String },

    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("Instance not found: {0}")]
    InstanceNotFound(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Messaging error: {0}")]
    Messaging(String),

    #[error("Proxy error: {0}")]
    Proxy(String),
}
```

---

## 5. Example App: `hello-axum` (the Wasm target)

### `apps/hello-axum/Cargo.toml`
```toml
[package]
name    = "hello-axum"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "hello-axum"

[dependencies]
axum    = "0.7"
tokio   = { version = "1", features = ["full"] }

[profile.release]
opt-level = 3
lto       = true
strip     = "symbols"
```

### `apps/hello-axum/src/main.rs`
```rust
use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(|| async { "Hello from Wasm!" }));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Build command (produces the `.wasm` binary)
```bash
rustup target add wasm32-wasip2
cargo build \
  --manifest-path apps/hello-axum/Cargo.toml \
  --target wasm32-wasip2 \
  --release
# Output: target/wasm32-wasip2/release/hello-axum.wasm
```

---

## 6. Build & Run the Node

```bash
# Build all crates
cargo build --release

# Run the node binary
./target/release/wasm-node \
  --db-path /var/lib/wasm-node/state.redb \
  --nats-url nats://127.0.0.1:4222 \
  --proxy-port 8080 \
  --admin-port 9090
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Workspace & Structure
- [ ] `cargo build` succeeds with zero errors across all crates
- [ ] `cargo clippy -- -D warnings` passes with no warnings
- [ ] All crate `Cargo.toml` files reference `workspace = true` for shared dependencies
- [ ] No duplicate dependency versions (`cargo tree --duplicates` is clean)

### Common Crate
- [ ] `AppId`, `InstanceId`, `FuelQuota`, `MemoryPages`, `AppConfig`, `InstanceState` are defined
- [ ] `PlatformError` enum covers all error categories (Storage, Runtime, Encryption, Messaging, Proxy)
- [ ] All types derive `Serialize`, `Deserialize`, `Clone`, `Debug`

### Example App
- [ ] `rustup target add wasm32-wasip2` works
- [ ] `cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release` produces a `.wasm` file
- [ ] `.wasm` file size is reasonable (< 20 MB after `wasm-opt` stripping)

### Tests
- [ ] `cargo test --workspace` passes (all unit tests green)
- [ ] `cargo doc --no-deps` generates documentation without errors
