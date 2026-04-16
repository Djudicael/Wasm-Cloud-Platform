use clap::Parser;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

pub mod db_config;
pub mod handlers;
pub mod upgrade;

#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
struct Args {
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!(node_id = %args.node_id, "wasm-node starting");

    if let Some(parent) = std::path::Path::new(&args.db_path).parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }

    let store = storage::Store::open(std::path::Path::new(&args.db_path))?;
    info!(path = %args.db_path, "storage opened");

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

    let bus = match &args.nats_creds {
        Some(creds) => messaging::NatsBus::connect_secure(&args.nats_url, creds).await?,
        None => messaging::NatsBus::connect(&args.nats_url).await?,
    };
    info!(url = %args.nats_url, "NATS connected");

    bus.setup_jetstream().await?;

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

    let supervisor = supervisor::Supervisor::new(
        store.clone(),
        runtime.clone(),
        port_alloc.clone(),
        upstream_registry.clone(),
        host_router.clone(),
        service_registry.clone(),
        env_resolver,
        event_tx.clone(),
    );

    supervisor.restore_from_storage().await?;
    info!("supervisor state restored from storage");

    supervisor.clone().start_health_loop();

    host_router.load_routes_from_store(&store).await;
    info!("routes loaded from local storage");

    // Initialize secret provider with KEK
    let kek = if let Some(_key_file) = &args.key_file {
        // TODO: Implement loading from file
        // For now, just generate a new key (will be persisted in storage)
        secrets::crypto::SymmetricKey::generate()
    } else {
        secrets::crypto::SymmetricKey::generate()
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

    for subject in &[
        "instance.ready.>",
        "instance.dead.>",
        "secrets.update.>",
        "config.update.>",
        "node.load.>",
        "routes.>",  // Subscribe to route events
        "cluster.>", // Subscribe to cluster bootstrap events
    ] {
        let d = dispatcher.clone();
        bus.subscribe(subject, move |event| {
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

        // Give the cluster a few seconds to respond
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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
        backpressure: proxy::backpressure::BackpressureSignal::new(),
        metrics: Some(rate_limit_metrics),
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

    // Admin API with pgBouncer status endpoint and Prometheus metrics
    let pgbouncer_check_addr = args.pgbouncer_addr.clone();
    let admin_app = axum::Router::new()
        .route("/health", axum::routing::get(|| async { "OK" }))
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
        .merge(metrics::exporter::metrics_router(prom_metrics));

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
    tokio::signal::ctrl_c().await.unwrap();
    info!("SIGTERM/Ctrl-C received — gracefully shutting down all instances");

    // Gracefully shutdown all instances with timeout
    let shutdown_timeout = std::time::Duration::from_secs(30);
    supervisor.shutdown_all(shutdown_timeout).await;

    info!("All instances stopped — exiting");
    std::process::exit(0);
}
