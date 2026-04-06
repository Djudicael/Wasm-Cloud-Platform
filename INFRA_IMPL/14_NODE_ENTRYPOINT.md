# Step 14 — Node Entrypoint (main.rs)

## Goal
Wire all crates together in `crates/node/src/main.rs`.
This is the final assembly — one binary, one command, full node.

---

## Context & Rationale

### The Problem This Solves

All the previous steps built independent crates in isolation. None of them know about
each other at the crate level (by design — to avoid circular dependencies). This step
is the **composition root**: the single place where all components are instantiated and
connected into a working system.

Without this step, you have a collection of libraries but no runnable node.

### Why One Binary (Not Multiple Services)?

A traditional microservices approach would split this into separate processes:
- A proxy process (Pingora)
- A supervisor process
- A metrics exporter

This platform deliberately uses a single binary. The reasons:

1. **Shared memory, zero IPC**: The `UpstreamRegistry` is shared between Pingora and
   the Supervisor via `Arc`. In a multi-process design, this would require IPC (Unix sockets,
   shared memory, or a sidecar). Shared memory is faster and simpler.

2. **Single deployment unit**: One `cargo build --release` → one binary → one `systemctl start`.
   No service mesh, no container orchestration needed.

3. **Single failure domain**: If the proxy and supervisor are in the same process, a crash
   takes both down. This sounds bad, but it is actually better: if they are in separate
   processes and the supervisor crashes while the proxy keeps running, the proxy will route
   to dead instances (502 errors) until an operator intervenes.

4. **Operational simplicity**: A single binary with well-defined CLI flags is easy to
   deploy, version, and roll back. There is no question of "which version of proxy
   is compatible with which version of supervisor".

### The Initialization Order Matters

The startup sequence in `main.rs` is not arbitrary — it follows the dependency graph:

```
1. Parse CLI args              (no deps)
2. Init tracing                (no deps)
3. Open storage (redb)         (needs: CLI args for db-path)
4. Load master key             (needs: CLI args for key-source)
5. Init secret provider        (needs: storage, master key)
6. Init metrics collector      (needs: storage)
7. Init Wasm runtime           (no deps — engine creation)
8. Init port allocator         (needs: CLI args for port range)
9. Init upstream registry      (no deps — empty table)
10. Init service registry      (no deps — empty table)
11. Init host router            (no deps — empty table, loaded in step 15)
12. Connect to NATS             (needs: CLI args for nats-url)
13. Setup JetStream             (needs: NATS connection)
14. Build Supervisor            (needs: everything above)
15. Restore state from redb    (needs: supervisor, storage)
16. Start health loop           (needs: supervisor)
17. Subscribe to NATS events    (needs: NATS, supervisor, all registries)
18. Start load reporter         (needs: supervisor, NATS)
19. Build Pingora proxy         (needs: router, upstream registry, supervisor callback)
20. Start admin API             (needs: metrics, upstream registry)
21. Run Pingora (blocks)        (needs: everything)
```

Steps 19–21 must be last because `proxy_server.run()` blocks forever. Steps 1–18 must
complete before serving any traffic.

### The Cold-Start Callback: Avoiding Circular Dependencies

Pingora needs to call `supervisor.ensure_instance()` to trigger cold starts. But if `proxy`
imports `supervisor`, and `supervisor` imports `proxy` (for the UpstreamRegistry), you have
a circular crate dependency.

The solution: `proxy` defines the cold-start type as a function pointer trait object:
```rust
Arc<dyn Fn(AppId) -> BoxFuture<'static, Option<SocketAddr>> + Send + Sync>
```

`main.rs` creates the closure:
```rust
let cold_start = Arc::new(move |app_id| Box::pin(supervisor.ensure_instance(app_id)));
```

`main.rs` knows about both `proxy` and `supervisor`. It is the only place with that
full picture. The crates themselves stay independent.

### State Restoration: Why Not Block on it?

`supervisor.restore_from_storage()` reads all apps from redb and deserializes their
artifacts. For a node with 100 deployed apps this takes ~100ms total (1ms per app
for deserialization). During this 100ms, the proxy is not yet running.

This is acceptable for a node restart scenario. The alternative — streaming restoration
while already serving traffic — risks serving requests for apps whose artifacts haven't
been deserialized yet (producing confusing errors).

The node is not registered as healthy in the load balancer until startup completes, so
the 100ms window of unavailability is invisible to users.

---

---

## 1. CLI Arguments

```rust
// crates/node/src/main.rs
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
struct Args {
    /// Path to the redb database file
    #[arg(long, default_value = "/var/lib/wasm-node/state.redb")]
    db_path: String,

    /// NATS server URL
    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// NATS credentials file path (optional, for authenticated clusters)
    #[arg(long)]
    nats_creds: Option<String>,

    /// HTTP proxy port (external traffic)
    #[arg(long, default_value = "8080")]
    proxy_port: u16,

    /// HTTPS proxy port (set to 0 to disable)
    #[arg(long, default_value = "8443")]
    proxy_https_port: u16,

    /// TLS certificate PEM path (required if proxy_https_port > 0)
    #[arg(long)]
    tls_cert: Option<String>,

    /// TLS key PEM path
    #[arg(long)]
    tls_key: Option<String>,

    /// Admin API port (metrics, status)
    #[arg(long, default_value = "9090")]
    admin_port: u16,

    /// OTLP endpoint for distributed tracing (optional)
    #[arg(long)]
    otlp_endpoint: Option<String>,

    /// Unique node identifier
    #[arg(long, env = "NODE_ID", default_value = "node-0")]
    node_id: String,

    /// Port allocation range start
    #[arg(long, default_value = "10000")]
    port_start: u16,

    /// Port allocation range end
    #[arg(long, default_value = "19999")]
    port_end: u16,

    /// Source of node master key: "env", "file", or "generate"
    #[arg(long, default_value = "env")]
    key_source: String,

    /// Path to master key file (used with --key-source=file or generate)
    #[arg(long)]
    key_file: Option<String>,
}
```

---

## 2. Full main.rs

```rust
// crates/node/src/main.rs
use std::sync::Arc;
use std::net::IpAddr;
use tokio::sync::mpsc;
use tracing::info;

mod args;
use args::Args;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 1. Parse arguments ────────────────────────────────────────────────────
    let args = Args::parse();

    // ── 2. Initialize tracing ─────────────────────────────────────────────────
    if let Some(ref endpoint) = args.otlp_endpoint {
        metrics::tracing_setup::init_tracing("wasm-node", endpoint);
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .json()
            .init();
    }
    info!(node_id = %args.node_id, "wasm-node starting");

    // ── 3. Open storage ──────────────────────────────────────────────────────
    let store = storage::Store::open(std::path::Path::new(&args.db_path))?;
    info!(path = %args.db_path, "storage opened");

    // ── 4. Load master key ────────────────────────────────────────────────────
    let master_key = load_master_key(&args);

    // ── 5. Initialize secret provider ────────────────────────────────────────
    let secret_provider = Arc::new(
        secrets::LocalSecretProvider::new(store.clone(), master_key)
    );

    // ── 6. Initialize Prometheus metrics ─────────────────────────────────────
    let metrics = Arc::new(metrics::exporter::Metrics::new());
    let metrics_collector = metrics::collector::MetricsCollector::start(store.clone());

    // ── 7. Initialize Wasm runtime ────────────────────────────────────────────
    let runtime = runtime::WasmRuntime::new();
    info!("Wasm runtime initialized (Cranelift AOT)");

    // ── 8. Initialize port allocator ─────────────────────────────────────────
    let bind_addr: IpAddr = "0.0.0.0".parse().unwrap();
    let port_alloc = Arc::new(
        supervisor::port_alloc::PortAllocator::new(bind_addr, args.port_start, args.port_end)
    );

    // ── 9. Initialize upstream registry (shared between proxy and supervisor) ─
    let upstream_registry = Arc::new(proxy::upstream::UpstreamRegistry::default());

    // ── 10. Initialize local service registry ─────────────────────────────────
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());

    // ── 11. Initialize host router (Host header → AppId) ──────────────────────
    let host_router = Arc::new(proxy::router::HostRouter::default());

    // ── 12. Connect to NATS ───────────────────────────────────────────────────
    let bus = match &args.nats_creds {
        Some(creds) => messaging::NatsBus::connect_secure(&args.nats_url, creds).await?,
        None => messaging::NatsBus::connect(&args.nats_url).await?,
    };
    info!(url = %args.nats_url, "NATS connected");

    // Set up JetStream durable streams
    bus.setup_jetstream().await?;

    // ── 13. NATS event channel (Supervisor → NATS) ────────────────────────────
    let (event_tx, event_rx) = mpsc::channel::<messaging::events::Event>(1000);
    let bus_clone = bus.clone();
    tokio::spawn(messaging::publisher::run_publisher(bus_clone, event_rx));

    // ── 14. Build Supervisor ──────────────────────────────────────────────────
    let supervisor = Arc::new(supervisor::Supervisor::new(
        store.clone(),
        runtime.clone(),
        port_alloc.clone(),
        upstream_registry.clone(),
        service_registry.clone(),
        secret_provider.clone(),
        metrics_collector.sender(),
        event_tx.clone(),
    ));

    // Restore state from redb (re-prepare all previously deployed apps)
    supervisor.restore_from_storage().await?;
    info!("supervisor state restored from storage");

    // Start background health loop
    supervisor.clone().start_health_loop();

    // ── 15. Subscribe to NATS events ─────────────────────────────────────────
    let dispatcher = Arc::new(messaging::handlers::EventDispatcher::new(
        supervisor.clone(),
        upstream_registry.clone(),
        host_router.clone(),
        store.clone(),
        runtime.clone(),
        args.node_id.clone(),
    ));

    // Subscribe to deployment events (durable — replays on restart)
    {
        let d = dispatcher.clone();
        bus.subscribe_durable("DEPLOY", &args.node_id, "deploy.>", move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        }).await?;
    }

    // Subscribe to live events (ephemeral — no replay needed)
    for subject in &["instance.ready.>", "instance.dead.>", "secrets.update.>",
                      "config.update.>", "node.load.>"] {
        let d = dispatcher.clone();
        let subject = subject.to_string();
        bus.subscribe(&subject, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        }).await?;
    }

    // ── 16. Start load reporter ───────────────────────────────────────────────
    supervisor::scaling::start_load_reporter(
        supervisor.clone(),
        bus.clone(),
        args.node_id.clone(),
        5_000_000_000, // node fuel budget per second
    );

    // ── 17. Build Pingora proxy ───────────────────────────────────────────────
    let cold_start_supervisor = supervisor.clone();
    let cold_start = Arc::new(move |app_id: common::types::AppId| {
        let sup = cold_start_supervisor.clone();
        Box::pin(async move {
            sup.ensure_instance(&app_id).await.ok()
        }) as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    let wasm_proxy = proxy::service::WasmProxy {
        router: host_router.clone(),
        upstream: upstream_registry.clone(),
        cold_start,
        node_table: Default::default(),
    };

    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(proxy::tls::tls_settings(
            std::path::Path::new(cert),
            std::path::Path::new(key),
        )),
        _ => None,
    };

    let proxy_server = proxy::ProxyServer::build(
        wasm_proxy,
        args.proxy_port,
        Some(args.proxy_https_port).filter(|&p| p > 0),
        tls,
    );

    // ── 18. Start Admin API (metrics, health) ─────────────────────────────────
    let admin_app = axum::Router::new()
        .merge(metrics::exporter::metrics_router(metrics.clone()))
        .merge(proxy::admin::admin_router(upstream_registry.clone()))
        .route("/health", axum::routing::get(|| async { "OK" }));

    let admin_addr = format!("0.0.0.0:{}", args.admin_port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&admin_addr).await
            .expect("admin API bind failed");
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app).await.unwrap();
    });

    // ── 19. Run Pingora (blocks forever) ─────────────────────────────────────
    info!(
        http = args.proxy_port,
        https = args.proxy_https_port,
        admin = args.admin_port,
        "node fully started"
    );
    proxy_server.run(); // never returns
}

fn load_master_key(args: &Args) -> secrets::crypto::SymmetricKey {
    match args.key_source.as_str() {
        "env" => {
            let hex = std::env::var("NODE_MASTER_KEY")
                .expect("NODE_MASTER_KEY env var required with --key-source=env");
            let bytes = hex::decode(&hex).expect("NODE_MASTER_KEY must be hex-encoded");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes[..32]);
            secrets::crypto::SymmetricKey::from_bytes(arr)
        }
        "file" => {
            let path = args.key_file.as_deref().expect("--key-file required with --key-source=file");
            let content = std::fs::read(path).expect("cannot read key file");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&content[..32]);
            secrets::crypto::SymmetricKey::from_bytes(arr)
        }
        "generate" => {
            let key = secrets::crypto::SymmetricKey::generate();
            let default_path = "/etc/wasm-node/master.key";
            let path = args.key_file.as_deref().unwrap_or(default_path);
            std::fs::write(path, key.as_bytes()).expect("cannot write key file");
            tracing::warn!("Generated new master key at {path}. Back this file up securely!");
            key
        }
        s => panic!("Unknown key source: {s}"),
    }
}
```

---

## 3. systemd Service Unit

```ini
# /etc/systemd/system/wasm-node.service
[Unit]
Description=Wasm Cloud Platform Node
After=network.target

[Service]
Type=simple
User=wasm-node
Group=wasm-node
ExecStart=/usr/local/bin/wasm-node \
  --db-path /var/lib/wasm-node/state.redb \
  --nats-url nats://nats.internal:4222 \
  --nats-creds /etc/wasm-node/node.creds \
  --proxy-port 80 \
  --proxy-https-port 443 \
  --tls-cert /etc/wasm-node/tls/cert.pem \
  --tls-key /etc/wasm-node/tls/key.pem \
  --admin-port 9090 \
  --node-id node-1 \
  --key-source file \
  --key-file /etc/wasm-node/master.key

Environment=RUST_LOG=info,wasm_node=debug
EnvironmentFile=-/etc/wasm-node/env

# Security hardening
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/wasm-node /etc/wasm-node/master.key

Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

---

## 4. First-Run Setup Script

```bash
#!/bin/bash
# scripts/setup-node.sh

set -e

NODE_ID=${1:-node-0}
NATS_URL=${2:-nats://127.0.0.1:4222}

echo "Setting up wasm-node: $NODE_ID"

# Create directories
mkdir -p /var/lib/wasm-node
mkdir -p /etc/wasm-node/tls

# Create service user
id -u wasm-node &>/dev/null || useradd -r -s /bin/false wasm-node

# Set permissions
chown -R wasm-node:wasm-node /var/lib/wasm-node
chmod 700 /var/lib/wasm-node

# Generate master key on first run
if [ ! -f /etc/wasm-node/master.key ]; then
    /usr/local/bin/wasm-node --key-source generate --key-file /etc/wasm-node/master.key --help > /dev/null 2>&1 || true
    # Actually generate via openssl for simplicity:
    openssl rand -out /etc/wasm-node/master.key 32
    chmod 600 /etc/wasm-node/master.key
    chown wasm-node:wasm-node /etc/wasm-node/master.key
    echo "Master key generated at /etc/wasm-node/master.key — BACK THIS UP!"
fi

# Write env file
cat > /etc/wasm-node/env << EOF
NODE_ID=$NODE_ID
RUST_LOG=info
EOF

# Install systemd unit
systemctl daemon-reload
systemctl enable wasm-node
systemctl start wasm-node

echo "wasm-node $NODE_ID is running!"
echo "Metrics: http://localhost:9090/metrics"
echo "Admin:   http://localhost:9090/upstreams"
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Binary & Startup
- [ ] `cargo build --release -p node` produces a single `wasm-node` binary
- [ ] `wasm-node --help` prints all CLI arguments with descriptions
- [ ] The binary starts and reaches "node fully started" log line within 3 seconds
- [ ] All 14+ startup steps complete in order without error on a fresh database

### Component Wiring
- [ ] `Store`, `WasmRuntime`, `PortAllocator`, `UpstreamRegistry`, `HostRouter`, `NatsBus`, `Supervisor`, `LogDispatcher`, and `MetricsCollector` are all initialized and wired together in `main.rs`
- [ ] The `cold_start` callback passed to the proxy correctly calls `supervisor.ensure_instance()`
- [ ] The `EventDispatcher` is constructed with references to all components it needs (supervisor, upstream, host_router, store, runtime, secret_provider)

### Listeners
- [ ] HTTP proxy is reachable on `--proxy-port` (default 8080)
- [ ] HTTPS proxy is reachable on `--proxy-https-port` when cert and key are provided
- [ ] Admin API is reachable on `--admin-port` (default 9090)
- [ ] Artifact server is reachable on `--artifact-port` (default 9091)

### State Restore
- [ ] On restart with an existing database, all previously deployed apps are restored
- [ ] Routes loaded from redb are immediately active in the proxy (no traffic gap)
- [ ] Secrets are available from redb immediately after startup (no remote fetch needed)

### Signal Handling
- [ ] `Ctrl-C` or `SIGTERM` triggers the graceful drain of all instances before process exit
- [ ] The node exits with code 0 after a clean shutdown

### systemd
- [ ] The systemd unit file installs and enables without errors
- [ ] `systemctl start wasm-node` starts the process as the `wasm-node` user
- [ ] `systemctl stop wasm-node` triggers graceful shutdown (not SIGKILL)
- [ ] The process restarts automatically on crash (`Restart=on-failure`)

### End-to-End Smoke Test
- [ ] Start the node → deploy an app → add a route → send an HTTP request → receive a 200 response, all without manual intervention beyond the CLI commands
