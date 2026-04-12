use clap::Parser;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

pub mod handlers;

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

    let env_resolver = Arc::new(|config: &common::types::AppConfig, _host_port: u16| {
        let mut vars = Vec::new();
        for (k, v) in &config.env_vars {
            vars.push((k.clone(), v.clone()));
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

    let dispatcher = Arc::new(handlers::EventDispatcher {
        supervisor: supervisor.clone(),
        upstream: upstream_registry.clone(),
        host_router: host_router.clone(),
        store: store.clone(),
        runtime: runtime.clone(),
    });

    {
        let d = dispatcher.clone();
        bus.subscribe_durable("DEPLOY", &args.node_id, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    for subject in &[
        "instance.ready.>",
        "instance.dead.>",
        "secrets.update.>",
        "config.update.>",
        "node.load.>",
    ] {
        let d = dispatcher.clone();
        bus.subscribe(subject, move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
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

    let wasm_proxy = proxy::service::WasmProxy {
        router: host_router.clone(),
        upstream: upstream_registry.clone(),
        rate_limiter: Arc::new(proxy::rate_limiter::RateLimiter::new(100.0, 100.0)),
        node_table: Arc::new(proxy::node_table::NodeLoadTable::default()),
        cold_start,
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

    let admin_app = axum::Router::new().route("/health", axum::routing::get(|| async { "OK" }));

    let admin_addr = format!("0.0.0.0:{}", args.admin_port);
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .expect("admin API bind failed");
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app).await.unwrap();
    });

    info!(
        http = args.proxy_port,
        https = args.proxy_https_port,
        admin = args.admin_port,
        "node fully started"
    );

    // Run Pingora in background to allow graceful shutdown setup
    std::thread::spawn(move || {
        proxy_server.run();
    });

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await.unwrap();
    info!("Shutting down...");

    Ok(())
}
