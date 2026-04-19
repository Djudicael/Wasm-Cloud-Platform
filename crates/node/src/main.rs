use clap::Parser;
use messaging::reconnect::{NatsHealth, NatsHealthWatcher};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[cfg(feature = "ebpf")]
use ebpf_monitor::{init, MonitorConfig};

pub mod db_config;
pub mod handlers;
pub mod recovery;
pub mod upgrade;

// Configuration is now handled by the `config` crate.
use config::{load_config, CliOverrides, HotConfigHandle};

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
    let args = Args::parse();

    // Convert CLI args to overrides for the config system
    let cli_overrides = CliOverrides {
        node_id: Some(args.node_id.clone()),
        db_path: Some(args.db_path.clone()),
        nats_url: Some(args.nats_url.clone()),
        nats_creds: args.nats_creds.clone(),
        http_port: Some(args.proxy_port),
        https_port: Some(args.proxy_https_port),
        tls_cert: args.tls_cert.clone(),
        tls_key: args.tls_key.clone(),
        admin_port: Some(args.admin_port),
        artifact_port: Some(args.artifact_port),
        port_start: Some(args.port_start),
        port_end: Some(args.port_end),
        key_source: Some(args.key_source.clone()),
        key_file: args.key_file.clone(),
        database_url: Some(args.database_url.clone()),
        pgbouncer_addr: Some(args.pgbouncer_addr.clone()),
        enable_db_proxy: Some(args.enable_db_proxy),
        db_proxy_addr: Some(args.db_proxy_addr.clone()),
        db_proxy_backend: Some(args.db_backend_addr.clone()),
        db_proxy_max_connections: Some(args.db_proxy_max_connections),
        log_level: None, // Not available in Args
        otlp_endpoint: args.otlp_endpoint.clone(),
        billing_export_dir: args.billing_export_dir.clone(),
        billing_export_interval_secs: Some(args.billing_export_interval_secs),
        platform_domain: args.platform_domain.clone(),
        dns_webhook_url: args.dns_webhook_url.clone(),
        dns_webhook_token: args.dns_webhook_token.clone(),
        auth_token: args.admin_token.clone(),
    };

    // Load configuration with merge priority: defaults < TOML < env < CLI
    let config_path = args.config.as_deref().map(std::path::Path::new);
    let config = load_config(config_path, &cli_overrides)?;

    // Set up logging with configured level
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(
            config
                .logging
                .level
                .parse()
                .unwrap_or(tracing::Level::INFO.into()),
        )
        .from_env_lossy();

    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!(node_id = %config.node.node_id, "wasm-node starting");

    if let Some(parent) = config.storage.db_path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }

    let store = storage::Store::open(&config.storage.db_path)?;
    info!(path = %config.storage.db_path.display(), "storage opened");

    // Initialize recovery metrics early (needed for recovery mode detection)
    let recovery_metrics = Arc::new(metrics::recovery::RecoveryMetrics::new());

    // Detect recovery mode (L4: total loss detection)
    let recovery_mode = recovery::detect_recovery_mode(&store, &config.node.node_id);
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
        config.runtime.port_start,
        config.runtime.port_end,
    ));

    let upstream_registry = Arc::new(proxy::upstream::UpstreamRegistry::default());
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());
    let host_router = Arc::new(proxy::router::HostRouter::default());

    let mut bus = match &config.nats.creds_file {
        Some(creds) => messaging::NatsBus::connect_secure(&config.nats.url, creds).await?,
        None => messaging::NatsBus::connect(&config.nats.url).await?,
    };
    bus.set_node_id(config.node.node_id.clone());
    info!(url = %config.nats.url, "NATS connected");

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
        default_database_url: config.database.default_url.clone(),
        health_check_addr: config.database.pgbouncer_addr.clone(),
        health_check_interval_secs: 30,
        enable_builtin_proxy: config.database.enable_db_proxy,
        builtin_proxy_addr: config.database.db_proxy_addr.clone(),
        builtin_proxy_backend: config.database.db_proxy_backend.clone(),
        builtin_proxy_max_connections: config.database.db_proxy_max_connections,
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
    let billing_collector =
        billing::BillingCollector::start(store.clone(), config.node.node_id.clone());
    info!("billing collector started");

    // Optionally start billing export loop
    if let Some(ref export_dir) = config.billing.export_dir {
        let exporter = Arc::new(billing::FileExporter::new(std::path::PathBuf::from(
            export_dir,
        )));
        let interval = Duration::from_secs(config.billing.export_interval_secs);
        billing::start_export_loop(store.clone(), exporter, interval);
        info!(
            dir = export_dir,
            interval = interval.as_secs(),
            "billing export loop started"
        );
    }

    // Initialize eBPF monitor (kernel-level observability)
    #[cfg(feature = "ebpf")]
    {
        let ebpf_config = ebpf_monitor::MonitorConfig {
            enabled: config.ebpf.enabled,
            fd_soft_limit: config.ebpf.fd_soft_limit,
            fd_hard_limit: config.ebpf.fd_hard_limit,
            mem_low_threshold_pages: config.ebpf.mem_low_threshold_pages,
            mem_critical_threshold_pages: config.ebpf.mem_critical_threshold_pages,
            disk_slow_threshold_ns: config.ebpf.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: config.ebpf.tcp_conn_limit_per_pid,
            syscall_rate_limit: config.ebpf.syscall_rate_limit,
            sampling_period_secs: config.ebpf.sampling_period_secs,
            enable_process_tracker: true,
            enable_tcp_monitor: true,
            enable_fd_watcher: true,
            enable_mem_pressure: true,
            enable_disk_monitor: true,
            enable_syscall_counter: true,
        };
        if let Some(()) = ebpf_monitor::init(ebpf_config).await {
            info!("eBPF monitor initialized and active");
        } else {
            info!("eBPF monitor not active, using fallback");
        }
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
    let kek = if let Some(key_file) = &config.runtime.key_file {
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
    let artifact_server_url = format!("http://127.0.0.1:{}", config.admin.artifact_port);

    let dispatcher = Arc::new(handlers::EventDispatcher {
        supervisor: supervisor.clone(),
        upstream: upstream_registry.clone(),
        host_router: host_router.clone(),
        store: store.clone(),
        runtime: runtime.clone(),
        node_id: config.node.node_id.clone(),
        artifact_server_url: artifact_server_url.clone(),
        secret_provider: secret_provider.clone(),
        bootstrap_keypair,
        bus: bus.clone(),
        dns_webhook: proxy::dns_webhook::DnsWebhookManager::new(
            config.dns.webhook_url.clone(),
            config.dns.webhook_token.clone(),
        ),
        node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
    });

    {
        let d = dispatcher.clone();
        let node_id = config.node.node_id.clone();
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
        let consumer = format!("node-{}", config.node.node_id);
        bus.subscribe_durable(&stream, &consumer, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    for _subject in &["secrets.update.>", "config.update.>"] {
        let d = dispatcher.clone();
        let stream = "CONTROL".to_string();
        let consumer = format!("node-{}", config.node.node_id);
        bus.subscribe_durable(&stream, &consumer, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    for _subject in &["node.load.>", "routes.", "cluster.>"] {
        let d = dispatcher.clone();
        let stream = "NODE".to_string();
        let consumer = format!("node-{}", config.node.node_id);
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
            node_id: config.node.node_id.clone(),
            artifact_server_url: artifact_server_url.clone(),
            public_key_bytes,
            protocol_version: common::protocol::PROTOCOL_VERSION,
            binary_version: common::protocol::BINARY_VERSION.to_string(),
        };

        bus.publish(&join_event).await?;
        info!("NodeJoined event published, waiting for snapshot");

        // Wait for StateSnapshot with a timeout instead of fixed sleep
        let snapshot_subject = format!("cluster.snapshot.{}", config.node.node_id);
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
        config.node.node_id.clone(),
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
        &config.node.node_id,
        common::protocol::BINARY_VERSION,
        common::protocol::PROTOCOL_VERSION,
    );
    info!(
        node_id = %config.node.node_id,
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

    let tls = match (&config.proxy.tls_cert, &config.proxy.tls_key) {
        (Some(cert), Some(key)) => Some(proxy::tls::tls_settings(
            std::path::Path::new(cert),
            std::path::Path::new(key),
        )),
        _ => None,
    };

    let proxy_timeouts = proxy::config::ProxyTimeouts::default();
    let proxy_server = proxy::ProxyServer::build(
        wasm_proxy,
        config.proxy.http_port,
        Some(config.proxy.https_port).filter(|&p| p > 0),
        tls,
        proxy_timeouts,
    );

    // Admin API with pgBouncer status endpoint and Prometheus metrics
    let pgbouncer_check_addr = config.database.pgbouncer_addr.clone();
    let db_path_clone = config.storage.db_path.clone();
    let store_gc = store.clone();
    let supervisor_gc = supervisor.clone();

    // Enhanced health check for external LBs and DNS providers
    let health_router = proxy::health::health_router(
        config.node.node_id.clone(),
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
        .layer(axum::middleware::from_fn(admin_auth_fn(config.admin.auth_token.clone())));

    let admin_addr = format!("0.0.0.0:{}", config.admin.port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .expect("admin API bind failed");
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app).await.unwrap();
    });

    let artifact_app = storage::artifact_server::artifact_router(store.clone());
    let artifact_addr = format!("0.0.0.0:{}", config.admin.artifact_port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr)
            .await
            .expect("artifact server bind failed");
        info!(addr = %artifact_addr, "artifact server listening");
        axum::serve(listener, artifact_app).await.unwrap();
    });

    info!(
        http = config.proxy.http_port,
        https = config.proxy.https_port,
        admin = config.admin.port,
        artifact = config.admin.artifact_port,
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
