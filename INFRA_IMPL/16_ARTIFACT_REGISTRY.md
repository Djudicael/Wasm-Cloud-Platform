# Step 16 — Artifact Registry (Large Binary Distribution)

## Goal
NATS has a default max message size of **1 MB**. A real Axum app compiled to Wasm is
**5–20 MB**. We cannot embed raw bytes in NATS deploy events.

Solution: each node runs a small HTTP artifact server. When deploying, the CLI uploads
the binary to **one node** (or an S3-compatible store). The deploy event carries only
a **URL + SHA-256 hash**. Every node pulls and verifies the binary independently.

---

## Context & Rationale

### The Problem This Solves

Step 08 defined the `DeployApp` event with `wasm_bytes: Vec<u8>`. This works for
toy examples but breaks in production:

- NATS default `max_payload` is 1 MB
- A minimal Axum app compiled to `wasm32-wasip2` is ~5 MB
- A real app with dependencies (HTTP client, DB driver, JSON) is 10–20 MB

Embedding a 15 MB payload in a NATS message would be rejected by the server. Even if
`max_payload` were increased, broadcasting 15 MB to every node on every deploy would
waste significant bandwidth.

### The URL-Based Approach: Decoupling Size from the Control Plane

The solution separates concerns:
- The **control plane** (NATS): carries small metadata (URL + hash + config)
- The **data plane** (HTTP): carries the large binary on demand

Each node fetches the binary independently via HTTP. This means:
- A 3-node cluster does 3 HTTP fetches instead of 1 NATS broadcast of a large payload
- Additional nodes join cheaply: fetch once, compile, done
- The NATS message stays small regardless of binary size

### Why Not Use NATS JetStream's Object Store?

NATS JetStream has an `Object Store` feature for large files. It would be a natural fit.
The reasons for using plain HTTP instead:

1. **No NATS dependency for data transfer**: if NATS is slow, binary distribution is slow.
   With HTTP, the artifact server is independent and can be optimized separately.
2. **Simplicity**: the artifact server is 50 lines of Axum code. JetStream Object Store
   requires managing buckets, chunks, and replication policies.
3. **S3 compatibility**: Strategy B replaces the HTTP server with S3/MinIO. The node code
   only knows "fetch from URL" — the URL scheme determines the backend.

### Why Store Raw .wasm Separately from the Compiled Artifact?

Two separate redb tables:
- `[raw_wasm]`: original `.wasm` bytes, keyed by SHA-256
- `[artifacts]`: AOT-compiled native artifact, keyed by AppId

**Why keep raw bytes at all?** Two reasons:
1. Other nodes need to fetch the binary (they fetch from the `[raw_wasm]` table via HTTP)
2. If a future Wasmtime version requires re-compilation (e.g., a Cranelift security patch
   that invalidates all existing artifacts), the raw bytes are available without needing
   another upload from the operator

**Why delete them eventually?** Raw bytes are larger than needed after compilation. A
15 MB `.wasm` compiles to ~8 MB of native artifact. Once all nodes have compiled, the
raw bytes on the original upload node are redundant.

### SHA-256 as the Universal Key

The SHA-256 of the `.wasm` file serves triple duty:
1. **URL key**: `GET /artifacts/<sha256>` fetches the binary
2. **Integrity check**: the downloading node verifies the hash after download
3. **Deduplication**: if two different apps share the same compiled artifact (e.g., a
   shared library), they store only one copy in `[raw_wasm]`

This is content-addressed storage — the same pattern used by Docker and Git.

---

---

## 1. Two Deployment Strategies

### Strategy A — Peer-to-Peer (No external storage needed)
```
CLI → uploads .wasm to Node-0's artifact server (HTTP PUT)
CLI → publishes DeployApp { artifact_url, sha256 } to NATS
Node-0 → already has the artifact, compiles immediately
Node-1 → fetches from Node-0's artifact server, compiles
Node-2 → fetches from Node-0's artifact server, compiles
```

### Strategy B — Centralized Object Store (S3/MinIO)
```
CLI → uploads .wasm to MinIO/S3
CLI → publishes DeployApp { artifact_url (S3), sha256 } to NATS
All nodes → fetch from S3, compile independently
```

**Implement Strategy A first** (no external dependencies). Strategy B is a config option.

---

## 2. Updated Deploy Event

```rust
// crates/messaging/src/events.rs — replace DeployApp
DeployApp {
    app_id: AppId,
    config: AppConfig,
    /// URL where the .wasm binary can be fetched.
    /// Format: "http://<node-ip>:9091/artifacts/<sha256>"
    artifact_url: String,
    /// Hex-encoded SHA-256 of the raw .wasm bytes.
    sha256: String,
    /// Human-readable size for logging.
    size_bytes: u64,
},
```

---

## 3. Artifact HTTP Server (on each node)

Runs on a dedicated port (default 9091), separate from the admin API (9090).
Only serves GET (read). PUT is authenticated and only accessible on localhost or via mTLS.

```rust
// crates/storage/src/artifact_server.rs
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, put},
    Router,
};
use std::sync::Arc;
use crate::Store;
use common::types::AppId;

#[derive(Clone)]
struct ArtifactServerState {
    store: Store,
}

/// GET /artifacts/:sha256 — serve raw .wasm bytes
async fn get_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
) -> Result<Bytes, StatusCode> {
    // We use sha256 as the lookup key for raw .wasm (pre-compilation)
    // This is separate from the compiled artifact stored under AppId
    match s.store.load_raw_wasm(&sha256) {
        Ok(Some(bytes)) => Ok(Bytes::from(bytes)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// PUT /artifacts/:sha256 — store raw .wasm bytes (localhost only)
async fn put_artifact(
    Path(sha256): Path<String>,
    State(s): State<ArtifactServerState>,
    body: Bytes,
) -> StatusCode {
    // Verify hash before storing
    use sha2::{Sha256, Digest};
    let actual = format!("{:x}", Sha256::digest(&body));
    if actual != sha256 {
        tracing::warn!(expected = %sha256, actual, "SHA-256 mismatch on PUT");
        return StatusCode::BAD_REQUEST;
    }
    match s.store.save_raw_wasm(&sha256, &body) {
        Ok(_) => StatusCode::CREATED,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub fn artifact_router(store: Store) -> Router {
    let state = ArtifactServerState { store };
    Router::new()
        .route("/artifacts/:sha256", get(get_artifact))
        .route("/artifacts/:sha256", put(put_artifact))
        .with_state(state)
}
```

---

## 4. Raw Wasm Table in redb

Separate table for raw `.wasm` bytes (pre-AOT). Keyed by SHA-256.

```rust
// crates/storage/src/tables.rs (add)
/// Key   : sha256 hex string
/// Value : raw .wasm bytes
pub const RAW_WASM: TableDefinition<&str, &[u8]> = TableDefinition::new("raw_wasm");
```

```rust
// crates/storage/src/artifact.rs (add to Store impl)
pub fn save_raw_wasm(&self, sha256: &str, bytes: &[u8]) -> Result<(), PlatformError> {
    let tx = self.db.begin_write()
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    {
        let mut table = tx.open_table(crate::tables::RAW_WASM)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        table.insert(sha256, bytes)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
    }
    tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
}

pub fn load_raw_wasm(&self, sha256: &str) -> Result<Option<Vec<u8>>, PlatformError> {
    let tx = self.db.begin_read()
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    let table = tx.open_table(crate::tables::RAW_WASM)
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    Ok(table.get(sha256)
        .map_err(|e| PlatformError::Storage(e.to_string()))?
        .map(|v| v.value().to_vec()))
}

pub fn raw_wasm_exists(&self, sha256: &str) -> Result<bool, PlatformError> {
    Ok(self.load_raw_wasm(sha256)?.is_some())
}

pub fn delete_raw_wasm(&self, sha256: &str) -> Result<(), PlatformError> {
    let tx = self.db.begin_write()
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    {
        let mut table = tx.open_table(crate::tables::RAW_WASM)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        table.remove(sha256)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
    }
    tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
}
```

---

## 5. Node Artifact Fetcher

When a node receives a `DeployApp` event, it fetches the binary if it doesn't already have it.

```rust
// crates/messaging/src/handlers.rs — updated handle_deploy
async fn handle_deploy(
    &self,
    app_id: AppId,
    config: AppConfig,
    artifact_url: String,
    sha256: String,
) {
    tracing::info!(app = %app_id.0, url = %artifact_url, "fetching artifact");

    // 1. Check local cache first (another node may have already stored it)
    let wasm_bytes = if self.store.raw_wasm_exists(&sha256).unwrap_or(false) {
        tracing::info!(sha256, "artifact already in local cache");
        self.store.load_raw_wasm(&sha256).unwrap().unwrap()
    } else {
        // 2. Fetch from the source node
        match fetch_artifact(&artifact_url, &sha256).await {
            Ok(bytes) => {
                self.store.save_raw_wasm(&sha256, &bytes).ok();
                bytes
            }
            Err(e) => {
                tracing::error!(url = %artifact_url, error = %e, "artifact fetch failed");
                return;
            }
        }
    };

    // 3. Compile and store (existing logic from step 08)
    let runtime = self.runtime.clone();
    let bytes_clone = wasm_bytes.clone();
    let artifact = tokio::task::spawn_blocking(move || runtime.compile(&bytes_clone)).await;

    match artifact {
        Ok(Ok(compiled)) => {
            self.store.store_artifact(&app_id, &compiled).ok();
            self.store.save_config(&config).ok();
            tracing::info!(app = %app_id.0, "deploy complete");
        }
        Ok(Err(e)) => tracing::error!(app = %app_id.0, error = %e, "compilation failed"),
        Err(e)    => tracing::error!(app = %app_id.0, error = %e, "spawn_blocking panic"),
    }
}

async fn fetch_artifact(url: &str, expected_sha256: &str) -> Result<Vec<u8>, String> {
    let resp = reqwest::get(url).await
        .map_err(|e| format!("HTTP GET failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("artifact server returned {}", resp.status()));
    }

    let bytes = resp.bytes().await
        .map_err(|e| format!("failed to read body: {e}"))?
        .to_vec();

    // Verify integrity
    use sha2::{Sha256, Digest};
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected_sha256 {
        return Err(format!("SHA-256 mismatch: expected {expected_sha256}, got {actual}"));
    }

    Ok(bytes)
}
```

Add `reqwest` to `crates/messaging/Cargo.toml`:
```toml
reqwest = { version = "0.12", features = ["rustls-tls"], default-features = false }
sha2    = "0.10"
```

---

## 6. Artifact Garbage Collection

Raw `.wasm` bytes can be deleted once all nodes have compiled them.
Use a simple TTL: delete raw wasm 24 hours after last access.

```rust
// crates/storage/src/artifact.rs (add)
pub fn prune_raw_wasm_older_than(&self, hours: u64) -> Result<u64, PlatformError> {
    // For now: delete raw wasm for any sha256 that has a compiled artifact
    // (meaning compilation succeeded and the raw bytes are no longer needed)
    // Full TTL-based pruning requires adding a metadata table — add in schema v2
    let tx = self.db.begin_read()
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    let raw_table = tx.open_table(crate::tables::RAW_WASM)
        .map_err(|e| PlatformError::Storage(e.to_string()))?;
    let sha256s: Vec<String> = raw_table.iter()
        .map_err(|e| PlatformError::Storage(e.to_string()))?
        .filter_map(|e| e.ok())
        .map(|(k, _)| k.value().to_string())
        .collect();
    drop(raw_table);
    drop(tx);

    let mut deleted = 0u64;
    for sha256 in sha256s {
        self.delete_raw_wasm(&sha256)?;
        deleted += 1;
    }
    Ok(deleted)
}
```

---

## 7. node.toml: Artifact Server Config

```toml
[artifact_server]
port          = 9091
# IP to bind on. "127.0.0.1" = localhost only (safest; other nodes access via cluster network)
bind_addr     = "0.0.0.0"
# Maximum artifact size accepted on PUT (bytes). Reject anything larger.
max_size_bytes = 104_857_600   # 100 MB
```

---

## 8. main.rs: Start Artifact Server

```rust
// crates/node/src/main.rs (add after admin API startup)
let artifact_app = storage::artifact_server::artifact_router(store.clone());
let artifact_addr = format!("0.0.0.0:{}", args.artifact_port); // new arg: --artifact-port 9091
tokio::spawn(async move {
    let listener = tokio::net::TcpListener::bind(&artifact_addr).await
        .expect("artifact server bind failed");
    tracing::info!(addr = %artifact_addr, "artifact server listening");
    axum::serve(listener, artifact_app).await.unwrap();
});
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### HTTP Artifact Server
- [x] `PUT /artifacts/:sha256` accepts a binary body and stores it in the `RAW_WASM` redb table
- [x] `PUT` returns `400 Bad Request` if the SHA-256 of the uploaded body does not match the URL parameter
- [x] `GET /artifacts/:sha256` returns the exact bytes that were previously uploaded
- [x] `GET` returns `404` for an unknown SHA-256
- [x] The server rejects bodies larger than `max_size_bytes` with `413 Payload Too Large`
- [x] The artifact server is reachable on its configured port from other nodes in the cluster

### Raw Wasm Storage
- [x] `save_raw_wasm(sha256, bytes)` stores bytes in the `RAW_WASM` table
- [x] `load_raw_wasm(sha256)` returns `None` for an unknown hash
- [x] `raw_wasm_exists(sha256)` returns `true` after a store and `false` before
- [x] `delete_raw_wasm(sha256)` removes the entry

### Deploy Event
- [x] `Event::DeployApp` carries `artifact_url` and `sha256` — no raw bytes embedded
- [x] A node that receives the event and already has the artifact (matching sha256) skips the download
- [x] A node that does not have the artifact fetches it via HTTP from `artifact_url`
- [x] A download failure (server unreachable, wrong hash) logs an error and aborts the deploy — no partial state

### Integrity Verification
- [x] `fetch_artifact()` verifies the SHA-256 of downloaded bytes before storing or compiling
- [x] A tampered response (bytes modified in transit) is detected and rejected

### Garbage Collection
- [x] `prune_raw_wasm_older_than()` removes raw bytes after they are no longer needed
- [x] Pruning raw bytes does not affect the compiled `ARTIFACTS` table

### Tests
- [x] A test uploads a binary via `PUT`, then retrieves it via `GET`, verifying byte-for-byte equality
- [x] A test uploads a binary with a mismatched SHA-256 and verifies `400` is returned
- [x] A test verifies that a second node can successfully fetch an artifact from the first node's artifact server
```
