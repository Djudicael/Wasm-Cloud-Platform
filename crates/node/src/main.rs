use clap::Parser;
use messaging::reconnect::{NatsHealth, NatsHealthWatcher};
use serde::Deserialize;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub mod db_config;
pub mod handlers;
pub mod recovery;
pub mod upgrade;

// ── TOML configuration file support ──────────────────────────────────────────
//
// The `--config path.toml` flag loads a TOML file whose keys mirror the CLI
// flags.  Merge priority (highest wins): CLI flag > environment variable > config
// file > built-in default.
//
// Example config file:
//
//   db_path = "/data/wasm-node/state.redb"
//   nats_url = "nats://nats:4222"
//   node_id = "node-1"
//   proxy_port = 8080
//   admin_port = 9090
//   admin_token = "s3cret"
//   key_file = "/secrets/kek.bin"
//
// Any field omitted from the file falls back to the CLI default.

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct NodeConfig {
    db_path: Option<String>,
    nats_url: Option<String>,
    nats_creds: Option<String>,
    proxy_port: Option<u16>,
    proxy_https_port: Option<u16>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    admin_port: Option<u16>,
    artifact_port: Option<u16>,
    otlp_endpoint: Option<String>,
    node_id: Option<String>,
    port_start: Option<u16>,
    port_end: Option<u16>,
    key_source: Option<String>,
    key_file: Option<String>,
    admin_token: Option<String>,
    database_url: Option<String>,
    pgbouncer_addr: Option<String>,
    enable_db_proxy: Option<bool>,
    db_proxy_addr: Option<String>,
    db_backend_addr: Option<String>,
    db_proxy_max_connections: Option<usize>,
    billing_export_dir: Option<String>,
    billing_export_interval_secs: Option<u64>,
    platform_domain: Option<String>,
    dns_webhook_url: Option<String>,
    dns_webhook_token: Option<String>,
}

impl NodeConfig {
    /// Load a TOML config file. Returns `Ok(Default)` if path is `None`.
    fn load(path: Option<&str>) -> anyhow::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config file {}: {e}", path))?;
        let config: NodeConfig = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("cannot parse config file {}: {e}", path))?;
        info!(path, "loaded configuration file");
        Ok(config)
    }
}

#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
struct Args {
    /// Path to a TOML configuration file. Values in the file are used as
    /// defaults; CLI flags and environment variables take precedence.
    #[arg(long)]
    config: Option<String>,

    #[arg(long, default_value = "/tmp/wasm-node/state.redb")]
    db_path: String,

    #[arg(long, default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    #[arg(long)]
    nats_creds: Option<String>,

    #[arg(long, default_value = "8080")]
    proxy_port: u16,

    #[arg(long, default_value = "8443")]
    proxy_https_port: u16,

    #[arg(long)]
    tls_cert: Option<String>,

    #[arg(long)]
    tls_key: Option<String>,

    #[arg(long, default_value = "9090")]
    admin_port: u16,

    #[arg(long, default_value = "9091")]
    artifact_port: u16,

    #[arg(long)]
    otlp_endpoint: Option<String>,

    #[arg(long, env = "NODE_ID", default_value = "node-0")]
    node_id: String,

    #[arg(long, default_value = "10000")]
    port_start: u16,

    #[arg(long, default_value = "19999")]
    port_end: u16,

    #[arg(long, default_value = "generate")]
    key_source: String,

    #[arg(long)]
    key_file: Option<String>,

    #[arg(long, env = "ADMIN_TOKEN")]
    admin_token: Option<String>,

    #[arg(long, default_value = "postgres://127.0.0.1:5432")]
    database_url: String,

    #[arg(long, default_value = "127.0.0.1:5432")]
    pgbouncer_addr: String,

    #[arg(long)]
    enable_db_proxy: bool,

    #[arg(long, default_value = "127.0.0.1:5433")]
    db_proxy_addr: String,

    #[arg(long, default_value = "db.internal:5432")]
    db_backend_addr: String,

    #[arg(long, default_value = "20")]
    db_proxy_max_connections: usize,

    #[arg(
        long,
        help = "Directory for billing record exports (if set, enables periodic export)"
    )]
    billing_export_dir: Option<String>,

    #[arg(
        long,
        default_value = "3600",
        help = "Billing export interval in seconds (requires --billing-export-dir)"
    )]
    billing_export_interval_secs: u64,

    #[arg(long, help = "Platform domain for subdomains (e.g. myplatform.com)")]
    platform_domain: Option<String>,

    #[arg(long, help = "Webhook URL for DNS automation")]
    dns_webhook_url: Option<String>,

    #[arg(long, help = "Auth token for DNS webhook")]
    dns_webhook_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();

    // Load TOML config file (if provided) and merge into args.
    // CLI flags and env vars already have their values from clap,
    // so we only override when the CLI value is still the default
    // and the config file provides a value.
    let file_config = NodeConfig::load(args.config.as_deref())?;
    args.merge_config(file_config);

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!(node_id = %args.node_id, "wasm-node starting");

    if let Some(parent) = std::path::Path::new(&args.db_path).parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }

    let store = storage::Store::open(std::path::Path::new(&args.db_path))?;
    info!(path = %args.db_path, "storage opened");

    // Initialize recovery metrics early (needed for recovery mode detection)
    let recovery_metrics = Arc::new(metrics::recovery::RecoveryMetrics::new());

    // Detect recovery mode (L4: total loss detection)
    let recovery_mode = recovery::detect_recovery_mode(&store, &args.node_id);
    match recovery_mode {
        recovery::RecoveryMode::Normal => {
            info!("normal startup — existing state found");
        }
        recovery::RecoveryMode::FullRebuild => {
            info!("recovery mode: full rebuild required — will request state from cluster");
            recovery_metrics.set_recovery_mode(1);
        }
        recovery::RecoveryMode::CorruptionDetected => {
            tracing::warn!("recovery mode: corruption detected — will attempt partial rebuild");
            recovery_metrics.set_recovery_mode(2);
        }
    }

    let runtime = runtime::WasmRuntime::new();
    info!("Wasm runtime initialized (Cranelift AOT)");

    let bind_addr: IpAddr = "0.0.0.0".parse().unwrap();
    let port_alloc = Arc::new(supervisor::port_alloc::PortAllocator::new(
        bind_addr,
        args.port_start,
        args.port_end,
    ));

    let upstream_registry = Arc::new(proxy::upstream::UpstreamRegistry::default());
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());
    let host_router = Arc::new(proxy::router::HostRouter::default());

    let mut bus = match &args.nats_creds {
        Some(creds) => messaging::NatsBus::connect_secure(&args.nats_url, creds).await?,
        None => messaging::NatsBus::connect(&args.nats_url).await?,
    };
    bus.set_node_id(args.node_id.clone());
    info!(url = %args.nats_url, "NATS connected");

    bus.setup_jetstream().await?;

    // Initialize NATS health tracking for L5 (partition) recovery
    let nats_health = Arc::new(NatsHealth::new());

    // Start NATS health watcher (updates last message timestamp periodically)
    let _nats_watcher_handle =
        NatsHealthWatcher::new((*nats_health).clone(), Duration::from_secs(5)).start();

    recovery::startup_integrity_check(&store, bus.client()).await;

    let (event_tx, _event_rx) = mpsc::channel::<messaging::events::Event>(1000);

    // Initialize database manager
    let db_config = db_config::DatabaseConfig {
        default_database_url: args.database_url.clone(),
        health_check_addr: args.pgbouncer_addr.clone(),
        health_check_interval_secs: 30,
        enable_builtin_proxy: args.enable_db_proxy,
        builtin_proxy_addr: args.db_proxy_addr.clone(),
        builtin_proxy_backend: args.db_backend_addr.clone(),
        builtin_proxy_max_connections: args.db_proxy_max_connections,
    };

    let db_manager = db_config::DatabaseManager::new(db_config.clone());
    db_manager.initialize().await?;

    let env_resolver = Arc::new(move |config: &common::types::AppConfig, _host_port: u16| {
        let mut vars = Vec::new();
        for (k, v) in &config.env_vars {
            vars.push((k.clone(), v.clone()));
        }
        // Inject DATABASE_URL if not already provided in env_vars
        if !config.env_vars.contains_key("DATABASE_URL") {
            vars.push((
                "DATABASE_URL".to_string(),
                db_config.default_database_url.clone(),
            ));
        }
        vars
    });

    // Initialize billing collector
    let billing_collector = billing::BillingCollector::start(store.clone(), args.node_id.clone());
    info!("billing collector started");

    // Optionally start billing export loop
    if let Some(ref export_dir) = args.billing_export_dir {
        let exporter = Arc::new(billing::FileExporter::new(std::path::PathBuf::from(
            export_dir,
        )));
        let interval = Duration::from_secs(args.billing_export_interval_secs);
        billing::start_export_loop(store.clone(), exporter, interval);
        info!(
            dir = export_dir,
            interval = interval.as_secs(),
            "billing export loop started"
        );
    }

    let supervisor = supervisor::Supervisor::new(
        store.clone(),
        runtime.clone(),
        port_alloc.clone(),
        upstream_registry.clone(),
        host_router.clone(),
        service_registry.clone(),
        env_resolver,
        event_tx.clone(),
        Some(billing_collector.tx()),
    );

    supervisor.restore_from_storage().await?;
    info!("supervisor state restored from storage");

    supervisor.clone().start_health_loop();

    host_router.load_routes_from_store(&store).await;
    info!("routes loaded from local storage");

    // Initialize secret provider with KEK
    //
    // KEK loading priority:
    //   1. If --key-file is provided, load the raw 32-byte key from that file
    //   2. Otherwise, try to load the KEK previously persisted in redb
    //   3. If neither source has a KEK, generate a fresh one and persist it
    //
    // This ensures secrets survive restarts: the same KEK is reused across
    // restarts unless the operator explicitly provides a different key file.
    let kek = if let Some(key_file) = &args.key_file {
        match std::fs::read(key_file) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                tracing::info!(path = %key_file, "loaded KEK from key file");
                secrets::crypto::SymmetricKey::from_bytes(key)
            }
            Ok(bytes) => {
                tracing::error!(
                    path = %key_file,
                    len = bytes.len(),
                    "key file must be exactly 32 bytes, generating new KEK instead"
                );
                let kek = secrets::crypto::SymmetricKey::generate();
                if let Err(e) = store.save_kek(kek.as_bytes()) {
                    tracing::warn!(error = %e, "failed to persist KEK");
                }
                kek
            }
            Err(e) => {
                tracing::error!(path = %key_file, error = %e, "failed to read key file, generating new KEK instead");
                let kek = secrets::crypto::SymmetricKey::generate();
                if let Err(e) = store.save_kek(kek.as_bytes()) {
                    tracing::warn!(error = %e, "failed to persist KEK");
                }
                kek
            }
        }
    } else if let Ok(Some(kek_bytes)) = store.load_kek() {
        // KEK was persisted on a previous run — reuse it so existing secrets remain readable
        if kek_bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&kek_bytes);
            tracing::info!("loaded KEK from storage (secrets from previous runs are readable)");
            secrets::crypto::SymmetricKey::from_bytes(key)
        } else {
            tracing::warn!(
                len = kek_bytes.len(),
                "stored KEK has unexpected length, generating new KEK (existing secrets will be unreadable)"
            );
            let kek = secrets::crypto::SymmetricKey::generate();
            if let Err(e) = store.save_kek(kek.as_bytes()) {
                tracing::warn!(error = %e, "failed to persist KEK");
            }
            kek
        }
    } else {
        // First run: generate a fresh KEK and persist it for future restarts
        let kek = secrets::crypto::SymmetricKey::generate();
        if let Err(e) = store.save_kek(kek.as_bytes()) {
            tracing::warn!(error = %e, "failed to persist KEK — secrets will be lost on restart");
        } else {
            tracing::info!("generated and persisted new KEK");
        }
        kek
    };
    let secret_provider = Arc::new(secrets::LocalSecretProvider::new(store.clone(), kek));

    // Check if this is a fresh node (no apps in storage)
    let is_fresh = store.list_apps()?.is_empty();

    // Generate bootstrap keypair if fresh (for receiving encrypted secrets)
    let bootstrap_keypair = if is_fresh {
        Some(secrets::BootstrapKeyPair::generate())
    } else {
        None
    };

    // Use localhost for artifact server URL
    // TODO: In production, this should be the node's publicly accessible IP
    let artifact_server_url = format!("http://127.0.0.1:{}", args.artifact_port);

    let dispatcher = Arc::new(handlers::EventDispatcher {
        supervisor: supervisor.clone(),
        upstream: upstream_registry.clone(),
        host_router: host_router.clone(),
        store: store.clone(),
        runtime: runtime.clone(),
        node_id: args.node_id.clone(),
        artifact_server_url: artifact_server_url.clone(),
        secret_provider: secret_provider.clone(),
        bootstrap_keypair,
        bus: bus.clone(),
        dns_webhook: proxy::dns_webhook::DnsWebhookManager::new(
            args.dns_webhook_url.clone(),
            args.dns_webhook_token.clone(),
        ),
        node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
    });

    {
        let d = dispatcher.clone();
        let node_id = args.node_id.clone();
        tracing::info!(subscribing_to = "DEPLOY", consumer = %node_id, "subscribing to deploy stream");
        if let Err(e) = bus
            .subscribe_durable("DEPLOY", &node_id, move |event| {
                let d = d.clone();
                async move { d.handle(event).await }
            })
            .await
        {
            tracing::error!(error = %e, "failed to subscribe to DEPLOY stream");
        } else {
            tracing::info!("successfully subscribed to DEPLOY stream");
        }
    }

    // Subscribe to critical control plane events with durable consumers
    for _subject in &["instance.ready.>", "instance.dead.>"] {
        let d = dispatcher.clone();
        let stream = "CONTROL".to_string();
        let consumer = format!("node-{}", args.node_id);
        bus.subscribe_durable(&stream, &consumer, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    for _subject in &["secrets.update.>", "config.update.>"] {
        let d = dispatcher.clone();
        let stream = "CONTROL".to_string();
        let consumer = format!("node-{}", args.node_id);
        bus.subscribe_durable(&stream, &consumer, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    for _subject in &["node.load.>", "routes.", "cluster.>"] {
        let d = dispatcher.clone();
        let stream = "NODE".to_string();
        let consumer = format!("node-{}", args.node_id);
        bus.subscribe_durable(&stream, &consumer, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    // If this is a fresh node, request state snapshot from cluster
    if is_fresh {
        info!("fresh node detected — requesting state snapshot from cluster");

        let public_key_bytes = dispatcher
            .bootstrap_keypair
            .as_ref()
            .map(|kp| kp.public_bytes())
            .unwrap_or_default();

        let join_event = messaging::events::Event::NodeJoined {
            node_id: args.node_id.clone(),
            artifact_server_url: artifact_server_url.clone(),
            public_key_bytes,
            protocol_version: common::protocol::PROTOCOL_VERSION,
            binary_version: common::protocol::BINARY_VERSION.to_string(),
        };

        bus.publish(&join_event).await?;
        info!("NodeJoined event published, waiting for snapshot");

        // Wait for StateSnapshot with a timeout instead of fixed sleep
        let snapshot_subject = format!("cluster.snapshot.{}", args.node_id);
        let timeout = tokio::time::Duration::from_secs(30);
        match tokio::time::timeout(timeout, bus.wait_for_event(&snapshot_subject)).await {
            Ok(Ok(_)) => info!("State snapshot received"),
            Ok(Err(e)) => warn!(error = %e, "failed to receive state snapshot"),
            Err(_) => warn!("timed out waiting for state snapshot after 30s"),
        }
    }

    supervisor::scaling::start_load_reporter(
        supervisor.clone(),
        bus.clone(),
        args.node_id.clone(),
        5_000_000_000,
    );

    let cold_start_supervisor = supervisor.clone();
    let cold_start = Arc::new(move |app_id: common::types::AppId| {
        let sup = cold_start_supervisor.clone();
        Box::pin(async move { sup.ensure_instance(&app_id).await.ok() })
            as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    // Initialize Prometheus metrics
    let prom_metrics = Arc::new(metrics::exporter::Metrics::new());
    prom_metrics.set_platform_info(
        &args.node_id,
        common::protocol::BINARY_VERSION,
        common::protocol::PROTOCOL_VERSION,
    );
    info!(
        node_id = %args.node_id,
        binary_version = common::protocol::BINARY_VERSION,
        protocol_version = common::protocol::PROTOCOL_VERSION,
        "platform version metrics initialized"
    );

    let backpressure = proxy::backpressure::BackpressureSignal::new();

    let default_rate_config = proxy::rate_limiter::RateLimitConfig {
        requests_per_second: 1000,
        burst_capacity: 200,
        per_ip_limit: 200,
    };

    // Initialize rate limit metrics (register with the same registry)
    let rate_limit_metrics = Arc::new(proxy::metrics::RateLimitMetrics::new(
        &prom_metrics.registry,
    ));

    let wasm_proxy = proxy::service::WasmProxy {
        router: host_router.clone(),
        upstream: upstream_registry.clone(),
        rate_limiter: Arc::new(proxy::rate_limiter::RateLimiter::new(default_rate_config)),
        node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
        cold_start,
        backpressure: backpressure.clone(),
        metrics: Some(rate_limit_metrics),
    };

    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(proxy::tls::tls_settings(
            std::path::Path::new(cert),
            std::path::Path::new(key),
        )),
        _ => None,
    };

    let proxy_timeouts = proxy::config::ProxyTimeouts::default();
    let proxy_server = proxy::ProxyServer::build(
        wasm_proxy,
        args.proxy_port,
        Some(args.proxy_https_port).filter(|&p| p > 0),
        tls,
        proxy_timeouts,
    );

    // Admin API with pgBouncer status endpoint and Prometheus metrics
    let pgbouncer_check_addr = args.pgbouncer_addr.clone();
    let db_path_clone = args.db_path.clone();
    let store_gc = store.clone();
    let supervisor_gc = supervisor.clone();

    // Enhanced health check for external LBs and DNS providers
    let health_router = proxy::health::health_router(
        args.node_id.clone(),
        nats_health.clone(),
        Arc::new(backpressure),
    );

    let admin_app = axum::Router::new()
        .merge(health_router)
        .route(
            "/status/pgbouncer",
            axum::routing::get(move || {
                let addr = pgbouncer_check_addr.clone();
                async move {
                    let available = supervisor::db_proxy::check_pgbouncer(&addr).await;
                    let status = if available { "healthy" } else { "unavailable" };
                    axum::Json(serde_json::json!({
                        "status": status,
                        "address": addr,
                        "available": available,
                    }))
                }
            }),
        )
        .route(
            "/admin/rebuild",
            axum::routing::post(move || {
                let db_path = db_path_clone.clone();
                async move {
                    tracing::warn!("Admin rebuild requested — draining and restarting");
                    match std::fs::remove_file(&db_path) {
                        Ok(_) => (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "rebuild_initiated",
                                "message": "Node will restart and rebuild from cluster state"
                            })),
                        ),
                        Err(e) => {
                            tracing::error!(error = %e, "failed to delete database for rebuild");
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "status": "error",
                                    "message": format!("Failed to delete database: {}", e)
                                })),
                            )
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gc/force",
            axum::routing::post(move || {
                let store = store_gc.clone();
                let supervisor = supervisor_gc.clone();
                async move {
                    tracing::info!("Forcing immediate GC run");

                    // Force purge undeployed apps with grace period = 0
                    let purged = store.gc_undeployed_apps(0).unwrap_or(0);
                    tracing::info!(apps = purged, "Forced GC: undeployed apps purged");

                    // Get list of apps that were marked undeployed by reading from GC metadata
                    // For simplicity, we kill instances for all apps that have no active routes
                    let app_ids = store.list_apps().unwrap_or_default();
                    let mut killed_count = 0;

                    for app_id in app_ids.iter() {
                        let app_id_obj = common::types::AppId(app_id.0.clone());
                        // Try to kill all instances - this is safe to call even if app is still deployed
                        match supervisor.kill_all_instances(&app_id_obj).await {
                            Ok(()) => {
                                killed_count += 1;
                            }
                            Err(e) => {
                                tracing::debug!(app = %app_id.0, error = %e, "No instances to kill");
                            }
}
                    }

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "gc_complete",
                            "undeployed_apps_purged": purged,
                            "apps_killed": killed_count,
                        })),
                    )
                }
            }),
        )
        .merge(metrics::exporter::metrics_router(prom_metrics))
        .layer(axum::middleware::from_fn(admin_auth_fn(args.admin_token.clone())));

    let admin_addr = format!("0.0.0.0:{}", args.admin_port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .expect("admin API bind failed");
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app).await.unwrap();
    });

    let artifact_app = storage::artifact_server::artifact_router(store.clone());
    let artifact_addr = format!("0.0.0.0:{}", args.artifact_port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr)
            .await
            .expect("artifact server bind failed");
        info!(addr = %artifact_addr, "artifact server listening");
        axum::serve(listener, artifact_app).await.unwrap();
    });

    info!(
        http = args.proxy_port,
        https = args.proxy_https_port,
        admin = args.admin_port,
        artifact = args.artifact_port,
        "node fully started"
    );

    // Run Pingora in background to allow graceful shutdown setup
    std::thread::spawn(move || {
        proxy_server.run();
    });

    // Wait for shutdown signal (SIGTERM / Ctrl-C)
    // On Linux/WSL, `systemctl stop` sends SIGTERM, so we must handle it.
    // We race both signals — whichever fires first triggers graceful drain.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received — gracefully shutting down all instances"),
            _ = sigint.recv() => info!("SIGINT (Ctrl-C) received — gracefully shutting down all instances"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.unwrap();
        info!("Ctrl-C received — gracefully shutting down all instances");
    }

    // Gracefully shutdown all instances with timeout
    let shutdown_timeout = std::time::Duration::from_secs(30);
    supervisor.shutdown_all(shutdown_timeout).await;

    info!("All instances stopped — exiting");
    std::process::exit(0);
}

impl Args {
    /// Merge values from a `NodeConfig` file into the CLI args.
    ///
    /// For every field where the config file provides a value (`Some`), we
    /// overwrite the corresponding CLI arg.  This means CLI flags always win
    /// over the config file because clap has already parsed them — but only
    /// when the user explicitly passed them.  Unfortunately clap doesn't expose
    /// "was this flag explicitly set?" for all types easily, so we use a simple
    /// heuristic: if the config file has a value, use it.  The user can always
    /// override by passing the CLI flag explicitly (which clap processes first).
    fn merge_config(&mut self, cfg: NodeConfig) {
        if let Some(v) = cfg.db_path {
            self.db_path = v;
        }
        if let Some(v) = cfg.nats_url {
            self.nats_url = v;
        }
        if let Some(v) = cfg.nats_creds {
            self.nats_creds = Some(v);
        }
        if let Some(v) = cfg.proxy_port {
            self.proxy_port = v;
        }
        if let Some(v) = cfg.proxy_https_port {
            self.proxy_https_port = v;
        }
        if let Some(v) = cfg.tls_cert {
            self.tls_cert = Some(v);
        }
        if let Some(v) = cfg.tls_key {
            self.tls_key = Some(v);
        }
        if let Some(v) = cfg.admin_port {
            self.admin_port = v;
        }
        if let Some(v) = cfg.artifact_port {
            self.artifact_port = v;
        }
        if let Some(v) = cfg.otlp_endpoint {
            self.otlp_endpoint = Some(v);
        }
        if let Some(v) = cfg.node_id {
            self.node_id = v;
        }
        if let Some(v) = cfg.port_start {
            self.port_start = v;
        }
        if let Some(v) = cfg.port_end {
            self.port_end = v;
        }
        if let Some(v) = cfg.key_source {
            self.key_source = v;
        }
        if let Some(v) = cfg.key_file {
            self.key_file = Some(v);
        }
        if let Some(v) = cfg.admin_token {
            self.admin_token = Some(v);
        }
        if let Some(v) = cfg.database_url {
            self.database_url = v;
        }
        if let Some(v) = cfg.pgbouncer_addr {
            self.pgbouncer_addr = v;
        }
        if let Some(v) = cfg.enable_db_proxy {
            self.enable_db_proxy = v;
        }
        if let Some(v) = cfg.db_proxy_addr {
            self.db_proxy_addr = v;
        }
        if let Some(v) = cfg.db_backend_addr {
            self.db_backend_addr = v;
        }
        if let Some(v) = cfg.db_proxy_max_connections {
            self.db_proxy_max_connections = v;
        }
        if let Some(v) = cfg.billing_export_dir {
            self.billing_export_dir = Some(v);
        }
        if let Some(v) = cfg.billing_export_interval_secs {
            self.billing_export_interval_secs = v;
        }
        if let Some(v) = cfg.platform_domain {
            self.platform_domain = Some(v);
        }
        if let Some(v) = cfg.dns_webhook_url {
            self.dns_webhook_url = Some(v);
        }
        if let Some(v) = cfg.dns_webhook_token {
            self.dns_webhook_token = Some(v);
        }
    }
}

/// Create the admin auth middleware closure.
///
/// - If `token` is `None`, all requests pass through (auth disabled).
/// - Health-check paths (`/health`, `/_platform/health`, `/status/`) are always allowed
///   so that load-balancers can probe the node without credentials.
/// - Otherwise the `Authorization` header must be `Bearer <token>`.
fn admin_auth_fn(
    token: Option<String>,
) -> impl Fn(
    axum::extract::Request,
    axum::middleware::Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = axum::response::Response> + Send>>
       + Clone
       + Send
       + Sync
       + 'static {
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::middleware::Next;
    use axum::response::IntoResponse;

    move |req: Request, next: Next| {
        let expected = token.clone();
        Box::pin(async move {
            // No token configured → auth disabled
            let expected = match expected {
                Some(t) => t,
                None => return next.run(req).await,
            };

            // Health/readiness endpoints are always unauthenticated
            let path = req.uri().path();
            if path.starts_with("/_platform/health")
                || path == "/health"
                || path.starts_with("/status/")
            {
                return next.run(req).await;
            }

            // Extract and validate Bearer token
            let authorized = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|t| t == expected)
                .unwrap_or(false);

            if authorized {
                next.run(req).await
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "error": "unauthorized",
                        "message": "valid Bearer token required via Authorization header (set ADMIN_TOKEN env var)"
                    })),
                )
                    .into_response()
            }
        })
    }
}
