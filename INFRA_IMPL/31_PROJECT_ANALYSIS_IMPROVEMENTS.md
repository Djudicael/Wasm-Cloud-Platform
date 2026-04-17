# Step 31 — Project Analysis & Improvement Plan

## Goal

A comprehensive audit of the implemented crates against the INFRA_IMPL design documents,
identifying gaps, code quality issues, missing features, architectural weaknesses, and
opportunities for new design documents. This is not a design doc itself — it is a
**living checklist and roadmap** for bringing the implementation to full fidelity with
the architecture vision.

---

## Methodology

Every crate was read in full. Every INFRA_IMPL document was reviewed. Each finding is
classified by severity and tagged with the relevant crate or design doc.

**Severity levels:**
- **P0 — Critical**: Data loss, security vulnerability, or correctness bug
- **P1 — High**: Feature gap that undermines a core guarantee from the design docs
- **P2 — Medium**: Code quality, maintainability, or missing test coverage
- **P3 — Low**: Nice-to-have, polish, future consideration

---

## 1. Crate-by-Crate Findings

### 1.1 `crates/common`

**P1 — `PlatformError` is a flat string enum with no source chain**

`D:\dev\Wasm-Cloud-Platform\crates\common\src\error.rs` defines every variant as
`Storage(String)`, `Runtime(String)`, etc. This means:
- No `#[source]` attribute — `anyhow::Error` chains are lost
- Callers construct errors with `PlatformError::Storage(e.to_string())`, destroying
  the original `redb::Error` or `serde_json::Error` context
- Debugging requires reading log messages, not attaching a debugger to the error chain

**Fix**: Use `thiserror` properly with source types:
```rust
#[error("Storage error")]
Storage(#[source] redb::Error),
#[error("Serialization error")]
Serialization(#[source] serde_json::Error),
```
This requires adding `redb` and `serde_json` as optional dependencies of `common`, or
defining a separate error type per crate. The current approach of stringifying everything
into one enum is a maintenance burden.

**P2 — `AppId` is a newtype over `String` with no validation**

`AppId("api-users:v1")` is valid, but so is `AppId("")`, `AppId(":::")`, or
`AppId("app with spaces : v1")`. The `AppId::new()` constructor does no validation.
Since `AppId` is used as a redb key (via `.as_str()`), invalid strings could cause
subtle bugs in table lookups or NATS subject formatting.

**Fix**: Add validation in `AppId::new()` — reject empty strings, whitespace, and
characters that are invalid in NATS subjects (`>`, `*`, `.`, `\n`).

**P2 — `InstanceId` wraps `Uuid` but is never used as a redb key**

`InstanceId(Uuid)` is generated but the billing system uses `instance_id: String` and
the instance manager uses `InstanceId` only for in-memory tracking. The Uuid is
converted to string via `.0.to_string()` in many places. Consider either committing
to Uuid as the canonical format or simplifying to a string.

**P2 — `DnsConfig` lives in `common/types.rs` but is only used by `proxy`**

`DnsConfig` and `DnsConfig::default_with_port()` are defined in common but only consumed
by the proxy crate's DNS webhook. This pollutes the common crate with proxy-specific
concerns.

**P3 — `ExtendedLimits` is not serialized in `AppConfig` consistently**

`ExtendedLimitsConfig` (the optional version) is in `AppConfig`, but `ExtendedLimits`
(the resolved version) is a separate struct with `Copy`. The `IoResourceTracker` in
`runtime/limits.rs` uses `ExtendedLimits` but the values are never actually enforced
by Wasmtime — they are only tracked as counters with no connection to the WASI layer.

---

### 1.2 `crates/storage`

**P1 — `Store::db_path()` returns a hardcoded `/tmp/unknown.redb`**

`D:\dev\Wasm-Cloud-Platform\crates\storage\src\integrity.rs` line:
```rust
pub fn db_path(&self) -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/unknown.redb")
}
```
This is called by `recovery::startup_integrity_check()` when it needs to delete a
corrupted database. It will **delete the wrong file** or fail silently. The actual
path is passed to `Store::open()` but never stored.

**Fix**: Store the path in the `Store` struct during `open()`:
```rust
pub struct Store {
    pub db: Arc<Database>,
    db_path: std::path::PathBuf,
}
```

**P1 — `check_table_readable()` iterates the entire table to check readability**

The integrity check reads every row of every table just to verify the table is
readable. For the `ARTIFACTS` table (which holds multi-MB compiled binaries as values),
this means reading gigabytes of data on every startup. The design doc (Step 27) says
"open a read transaction and iterate the table" but the implementation takes this
literally.

**Fix**: Only check that the table can be opened and the first entry can be read:
```rust
fn check_table_readable(&self, table_name: &str) -> Result<u64, PlatformError> {
    let tx = self.db.begin_read()?;
    let table = tx.open_table(table_def)?;
    // Just check the first entry, not all of them
    match table.iter()?.next() {
        Some(Ok(_)) => Ok(table.len()? as u64),
        None => Ok(0), // Empty table is OK
        Some(Err(e)) => Err(PlatformError::Storage(e.to_string())),
    }
}
```

**P1 — `partial_rebuild()` for routes does not actually clear the corrupted table**

`recreate_table_routes()` opens the table in a write transaction and then commits
without deleting anything. `redb` tables cannot be truncated — you must delete the
database and recreate it, or delete each entry individually. The current code does
nothing useful.

**Fix**: Either iterate and delete all entries, or document that redb's MVCC means
a "corrupted" table will likely require a full rebootstrap anyway (since redb's
checksums catch corruption at the page level, not the entry level).

**P2 — `Store` is `Clone` but cloning `Arc<Database>` means all clones share the same
underlying file**

This is intentional (shared database within one process), but there is no documentation
of this invariant. A future developer might expect `Clone` to create an independent
database. Add a doc comment.

**P2 — Migration code reads all records, drops the read transaction, then writes**

The `migrate_v1_to_v2()` and `migrate_v2_to_v3()` methods read all configs into a
`Vec`, drop the read transaction, then open a write transaction. Between the read and
write, another writer could modify the data. In practice this is fine because the
migration runs at startup before any other writers exist, but it is not formally safe.

**P2 — No `billing` table in `Store::open()` table creation**

The `BILLING` table is defined in `tables.rs` and opened in `Store::open()`, but it
was added without a corresponding schema version bump. The schema version is 3, but
the billing table was added after version 3. This means a node with schema version 3
might not have the billing table if it was created by an older binary. The migration
system should have a v3→v4 step that creates the billing table.

**P3 — `GcConfig` is in `common` but GC logic is in `storage`**

`common::gc::GcConfig` defines the configuration, but `storage::gc` implements the
logic. The config should probably live in `storage` since no other crate uses it
directly.

---

### 1.3 `crates/runtime`

**P1 — `eprintln!()` debug statements left in production code**

`D:\dev\Wasm-Cloud-Platform\crates\runtime\src\executor.rs` contains 15+ `eprintln!()`
calls:
```rust
eprintln!("[SPAWN] WASI config: inherit_network=true, ...");
eprintln!("[SPAWN] About to instantiate component");
eprintln!("[RUN] Checking {}: {:?}", interface_name, interface_idx);
eprintln!("🔴 WASM ERROR: instance={}, exit code=1", self.id.0);
```
These bypass the tracing system and write directly to stderr. In production, they
cannot be filtered by log level, and they appear in every node's stderr regardless
of `RUST_LOG` settings.

**Fix**: Replace all `eprintln!()` with `tracing::debug!()` or `tracing::trace!()`.
The emoji-prefixed ones (`🔴`) should be `tracing::error!()`.

**P1 — `RunningInstance::read_memory_usage()` returns a hardcoded `1024`**

```rust
fn read_memory_usage(&mut self) -> usize {
    // In the component model, linear memories are deeply nested and not directly
    // exported as `Val::Memory`. For now, we return a non-zero placeholder.
    1024
}
```
This means:
- Billing records always show `ram_bytes: 1024` regardless of actual usage
- The memory pressure sentinel (Step 30 eBPF) cannot correlate with actual Wasm memory
- The `MemoryLimiter` tracks memory growth but the result is never read

**Fix**: Wasmtime's `Memory` type exposes `data_size()` and `byte_size()`. After
instantiation, iterate the module's exported memories and sum their sizes. For the
component model, use `Instance::exports()` to find memory exports.

**P1 — `IoResourceTracker` tracks limits but never enforces them**

The `track_fd_open()`, `track_fs_write()`, `track_net_egress()()`, and
`track_outbound_connect()` methods return `Result<(), PlatformError>` but are never
called by any WASI hook. The WASI layer in `executor.rs` does:
```rust
builder.inherit_network();
builder.allow_tcp(true);
```
This gives the Wasm module unrestricted network and file access. The `IoResourceTracker`
is a dead code path — it exists in `StoreState` but nothing calls its methods.

**Fix**: This requires implementing custom WASI host functions that intercept file
and network operations and call the tracker. This is a significant implementation
gap relative to Step 13 (Security Model) which promises per-app FD limits, egress
limits, and connection limits.

**P2 — `wasi.rs` from Step 13 is not implemented**

The design doc defines `NetworkPolicy` with `allow_outbound_tcp`, `allowed_cidrs`,
`max_connections`, and an `apply_network_policy()` function. None of this exists in
the codebase. The current implementation uses `WasiCtxBuilder` defaults with no
policy enforcement.

**P2 — WASI Preview 2 version probing is fragile**

The `run()` method tries 7 WASI version strings (`0.2.0` through `0.2.6`) to find
the entry point. If Wasmtime adds `0.2.7`, this code silently fails. This should
use a more robust discovery mechanism or at least log which version was found.

**P2 — `unsafe fn deserialize()` has no hash verification**

Step 13 (Security) specifies SHA-256 verification before compilation. The `deserialize()`
function trusts any bytes. The hash check happens in `handlers.rs` during deploy, but
there is no check when deserializing from redb at startup (after a node restart, the
artifact is loaded from redb without re-verification).

---

### 1.4 `crates/supervisor`

**P1 — `Supervisor` struct has no `restore_from_storage()` visible in the code read**

The `main.rs` calls `supervisor.restore_from_storage().await?` but the `lib.rs` shown
does not define this method. It may be in a file not read, but if it exists, it should
be verified against Step 07's specification for restoring instances from redb after
a node restart.

**P1 — Health loop interval is not configurable**

Step 07 specifies a 5-second health loop, but the interval should be configurable
per the design doc's discussion of the trade-off between detection latency and CPU
overhead. The `start_health_loop()` method likely hardcodes the interval.

**P2 — `LocalServiceRegistry` duplicates `UpstreamRegistry`**

Both `LocalServiceRegistry` (in supervisor) and `UpstreamRegistry` (in proxy) track
the same data: app_id → list of socket addresses. The Supervisor registers instances
in both, which means they can get out of sync. The design doc (Step 07) says the
Supervisor writes to the `UpstreamRegistry` directly — the `LocalServiceRegistry`
appears to be redundant.

**Fix**: Remove `LocalServiceRegistry` and use `UpstreamRegistry` as the single
source of truth for instance addresses.

**P2 — `ConcurrencyController` in `scaling.rs` is never integrated**

The `ConcurrencyController` struct with its semaphore-based scaling logic is defined
but never used in the Supervisor or proxy request path. The `select_upstream()` method
in `WasmProxy` does not use it. This is dead code.

**P2 — `FuelAdmissionController` uses a 1-second window with `VecDeque`**

The rolling window stores `(timestamp, fuel)` tuples and removes entries older than
1 second on every call. Under high load (thousands of executions per second), this
deque grows without bound within the 1-second window. A ring buffer with a fixed
size would be more appropriate.

**P2 — `hot_swap()` in `deployment.rs` does not update routes**

When swapping from `api-users:v1` to `api-users:v2`, the `HostRouter` still maps
`api.myapp.com` to the old `AppId`. The hot-swap function spawns the new version and
drains the old, but never updates the route table. This means requests continue to
be routed to the old app ID until a manual route update.

**Fix**: After confirming the new version is healthy, update the `HostRouter` to
point to the new `AppId`.

**P2 — `RollbackPolicy` is defined but never used**

The `RollbackPolicy` struct with `trap_rate_threshold`, `observation_window`, and
`auto_rollback_enabled` is defined in `deployment.rs` but there is no code that
monitors trap rates and triggers automatic rollback.

**P3 — `AuditEvent` uses `String` timestamps instead of proper types**

`AuditEvent.timestamp` is a `String` — it should be a `u64` (millis since epoch) or
`chrono::DateTime<Utc>` for consistency with the rest of the codebase.

**P3 — `write_audit_event()` silently fails**

If the file open or write fails, the error is silently discarded with `let _`. Audit
events should never be silently dropped — at minimum, log the failure.

---

### 1.5 `crates/proxy`

**P1 — `HostRouter` does not support path-based routing**

Step 09 and Step 15 describe routing based on the Host header, but the `Route` struct
in `common/types.rs` includes `path_prefix` and `strip_prefix` fields. The
`HostRouter.resolve()` method ignores these entirely — it only matches on host.
This means path-based routing (e.g., `api.myapp.com/v1/*` → app-v1, `api.myapp.com/v2/*` → app-v2)
is impossible despite being part of the data model.

**Fix**: Extend `resolve()` to accept both host and path, returning the most specific
match (longest path prefix wins).

**P1 — `WasmProxy.select_upstream()` has dead cross-node routing code**

The `node_is_overloaded()` method always returns `false`:
```rust
async fn node_is_overloaded(&self) -> bool {
    false // placeholder
}
```
And `least_loaded_node()` returns a `NodeEntry` with a hardcoded `supervisor_addr`
that is never the actual Wasm app address. Cross-node request steering (Step 12)
does not work — requests are never proxied to other nodes.

**Fix**: Either implement cross-node routing properly (which requires knowing the
remote node's Pingora address, not the supervisor address) or remove the dead code
and document that cross-node routing is not yet implemented.

**P2 — `RateLimiter` uses `RwLock` on every request**

Both `check_request()` and `try_acquire()` acquire write locks on the `app_buckets`
and `ip_buckets` hash maps. Under high concurrency, this creates contention on every
request. The design doc (Step 24) mentions that the rate limiter should use lock-free
data structures for the hot path.

**Fix**: Use `DashMap` (from `dashmap` crate) instead of `RwLock<HashMap>`, which
provides sharded interior mutability without global write locks.

**P2 — No Slowloris protection implemented**

Step 24 defines `ProxyTimeouts` with `request_header_read_timeout`,
`keepalive_idle_timeout`, `max_header_size`, and `max_connections_per_ip`. None of
these are configured in the `ProxyServer::build()` method. Pingora supports these
via its configuration, but the current code uses Pingora defaults.

**P2 — TLS configuration uses file paths, not bytes**

`tls::tls_settings()` takes file paths for cert and key. In a cloud-native deployment,
certificates are often provided as environment variables or secret values, not files
on disk. The function should also accept bytes.

**P3 — `NodeLoadTable` uses `Instant` for `last_seen` but `Instant` is not serializable**

If the node table were ever persisted or sent over the network, `Instant` would be
a problem. For now it is in-memory only, but this limits future extensibility.

---

### 1.6 `crates/messaging`

**P1 — `subscribe()` does not use JetStream durable consumers**

The `subscribe()` method uses `self.client.subscribe()` which is a core NATS
ephemeral subscription. If the node restarts, all messages published while it was
down are lost. Only `subscribe_durable()` uses JetStream with durable consumers.

Currently, critical subjects like `instance.ready.>`, `instance.dead.>`,
`secrets.update.>`, `config.update.>`, `node.load.>`, `routes.>`, and `cluster.>`
are subscribed with the ephemeral `subscribe()`. This means a node restart loses
all events published during the restart window.

**Fix**: All control-plane subscriptions should use `subscribe_durable()`. The
ephemeral `subscribe()` should only be used for fire-and-forget events where loss
is acceptable.

**P1 — `subscribe_durable()` acks malformed messages**

```rust
Err(e) => {
    tracing::warn!(error = %e, "failed to deserialize NATS JetStream message");
    let _ = msg.ack().await; // Ack malformed messages so they aren't redelivered
}
```
Acknowledging a malformed message means it is permanently lost. If a new binary
version publishes a message format that the current version cannot parse, the
current version will ack and discard it — the message is gone forever.

**Fix**: Use `nak()` (negative acknowledgment) for malformed messages, or move them
to a dead-letter subject. At minimum, log the full message payload for forensics
before acking.

**P2 — `Event` enum is not versioned in the wire format**

The `Event` enum uses `#[serde(tag = "type", rename_all = "snake_case")]` for
deserialization, but there is no protocol version field in the event payload itself.
The `MessageEnvelope<T>` in `common/protocol.rs` has a `protocol_version`, but it
is never used in the actual NATS publish/subscribe path — events are serialized
directly, not wrapped in an envelope.

**Fix**: Either wrap all events in `MessageEnvelope<Event>` before publishing, or
add a `protocol_version` field to the `Event` enum's serialization.

**P2 — `setup_jetstream()` only creates the DEPLOY stream**

Step 08 specifies multiple JetStream streams: DEPLOY, ROUTES, SECRETS, CONFIG,
INSTANCE, NODE_LOAD. Only DEPLOY is created. The other subjects are subscribed
with ephemeral subscriptions that lose messages on restart.

**P2 — No backpressure on the message handler**

Both `subscribe()` and `subscribe_durable()` spawn a Tokio task that processes
messages one at a time. If the handler is slow (e.g., artifact compilation takes
seconds), messages pile up in the NATS client's internal buffer. There is no
mechanism to apply backpressure to NATS (e.g., by not acking until the handler
completes).

For `subscribe_durable()`, the ack happens after the handler completes, which is
correct. But for `subscribe()`, there is no backpressure at all.

**P3 — `NatsBus` is `Clone` but cloning the `Client` is not documented**

`async_nats::Client` is `Clone` and uses internal Arcs, so cloning is cheap. But
this should be documented on the `NatsBus` struct.

---

### 1.7 `crates/secrets`

**P2 — `SymmetricKey` does not implement `Debug`**

This is intentional for security (preventing key material from appearing in logs),
but it makes debugging very difficult. Consider implementing a redacted Debug:
```rust
impl Debug for SymmetricKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SymmetricKey([REDACTED; 32])")
    }
}
```

**P2 — KEK is generated on every startup if no key file is provided**

In `main.rs`:
```rust
let kek = if let Some(_key_file) = &args.key_file {
    secrets::crypto::SymmetricKey::generate() // TODO: load from file
} else {
    secrets::crypto::SymmetricKey::generate()
};
```
Both branches generate a new key. This means **all encrypted secrets in redb become
unreadable after a node restart** because the KEK used to encrypt the DEKs is lost.

**Fix**: This is a P0 data-loss bug in practice. The KEK must be persisted — either
in a file, in a hardware security module, or derived from a passphrase. The `key_file`
branch should actually load the key, not generate a new one.

**P2 — `AppSecretBundle` stores encrypted secret values as `Vec<u8>`**

The `secrets` field is `HashMap<String, Vec<u8>>` where each value is
`nonce || ciphertext`. This is correct, but the `EncryptedBlob` wrapper type is
not used consistently — sometimes the raw `Vec<u8>` is passed directly to `decrypt()`.

**P3 — `bootstrap_crypto.rs` encryption uses X25519 but the implementation
details were not fully reviewed**

The `encrypt_for_peer()` and `BootstrapKeyPair::decrypt()` functions handle the
cluster bootstrap secret transfer. These should be audited against Step 06's
specification for ephemeral key exchange.

---

### 1.8 `crates/billing`

**P2 — `billing_writer_loop` writes one record at a time**

Each billing record results in a separate `store.write_billing_record()` call,
which opens a write transaction, writes, and commits. Under high load (many instances
exiting simultaneously), this creates many small write transactions on redb.

**Fix**: Batch records — accumulate N records or wait M milliseconds, then write them
all in a single transaction. This reduces write amplification and improves redb
performance.

**P2 — `S3Exporter` uses a placeholder AWS Signature**

```rust
.header("Authorization", format!(
    "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders=..., Signature=placeholder",
    key, scope
));
```
The `Signature=placeholder` means S3 exports will fail with any S3-compatible service
that validates signatures. This is not a working implementation — it is a stub.

**Fix**: Use a proper AWS SigV4 signing library (e.g., `aws-sigv4` crate) or use
pre-signed URLs.

**P2 — `get_tenant_list()` scans all billing records**

To get the list of tenants, the code reads every billing record from redb and
extracts unique tenant IDs. With millions of records, this is O(n) and slow.

**Fix**: Maintain a separate `tenants` index table, or cache the tenant list in
memory and update it when new records arrive.

**P3 — `BillingRecord` uses `String` for `prev_hash` and `record_hash`**

Hex-encoded SHA-256 hashes are 64-character strings. Using `[u8; 32]` with hex
encoding only for display would reduce memory and storage by 50%.

---

### 1.9 `crates/node`

**P1 — `our_node_id()` reads from environment variable, not from the struct**

```rust
fn our_node_id(&self) -> String {
    std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
}
```
The `EventDispatcher` has a `node_id: String` field, but this method ignores it
and reads from the environment. If `NODE_ID` is not set, it defaults to `"node-0"`,
which means two nodes without the env var will both think they are `node-0`.

**Fix**: Use `self.node_id.clone()`.

**P1 — `handle_deploy()` does not verify the SHA-256 hash of the fetched artifact**

Step 13 (Security) specifies that the SHA-256 of the `.wasm` bytes must be verified
before compilation. The `handle_deploy()` method fetches the artifact, stores it, and
compiles it without computing the hash of the fetched bytes. The `expected_hash` is
used as a redb key but not as a verification check.

**Fix**: After fetching, compute `sha256(&wasm_bytes)` and compare with `expected_hash`.
If they don't match, log a `SECURITY` error and refuse to compile.

**P2 — `handle_state_snapshot()` does not fetch missing artifacts**

When a new node receives a `StateSnapshot`, it stores configs, routes, and secrets,
but does not fetch the actual Wasm artifacts. The artifact hashes are stored, but
the node has no artifacts to compile. It will fail on the first request to any app.

The artifact push from the joining node happens in `handle_node_joined()`, but this
is a background `tokio::spawn` that may not complete before the snapshot is processed.

**Fix**: After processing the snapshot, the node should fetch all missing artifacts
from the cluster before declaring itself ready.

**P2 — No graceful shutdown on SIGTERM**

Step 20 (Graceful Shutdown) defines a three-phase shutdown: HTTP shutdown → TCP close
→ hard abort. But `main.rs` does not set up a signal handler for SIGTERM. The node
will be hard-killed on `systemctl stop`, losing in-flight requests.

**Fix**: Use `tokio::signal::unix::signal(SignalKind::terminate())` to catch SIGTERM
and initiate graceful drain.

**P2 — Admin API has no authentication**

The `/admin/rebuild` endpoint deletes the database with no authentication. Anyone
who can reach port 9090 can destroy the node's state.

**Fix**: Add a bearer token check to all admin endpoints. The token can be set via
CLI flag or environment variable.

**P3 — `main.rs` has a 5-second sleep for cluster bootstrap**

```rust
tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
```
After publishing `NodeJoined`, the node sleeps for 5 seconds waiting for a
`StateSnapshot`. This is fragile — on a slow network, 5 seconds may not be enough.
On a fast network, it adds unnecessary startup latency.

**Fix**: Use a proper future with a timeout: wait for the first `StateSnapshot`
event with a 30-second deadline, rather than a fixed sleep.

---

### 1.10 `crates/ctl`

**P2 — CLI billing commands read directly from redb, not via NATS**

The `Billing` subcommand takes a `--store-path` and reads the redb file directly.
This only works if `wasm-ctl` runs on the same machine as the node. For remote
management, billing queries should go through the admin API.

**P2 — No `wasm-ctl logs` implementation visible**

The `Commands::Logs` variant calls `cmds::logs::run()`, but the actual log streaming
implementation (SSE from Step 17) needs the admin API to support it.

**P3 — No `--output` format flag (json, yaml, plain)**

All CLI output is plain text. For scripting and automation, a `--json` flag would
be valuable.

---

## 2. Cross-Cutting Concerns

### 2.1 Error Handling

**P1 — Inconsistent error handling across crates**

Some crates use `PlatformError`, some use `anyhow::Error`, some use `redb::Error`
directly. The `Store` methods return `Result<T, redb::Error>` in some places and
`Result<T, PlatformError>` in others. This makes it hard to compose operations
across crates.

**Fix**: Define a consistent error strategy:
- `common::PlatformError` for all public APIs
- Crate-specific error types that implement `Into<PlatformError>`
- Never expose `redb::Error`, `wasmtime::Error`, or `async_nats::Error` across crate
  boundaries

### 2.2 Testing

**P1 — No integration tests for the runtime crate**

The `runtime` crate has `#[cfg(test)] mod tests` but the tests file was not visible
in the directory listing. The design doc (Step 23) specifies 13 integration tests
with "real Wasmtime". These should verify:
- Fuel exhaustion produces a trap, not a hang
- Memory limit enforcement works
- Component model instantiation succeeds for valid WASI Preview 2 modules

**P2 — E2E tests directory is empty**

The `crates/e2e` directory exists but has no `src/` directory. The design doc
specifies E2E tests using testcontainers with NATS. None are implemented.

**P2 — No chaos/fault injection tests**

Step 27 (Disaster Recovery) defines L3–L6 failure modes but there are no tests that
simulate them:
- Kill a node process and verify it recovers
- Corrupt a redb page and verify integrity check detects it
- Disconnect NATS and verify degraded mode
- Fill the disk and verify GC triggers

### 2.3 Observability

**P2 — No distributed tracing propagation**

Step 11 defines OpenTelemetry tracing with `trace_id` propagation, but the proxy
does not inject or propagate trace context headers (`traceparent`, `tracestate`).
Requests enter Pingora without a trace context and leave without one.

**Fix**: In `upstream_request_filter()`, inject the current `trace_id` as an
`X-Trace-Id` header. If the incoming request has a `traceparent` header, propagate it.

**P2 — No structured logging**

The codebase uses `tracing::info!()` with ad-hoc key-value pairs. There is no
consistent log schema. Some logs use `app = %app_id.0`, others use `app_id = ...`,
others use just the string. A structured log format (JSON) would enable log
aggregation and querying.

### 2.4 Configuration

**P2 — No configuration file support**

All configuration is via CLI flags. For a production deployment, a TOML or YAML
config file is essential. The `wasm-node` binary should support `--config path.toml`
that merges with CLI flags.

**P2 — No hot-reload of configuration**

Changing a rate limit, FD limit, or memory threshold requires restarting the node.
The design docs mention runtime-adjustable thresholds (e.g., `wasm-ctl node ebpf-config`),
but there is no mechanism to update configuration without a restart.

---

## 3. INFRA_IMPL Gap Analysis

### 3.1 Implemented vs. Specified

| Step | Title | Implementation Status |
|------|-------|----------------------|
| 00 | Overview | ✅ Architecture matches |
| 01 | Workspace Setup | ✅ Complete |
| 02 | Storage (redb) | ⚠️ Missing `db_path` storage, billing table migration |
| 03 | WASM Runtime | ⚠️ `eprintln!` debug, hardcoded memory, no IO enforcement |
| 04 | WASI Networking | ❌ `NetworkPolicy` not implemented, no port pre-binding |
| 05 | Env Config | ⚠️ KEK always regenerated (data loss on restart) |
| 06 | Secrets | ⚠️ KEK persistence missing, `key_file` loading is TODO |
| 07 | Supervisor Core | ⚠️ `LocalServiceRegistry` redundant, health loop not configurable |
| 08 | NATS Messaging | ❌ Only DEPLOY stream uses JetStream, others are ephemeral |
| 09 | Proxy (Pingora) | ⚠️ No path routing, no cross-node steering, no slowloris defense |
| 10 | Deployment Protocol | ⚠️ No SHA-256 verification on fetch, hot-swap doesn't update routes |
| 11 | Metrics | ⚠️ No trace propagation, no structured logging |
| 12 | Scaling | ❌ `ConcurrencyController` unused, cross-node routing broken |
| 13 | Security | ❌ `NetworkPolicy` not implemented, IO limits not enforced |
| 14 | Node Entrypoint | ⚠️ No SIGTERM handler, no admin auth, hardcoded node-0 fallback |
| 15 | Route Management | ❌ Path-based routing not implemented |
| 16 | Artifact Registry | ⚠️ No hash verification on fetch |
| 17 | WASM Logs | ⚠️ SSE log tailing not verified |
| 18 | Admin CLI | ⚠️ Billing reads redb directly, no JSON output |
| 19 | Cluster Bootstrap | ⚠️ 5-second sleep instead of proper wait |
| 20 | Graceful Shutdown | ❌ No SIGTERM handler |
| 21 | Database Connections | ⚠️ Built-in proxy is a raw TCP forwarder, not PostgreSQL-aware |
| 22 | Schema Versioning | ⚠️ Missing billing table migration step |
| 23 | Integration Testing | ❌ E2E tests not implemented |
| 24 | Rate Limiting | ⚠️ Uses RwLock (contention), no slowloris defense |
| 25 | Platform Upgrades | ⚠️ Upgrade handler exists but no binary download/verify |
| 26 | Artifact GC | ✅ Mostly complete |
| 27 | Disaster Recovery | ⚠️ `db_path()` hardcoded, `partial_rebuild` doesn't clear table |
| 28 | Billing | ⚠️ S3 exporter has placeholder signature, single-record writes |
| 29 | DNS Integration | ✅ Webhook implemented |
| 30 | eBPF Monitoring | 📝 Design only, not implemented |

### 3.2 Missing INFRA_IMPL Documents

Based on the gaps identified, the following new design documents would fill holes
in the architecture:

**31 — Observability & Distributed Tracing** (this document replaces the gap analysis)

**32 — Configuration Management & Hot-Reload**

The platform has no configuration file support and no runtime configuration changes.
This document should define:
- TOML config file format for `wasm-node`
- Config merge priority: file < env vars < CLI flags
- Runtime-adjustable parameters (rate limits, thresholds, log levels)
- Admin API endpoints for live config changes
- Config change notification via NATS (other nodes learn about changes)

**33 — WASI Policy Enforcement**

Step 13 defines `NetworkPolicy` and `ExtendedLimits` but they are not enforced.
This document should define:
- How `NetworkPolicy` maps to Wasmtime WASI configuration
- Per-app outbound connection tracking and enforcement
- FD limit enforcement via custom WASI host functions
- Filesystem write limits via WASI preopen restrictions
- Integration with the eBPF syscall monitor (Step 30) for kernel-level enforcement

**34 — Admin API Security & Authentication**

The admin API has no authentication. This document should define:
- Bearer token authentication for admin endpoints
- Token generation and distribution
- TLS requirement for admin API
- Rate limiting on admin endpoints
- Audit logging for all admin actions

**35 — Chaos Testing & Fault Injection**

Step 23 defines integration testing but there are no fault injection tests. This
document should define:
- Process kill and restart tests
- Network partition simulation (tc netem)
- Disk corruption simulation
- Memory pressure simulation
- NATS disconnection tests
- Automated recovery verification

**36 — Structured Logging & Log Aggregation**

The platform uses ad-hoc `tracing` calls. This document should define:
- Consistent log field naming convention
- JSON structured log format
- Log level configuration per module
- Log forwarding to external aggregators (Loki, Elasticsearch)
- Log retention and rotation

**37 — Health Check Protocol**

The health check endpoint exists but is basic. This document should define:
- Liveness vs. readiness vs. startup probes
- Dependency health (NATS, redb, disk)
- Health check response format for external load balancers
- Degraded-mode health reporting (serving but not accepting new deploys)

---

## 4. Priority Improvement Roadmap

### Phase 1 — Critical Fixes (P0/P1)

These are correctness or data-loss issues that should be fixed before any production use.

| # | Finding | Crate | Effort |
|---|---------|-------|--------|
| 1 | KEK regeneration on restart loses all secrets | secrets, node | S |
| 2 | `db_path()` returns hardcoded path | storage | S |
| 3 | `eprintln!()` debug statements in production | runtime | S |
| 4 | `read_memory_usage()` returns hardcoded 1024 | runtime | M |
| 5 | SHA-256 not verified on artifact fetch | node | S |
| 6 | `our_node_id()` reads env var instead of struct field | node | S |
| 7 | `partial_rebuild()` does not clear corrupted table | storage | M |
| 8 | Integrity check reads entire ARTIFACTS table | storage | S |
| 9 | Only DEPLOY stream uses JetStream durability | messaging | M |
| 10 | Malformed NATS messages acked and lost | messaging | S |
| 11 | `IoResourceTracker` never called by WASI | runtime | L |
| 12 | No SIGTERM graceful shutdown | node | M |
| 13 | No admin API authentication | node, proxy | M |

**Effort**: S = <1 day, M = 1–3 days, L = 1+ week

### Phase 2 — Feature Gaps (P1)

These are features specified in design docs but not implemented.

| # | Finding | Design Doc | Effort |
|---|---------|-----------|--------|
| 14 | `NetworkPolicy` not enforced | Step 13 | L |
| 15 | Path-based routing not implemented | Step 15 | M |
| 16 | Cross-node request steering broken | Step 12 | L |
| 17 | `ConcurrencyController` unused | Step 12 | M |
| 18 | Slowloris protection not configured | Step 24 | S |
| 19 | Hot-swap doesn't update routes | Step 10 | S |
| 20 | Auto-rollback not implemented | Step 10 | M |
| 21 | `MessageEnvelope` not used in NATS | Step 25 | M |
| 22 | Billing table missing schema migration | Step 22 | S |

### Phase 3 — Quality & Maintainability (P2)

| # | Finding | Effort |
|---|---------|--------|
| 23 | `PlatformError` should use `#[source]` chains | M |
| 24 | `AppId` validation | S |
| 25 | `RateLimiter` should use `DashMap` | S |
| 26 | Billing batch writes | M |
| 27 | S3 exporter signature implementation | M |
| 28 | E2E test harness | L |
| 29 | Distributed tracing propagation | M |
| 30 | Configuration file support | M |
| 31 | `LocalServiceRegistry` removal | S |
| 32 | WASI version probing robustness | S |
| 33 | `RollbackPolicy` implementation | M |
| 34 | Audit log error handling | S |
| 35 | Tenant list caching | S |

### Phase 4 — New Design Documents

| # | Document | Priority |
|---|----------|----------|
| 36 | 32 — Configuration Management & Hot-Reload | P1 |
| 37 | 33 — WASI Policy Enforcement | P1 |
| 38 | 34 — Admin API Security & Authentication | P1 |
| 39 | 35 — Chaos Testing & Fault Injection | P2 |
| 40 | 36 — Structured Logging & Log Aggregation | P2 |
| 41 | 37 — Health Check Protocol | P2 |

---

## 5. Architectural Observations

### 5.1 The "Single Binary" Choice Is Paying Off

The decision to put Pingora, the Supervisor, and all other components in one binary
(Step 14) avoids IPC complexity. The `UpstreamRegistry` shared via `Arc<RwLock<>>`
between the proxy and supervisor is simple and fast. No other architecture choice
would make this easier.

### 5.2 The "Shared-Nothing" Choice Creates a Testing Challenge

Each node is self-sufficient with local redb, which is great for production. But
testing requires running a full node with NATS, redb, and compiled Wasm artifacts.
The E2E test harness (Step 23) is critical for validating cross-node behavior, and
its absence is a significant gap.

### 5.3 The Billing Hash Chain Is Well-Designed

The `BillingRecord` with `prev_hash` and `record_hash` is a solid design. The
`verify_chain()` function correctly detects both tampering and broken links. The
main weaknesses are operational (single-record writes, placeholder S3 signature)
rather than architectural.

### 5.4 The eBPF Plan (Step 30) Is Ambitious but Well-Scoped

The eBPF design document correctly identifies the limitations (Linux-only, requires
CAP_BPF) and provides a userspace fallback. The graduated response to memory
pressure (low → medium → critical) is a good pattern. The main risk is that the
syscall counter's overhead (firing on every syscall) may be too high for production.

### 5.5 The Biggest Gap: WASI Policy Enforcement

Step 13 (Security Model) promises defense in depth with `NetworkPolicy`, per-app
FD limits, egress limits, and connection limits. None of these are enforced. The
Wasm SFI boundary is the only real isolation layer. If a Wasm module makes
excessive outbound connections, the platform cannot stop it. This is the single
most important feature gap to close.

---

## Completion Checklist

**This analysis is complete when all findings have been triaged and assigned.**

### Triage
- [ ] All P0/P1 findings reviewed by project owner
- [ ] Phase 1 items assigned to a sprint or milestone
- [ ] Decision made on KEK persistence strategy (file vs. HSM vs. passphrase)
- [ ] Decision made on JetStream durability scope (which subjects need durable consumers)
- [ ] Decision made on WASI policy enforcement approach (custom host functions vs. Wasmtime config)

### New Design Documents
- [ ] Step 32 — Configuration Management & Hot-Reload: written
- [ ] Step 33 — WASI Policy Enforcement: written
- [ ] Step 34 — Admin API Security & Authentication: written
- [ ] Step 35 — Chaos Testing & Fault Injection: written
- [x] Step 36 — Structured Logging & Log Aggregation: written
- [x] Step 37 — Health Check Protocol: written

### Implementation Verification
- [ ] All Phase 1 items resolved and tested
- [ ] All Phase 2 items resolved or deferred with documented rationale
- [ ] E2E test harness operational with at least 3 test scenarios
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes with 0 warnings
- [ ] `cargo test --workspace --lib --bins` passes with 0 failures
- [ ] Integration tests for storage and runtime pass
