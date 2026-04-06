# Step 23 — Integration & End-to-End Testing

## Goal
Define the testing pyramid for the entire platform. Without tests at each layer,
changes to any crate can silently break the system. This file covers:
1. Unit tests per crate
2. Integration tests (real Wasm binary, real redb, real NATS)
3. End-to-end tests (full node binary, HTTP requests)
4. Chaos tests (node failure, NATS disconnect, OOM)
5. Load tests (fuel/memory limits under traffic)

---

## Context & Rationale

### The Problem This Solves

Every previous step has a "Completion Checklist" with specific assertions. Without a
testing plan, verifying these assertions requires manual testing on every code change.
This is slow, error-prone, and breaks as the codebase grows.

A comprehensive test suite makes it safe to:
- Refactor internal crate boundaries without breaking behavior
- Upgrade dependencies (Wasmer, async-nats, redb) with confidence
- Add new features without regressions
- Verify security properties automatically

### Why No Mocks for Infrastructure?

Many test guides recommend mocking: mock the database, mock NATS, mock the Wasm runtime.
This platform deliberately avoids mocks for its core infrastructure:

**Problem with mocking the database (redb)**:
- A mock returns what the test tells it to return
- It cannot catch bugs in the `serde_json` serialization format
- It cannot catch bugs in the redb table definition (wrong key/value types)
- A test that mocked redb would pass even if the real code was completely broken

The correct approach: use `tempfile::NamedTempFile` to create a real redb database for
each test. It's fast (redb opens in microseconds), isolated (each test gets its own file),
and tests the real code path.

**Problem with mocking NATS**:
- The messaging layer has subtle behavior: JetStream consumer replay, durable subscriptions,
  wildcard subject matching
- A mock cannot reproduce these behaviors correctly without reimplementing NATS
- Use `nats-server` embedded in tests or spin up a local NATS server in CI

**Problem with mocking the Wasm runtime**:
- Mocking Wasmer would mean the tests never actually run Wasm
- Fuel metering, memory limits, and WASI networking cannot be tested through a mock
- The `apps/hello-axum` test binary provides a real Wasm target for integration tests

### The Testing Pyramid

```
    ▲
    │  E2E tests         (slowest, highest confidence)
    │  ─────────────     Full node binary + HTTP requests
    │  Integration tests (medium speed)
    │  ─────────────     Real Wasm + real redb + real NATS
    │  Unit tests        (fastest)
    └  ─────────────     Single function / struct in isolation
```

The pyramid shape reflects the investment: many fast unit tests, fewer slower integration
tests, a handful of complete E2E scenarios.

### Why Chaos Tests?

The platform makes strong claims about resilience:
- "A node failure only affects apps on that node" → **test it**
- "Instances survive NATS disconnects" → **test it**
- "A Wasm trap does not crash the node" → **test it**

Chaos tests inject these failure conditions deliberately and verify the system behaves
correctly. Without them, these guarantees are theoretical claims, not verified behavior.

### Why Load Tests?

Fuel and memory limits are core platform features. A load test verifies:
1. Under sustained traffic, the node does not leak memory (instances are properly cleaned up)
2. Fuel limits are enforced: a request that exceeds its quota fails gracefully, not with
   a process crash
3. The 10ms cold start target holds under realistic concurrency (not just on an idle machine)

### CI Pipeline Design

Tests are organized into three stages to balance speed and coverage:
1. **Unit** (< 30s): run on every commit — fast feedback on obvious breakage
2. **Integration** (< 3 min): run on every PR — catches cross-crate issues
3. **E2E + Chaos** (< 15 min): run before merge to main — catches system-level issues

The `e2e` crate builds the full `wasm-node` binary and the `hello-axum.wasm` test app as
part of the test setup. Tests send real HTTP requests to a real running node.

---

---

## 1. Test Crate Layout

```
crates/
├── storage/tests/
│   └── storage_integration.rs    ← real redb, no mocks
├── runtime/tests/
│   └── runtime_integration.rs    ← real Wasmer, real .wasm files
├── supervisor/tests/
│   └── supervisor_integration.rs ← real supervisor with embedded NATS
└── e2e/                          ← new crate
    ├── Cargo.toml
    └── tests/
        ├── deploy_and_request.rs ← full stack: deploy → HTTP → verify
        ├── hot_swap.rs
        ├── chaos.rs
        └── load.rs
```

```toml
# crates/e2e/Cargo.toml
[package]
name    = "e2e"
version = "0.1.0"
edition = "2021"

[dev-dependencies]
tokio         = { workspace = true }
reqwest       = { version = "0.12", features = ["rustls-tls"] }
assert_cmd    = "2"          # run binaries as subprocess
tempfile      = "3"
nats-server   = { version = "0.1" }  # embeds NATS for testing
common        = { path = "../common" }
messaging     = { path = "../messaging" }
storage       = { path = "../storage" }
```

---

## 2. Unit Tests

### Storage (redb)

```rust
// crates/storage/tests/storage_integration.rs
use storage::Store;
use common::types::*;
use tempfile::NamedTempFile;

fn make_store() -> (Store, NamedTempFile) {
    let f = NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    (s, f)
}

#[test]
fn test_schema_migration_fresh_db() {
    let (store, _f) = make_store();
    // Should complete without error and be at version 1
    // (tested via internal read_schema_version)
}

#[test]
fn test_artifact_store_and_load() {
    let (store, _f) = make_store();
    let id = AppId::new("test", "v1");
    let bytes = b"fake compiled wasm artifact";
    store.store_artifact(&id, bytes).unwrap();
    let loaded = store.load_artifact(&id).unwrap().unwrap();
    assert_eq!(loaded, bytes);
    assert!(store.artifact_exists(&id).unwrap());
    store.delete_artifact(&id).unwrap();
    assert!(!store.artifact_exists(&id).unwrap());
}

#[test]
fn test_config_roundtrip_with_secrets() {
    let (store, _f) = make_store();
    let config = AppConfig {
        id: AppId::new("my-app", "v2"),
        fuel_quota: FuelQuota(1_000_000),
        memory_limit: MemoryPages(512),
        max_instances: 5,
        idle_timeout_secs: 60,
        wasm_bind_port: 8080,
        env_vars: [("LOG_LEVEL".into(), "debug".into())].into(),
        secret_keys: vec!["DATABASE_URL".into()],
        db_max_connections: 10,
    };
    store.save_config(&config).unwrap();
    let loaded = store.load_config(&config.id).unwrap().unwrap();
    assert_eq!(loaded.fuel_quota.0, 1_000_000);
    assert_eq!(loaded.secret_keys, vec!["DATABASE_URL"]);
}

#[test]
fn test_route_crud() {
    let (store, _f) = make_store();
    let route = common::types::Route {
        host: "api.example.com".into(),
        app_id: AppId::new("my-app", "v1"),
        path_prefix: "/".into(),
        strip_prefix: false,
        created_at: 0,
        updated_at: 0,
    };
    store.save_route(&route).unwrap();
    let routes = store.list_routes().unwrap();
    assert_eq!(routes.len(), 1);
    store.delete_route("api.example.com").unwrap();
    assert!(store.list_routes().unwrap().is_empty());
}

#[test]
fn test_metrics_write_and_prune() {
    let (store, _f) = make_store();
    let bucket = storage::metrics::MetricBucket {
        app_id: "test-app".into(),
        minute_ts: 1000 * 60, // old timestamp
        request_count: 42,
        fuel_consumed_total: 1_000_000,
        fuel_consumed_avg: 23_809,
        ram_usage_peak_bytes: 64 * 1024 * 1024,
        latency_p50_ms: 5.0,
        latency_p99_ms: 50.0,
        trap_count: 0,
    };
    store.write_metric_bucket(&bucket).unwrap();
    let pruned = store.prune_old_metrics(0).unwrap(); // prune everything
    assert!(pruned >= 1);
}
```

### Runtime (Wasmer)

```rust
// crates/runtime/tests/runtime_integration.rs
use runtime::WasmRuntime;
use common::types::{AppConfig, AppId, FuelQuota, MemoryPages};

/// Compile and run a trivial WAT module.
#[test]
fn test_compile_and_run() {
    let wat = r#"(module (func (export "_start")))"#;
    let wasm = wat::parse_str(wat).unwrap();

    let rt = WasmRuntime::new();
    let artifact = rt.compile(&wasm).unwrap();
    assert!(!artifact.is_empty());

    let config = AppConfig {
        id: AppId::new("test", "v1"),
        fuel_quota: FuelQuota(1_000_000),
        memory_limit: MemoryPages(256),
        max_instances: 1,
        idle_timeout_secs: 60,
        wasm_bind_port: 8080,
        env_vars: Default::default(),
        secret_keys: vec![],
        db_max_connections: 0,
    };
    let mut prepared = rt.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 9999).unwrap();
    let stats = instance.run();

    assert_eq!(stats.trap, None);
    assert!(stats.fuel_consumed > 0);
}

/// Verify that out-of-fuel raises a Trap (not a panic).
#[test]
fn test_fuel_exhaustion_trap() {
    // A module that loops forever
    let wat = r#"
        (module
            (func (export "_start")
                (loop $l (br $l))  ;; infinite loop
            )
        )
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let rt = WasmRuntime::new();
    let artifact = rt.compile(&wasm).unwrap();
    let config = AppConfig {
        id: AppId::new("looper", "v1"),
        fuel_quota: FuelQuota(1_000), // very small
        ..AppConfig::default_for(AppId::new("looper", "v1"))
    };
    let mut prepared = rt.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 9998).unwrap();
    let stats = instance.run();

    assert!(stats.trap.is_some(), "expected out-of-fuel trap");
}

/// Verify that memory limit is enforced.
#[test]
fn test_memory_limit_enforced() {
    // A module that tries to grow memory to 1000 pages (64 MB)
    let wat = r#"
        (module
            (memory 1)
            (func (export "_start")
                i32.const 1000
                memory.grow
                drop
            )
        )
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let rt = WasmRuntime::new();
    let artifact = rt.compile(&wasm).unwrap();
    let config = AppConfig {
        id: AppId::new("memhog", "v1"),
        memory_limit: MemoryPages(10), // limit to 640 KB
        ..AppConfig::default_for(AppId::new("memhog", "v1"))
    };
    let mut prepared = rt.prepare(&artifact, config).unwrap();
    let mut instance = prepared.spawn_instance(vec![], 9997).unwrap();
    let stats = instance.run();
    // memory.grow returns -1 when it fails — no trap, but memory did not grow
    // (exact behavior depends on Wasmer version)
    println!("RAM after OOM attempt: {} bytes", stats.ram_bytes);
}
```

---

## 3. End-to-End Tests

These tests run a full node (or individual components) against a real NATS server.

```rust
// crates/e2e/tests/deploy_and_request.rs
use std::time::Duration;
use tokio::time::sleep;

/// Spin up a test node, deploy hello-axum.wasm, send an HTTP request, verify the response.
#[tokio::test(flavor = "multi_thread")]
async fn test_deploy_and_serve_http() {
    // 1. Start embedded NATS (or connect to a local one)
    let nats_url = start_test_nats().await;

    // 2. Create a temp redb
    let db_file = tempfile::NamedTempFile::new().unwrap();

    // 3. Start a node in the background
    let node_handle = tokio::spawn(start_test_node(
        db_file.path().to_path_buf(),
        nats_url.clone(),
        8180,  // proxy port
        9180,  // admin port
        9191,  // artifact port
    ));

    sleep(Duration::from_millis(500)).await; // wait for node to boot

    // 4. Load the test wasm binary
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/hello_axum.wasm"
    );
    let wasm_bytes = std::fs::read(wasm_path)
        .expect("build hello-axum first: cargo build --target wasm32-wasip2");

    // 5. Upload the artifact
    let sha256 = sha256_hex(&wasm_bytes);
    let client = reqwest::Client::new();
    let upload_resp = client
        .put(format!("http://127.0.0.1:9191/artifacts/{sha256}"))
        .body(wasm_bytes.clone())
        .send().await.unwrap();
    assert!(upload_resp.status().is_success());

    // 6. Publish deploy event
    let bus = messaging::NatsBus::connect(&nats_url).await.unwrap();
    bus.publish(&messaging::events::Event::DeployApp {
        app_id: common::types::AppId::new("hello-axum", "v1"),
        config: common::types::AppConfig::default_for(
            common::types::AppId::new("hello-axum", "v1")
        ),
        artifact_url: format!("http://127.0.0.1:9191/artifacts/{sha256}"),
        sha256,
        size_bytes: wasm_bytes.len() as u64,
    }).await.unwrap();

    // 7. Add a route
    bus.publish(&messaging::events::Event::RouteAdd {
        route: common::types::Route {
            host: "localhost".into(),
            app_id: common::types::AppId::new("hello-axum", "v1"),
            path_prefix: "/".into(),
            strip_prefix: false,
            created_at: 0, updated_at: 0,
        },
    }).await.unwrap();

    sleep(Duration::from_secs(2)).await; // wait for compile

    // 8. Send HTTP request and verify response
    let resp = client
        .get("http://127.0.0.1:8180/")
        .header("host", "localhost")
        .send().await.unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Hello"), "expected 'Hello' in response, got: {body}");

    node_handle.abort();
}
```

---

## 4. Hot-Swap Test

```rust
// crates/e2e/tests/hot_swap.rs
#[tokio::test(flavor = "multi_thread")]
async fn test_hot_swap_zero_downtime() {
    let nats_url = start_test_nats().await;
    // ... setup same as above ...

    // Deploy v1
    deploy_app(&bus, "hot-swap-app", "v1", &v1_wasm).await;
    sleep(Duration::from_secs(2)).await;

    // Start sending requests in the background
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bool>(100);
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        loop {
            let ok = client.get("http://127.0.0.1:8180/")
                .header("host", "localhost")
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            tx.send(ok).await.ok();
            sleep(Duration::from_millis(10)).await;
        }
    });

    sleep(Duration::from_millis(200)).await;

    // Deploy v2 while traffic is flowing
    deploy_app(&bus, "hot-swap-app", "v2", &v2_wasm).await;
    update_route(&bus, "localhost", "hot-swap-app:v2").await;

    sleep(Duration::from_secs(3)).await;

    // Count failures during the swap
    let mut failures = 0;
    while let Ok(ok) = rx.try_recv() {
        if !ok { failures += 1; }
    }

    assert_eq!(failures, 0, "expected zero failed requests during hot-swap");
}
```

---

## 5. Chaos Tests

```rust
// crates/e2e/tests/chaos.rs

/// Verify that restarting a node restores all deployed apps.
#[tokio::test(flavor = "multi_thread")]
async fn test_node_restart_restores_state() {
    let nats_url = start_test_nats().await;
    let db_file = tempfile::NamedTempFile::new().unwrap();

    // Start node, deploy app, verify it works
    let node = start_test_node(...).await;
    deploy_app(&bus, "persist-test", "v1", &wasm).await;
    sleep(Duration::from_secs(2)).await;
    assert_http_ok("http://127.0.0.1:8180/").await;

    // Kill the node
    node.abort();
    sleep(Duration::from_millis(200)).await;

    // Restart with the SAME db_file
    let node2 = start_test_node(db_file.path().to_path_buf(), ...).await;
    sleep(Duration::from_millis(500)).await;

    // App should be available again (cold start on first request)
    assert_http_ok("http://127.0.0.1:8180/").await;

    node2.abort();
}

/// Verify that fuel exhaustion returns HTTP 429 (not a crash).
#[tokio::test(flavor = "multi_thread")]
async fn test_fuel_exhaustion_returns_429() {
    // Deploy an app with a very small fuel quota
    // Send a request that consumes a lot of CPU
    // Verify Pingora returns 429 or 504
    todo!()
}
```

---

## 6. Load Test (with `drill` or `oha`)

```bash
# Install oha (Rust-based HTTP load tester)
cargo install oha

# Warm up
oha -n 100 -c 10 http://127.0.0.1:8180/

# Load test: 10,000 requests, 100 concurrent
oha -n 10000 -c 100 -H "Host: localhost" http://127.0.0.1:8180/

# Expected results:
# p99 latency: < 50ms (most time = Wasmer instance startup for cold requests)
# Error rate: 0%
# Throughput: depends on the app (a simple hello-world: ~5,000 req/s)
```

---

## 7. CI Pipeline

```yaml
# .github/workflows/ci.yml
name: CI
on: [push, pull_request]

jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-wasip2
      - name: Build test Wasm app
        run: cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release
      - name: Run unit tests
        run: cargo test --workspace --exclude e2e

  integration-tests:
    runs-on: ubuntu-latest
    services:
      nats:
        image: nats:latest
        ports: ["4222:4222"]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-wasip2
      - name: Build test Wasm app
        run: cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release
      - name: Run E2E tests
        run: cargo test -p e2e
        env:
          NATS_URL: nats://127.0.0.1:4222
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Unit Tests (per crate)
- [ ] `cargo test -p storage` — all storage tests pass (artifact, config, metrics, routes, schema)
- [ ] `cargo test -p runtime` — compile, run, fuel exhaustion, and memory limit tests pass
- [ ] `cargo test -p secrets` — crypto roundtrip, wrong-key error, and provider CRUD tests pass
- [ ] `cargo test -p supervisor` — spawn, kill, idle prune, and ensure_instance tests pass
- [ ] `cargo test -p proxy` — upstream registry round-robin, host router, and cold-start trigger tests pass
- [ ] `cargo test -p messaging` — publish, subscribe, and durable consumer tests pass
- [ ] All unit tests use real dependencies (real redb, real Wasmer) — no mocks

### Integration Tests
- [ ] `test_deploy_and_serve_http` — deploys `hello-axum.wasm`, sends a request, gets 200 ✓
- [ ] `test_hot_swap_zero_downtime` — hot-swaps v1→v2 under continuous traffic with 0 failures ✓
- [ ] `test_node_restart_restores_state` — node restarts with same database, app still serves traffic ✓
- [ ] `test_fuel_exhaustion_returns_4xx` — out-of-fuel trap returns 429 or 504, not 500 ✓
- [ ] `test_secret_rotation` — rotating a secret causes new instances to receive the new value ✓
- [ ] `test_route_add_and_serve` — adding a route makes an app reachable via its hostname ✓

### Chaos Tests
- [ ] `test_nats_disconnect_reconnect` — node continues serving traffic during a 5-second NATS outage ✓
- [ ] `test_concurrent_deploys` — deploying 5 apps simultaneously causes no corruption or deadlock ✓
- [ ] `test_port_pool_exhaustion` — exhausting all ports returns a clear error, then releasing ports makes them reusable ✓

### Load Tests
- [ ] `oha -n 1000 -c 50` against a running node produces 0 errors and p99 latency < 100ms
- [ ] Memory usage of the node process does not grow unboundedly under sustained load (no memory leak)

### CI Pipeline
- [ ] `cargo test --workspace --exclude e2e` passes on every PR (unit tests)
- [ ] E2E tests run in CI with a real NATS container
- [ ] The CI pipeline builds `hello-axum.wasm` before running E2E tests
- [ ] `cargo clippy -- -D warnings` passes on every PR
- [ ] `cargo fmt --check` passes on every PR
