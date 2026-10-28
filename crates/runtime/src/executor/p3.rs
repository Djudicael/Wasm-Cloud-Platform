//! P3 HTTP services run on the same dedicated identity thread as P2 services.
use super::*;
use http_body_util::{BodyExt, Full, Limited};
use wasmtime_wasi_http::p3::{bindings::ServicePre, Request};
mod sockets;

const MAX_BODY: usize = 16 * 1024 * 1024;

pub(super) fn is_service(engine: &Engine, module: &Component) -> bool {
    module
        .component_type()
        .exports(engine)
        .any(|(name, _)| name.starts_with("wasi:http/handler@0.3."))
}

pub(super) fn linker(engine: &Engine) -> Result<Linker<StoreState>, PlatformError> {
    let mut linker = build_async_http_linker(engine)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)
        .and_then(|()| wasmtime_wasi_http::p3::add_to_linker(&mut linker))
        .and_then(|()| sockets::add_to_linker(&mut linker))
        .map_err(|e| PlatformError::runtime(format!("P3 linker error: {e}")))?;
    Ok(linker)
}

pub(super) fn spawn(
    prepared: &PreparedModule,
    env: Vec<(String, String)>,
    addr: SocketAddr,
    check: Option<SocketAddrCheckFn>,
    hook: Option<HttpServerThreadStartHook>,
) -> Result<HttpServerInstance, PlatformError> {
    if !prepared.config.triggers.is_empty() {
        return Err(PlatformError::ConfigValidation(
            "P3 native triggers are not supported".into(),
        ));
    }
    let pre = linker(&prepared.engine)?
        .instantiate_pre(&prepared.module)
        .and_then(ServicePre::new)
        .map_err(|e| PlatformError::runtime(format!("P3 service contract error: {e}")))?;
    let pre = Arc::new(pre);
    let engine = prepared.engine.clone();
    let config = prepared.config.clone();
    let policy = resolve_instance_policy(&config, addr.port())?;
    let counters = Arc::new(PolicyCounters::new());
    let task_counters = counters.clone();
    let error_config = config.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let (trigger_tx, _) = tokio::sync::mpsc::channel(1);
    let task = spawn_dedicated_current_thread(
        hook,
        move || async move {
            let start = Instant::now();
            let result = async {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let (client, _) = accepted?;
                        let (pre, engine, config, env, check, counters) =
                            (pre.clone(), engine.clone(), config.clone(), env.clone(), check.clone(), task_counters.clone());
                        connections.spawn(async move {
                            let service = service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                                let (pre, engine, config, env, check, counters) =
                                    (pre.clone(), engine.clone(), config.clone(), env.clone(), check.clone(), counters.clone());
                                async move {
                                    if common::triggers::reserved_network_path(req.uri().path()) {
                                        return Ok::<_, wasmtime::Error>(hyper::Response::builder().status(403).body(Full::new(bytes::Bytes::new()))?);
                                    }
                                    let names: Vec<_> = req.headers().keys().filter(|n| n.as_str().starts_with("x-trigger-")).cloned().collect(); for name in names { req.headers_mut().remove(name); }
                                    let state = build_store_state(&config, env, addr.port(), check, Some(counters))?;
                                    let mut store = Store::new(&engine, state);
                                    store.limiter(|s| &mut s.limiter);
                                    configure_store(&mut store, config.fuel_quota)?;
                                    let service = pre.instantiate_async(&mut store).await?;
                                    let req = req.map(|b| Limited::new(b, MAX_BODY).map_err(|_| wasmtime_wasi_http::Error::HttpRequestBodySize(None)));
                                    let (req, io) = Request::from_http(&mut store.data_mut().http_hooks, req);
                                    let response = tokio::time::timeout(std::time::Duration::from_secs(30), store.run_concurrent(async |accessor| {
                                        let (response, ()) = tokio::try_join!(async {
                                            let response = service.handle(accessor, req).await?
                                                .map_err(|e| wasmtime::format_err!("P3 HTTP handler failed: {e:?}"))?;
                                            let response = accessor.with(|s| response.into_http(s, async { Ok(()) }))?;
                                            let (parts, body) = response.into_parts();
                                            let body = Limited::new(body, MAX_BODY).collect().await.map_err(|e| wasmtime::format_err!("P3 response body error: {e}"))?.to_bytes();
                                            wasmtime::error::Ok(hyper::Response::from_parts(parts, Full::new(body)))
                                        }, async { io.await.map_err(wasmtime::Error::from) })?;
                                        wasmtime::error::Ok(response)
                                    })).await???;
                                    Ok(response)
                                }
                            });
                            if let Err(error) = http2::Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(client), service).await {
                                tracing::warn!(%error, "P3 HTTP adapter connection failed");
                            }
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            Ok::<(), std::io::Error>(())
        }.await;
            stats(
                &config,
                &task_counters,
                start.elapsed().as_millis() as u64,
                result.err().map(|e| e.to_string()),
            )
        },
        move |e| {
            stats(
                &error_config,
                &PolicyCounters::new(),
                0,
                Some(e.to_string()),
            )
        },
    );
    Ok(HttpServerInstance {
        task,
        shutdown_tx,
        policy_counters: counters,
        trigger_handle: TriggerHandle(trigger_tx),
        policy,
    })
}

fn stats(
    config: &AppConfig,
    counters: &PolicyCounters,
    wall_clock_ms: u64,
    trap: Option<String>,
) -> ExecutionStats {
    ExecutionStats {
        instance_id: InstanceId::new(),
        fuel_limit: config.fuel_quota.0,
        fuel_consumed: 0,
        ram_bytes: counters
            .current_memory_bytes
            .load(std::sync::atomic::Ordering::Relaxed) as usize,
        wall_clock_ms,
        trap,
        io_stats: IoStats {
            open_fds_peak: counters
                .open_fds_peak
                .load(std::sync::atomic::Ordering::Relaxed),
            fs_bytes_written: counters
                .fs_write_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            net_egress_bytes: counters
                .egress_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            outbound_connections: counters
                .outbound_connections_total
                .load(std::sync::atomic::Ordering::Relaxed)
                as u32,
        },
    }
}
