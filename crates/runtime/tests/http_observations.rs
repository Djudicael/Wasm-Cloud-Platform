//! Real WASI measurements. Build the two HTTP fixtures explicitly before this test.
use common::{
    triggers::{ScheduleTrigger, Trigger},
    types::{AppConfig, AppId, FuelQuota},
};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::{
    executor::{HttpExecution, HttpServerInstance},
    WasmRuntime,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

type Samples = Arc<Mutex<Vec<HttpExecution>>>;

fn schedule() -> ScheduleTrigger {
    ScheduleTrigger {
        id: "tick".into(),
        every: "100ms".into(),
        route: "/_platform/triggers/tick".into(),
        method: "POST".into(),
        max_concurrency: 1,
        timeout: "50ms".into(),
    }
}

async fn start(p3: bool, delayed: bool) -> (HttpServerInstance, SocketAddr, Samples) {
    let target = std::env::var("CARGO_TARGET_DIR")
        .expect("set CARGO_TARGET_DIR to the explicitly built fixture target");
    let fixture = std::path::Path::new(&target)
        .join("wasm32-wasip2/debug")
        .join(if p3 {
            "http_p3_probe.wasm"
        } else {
            "http_hello_component.wasm"
        });
    let wasm = std::fs::read(&fixture)
        .unwrap_or_else(|error| panic!("build the WASI fixture {}: {error}", fixture.display()));
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&wasm).unwrap();
    let mut config = AppConfig::default_for(AppId::new("http-observations", "v1"));
    config.fuel_quota = FuelQuota(2_000_000_000);
    if !p3 {
        config.triggers.push(Trigger::Schedule(schedule()));
    }
    let prepared = runtime.prepare(&artifact, config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let samples: Samples = Arc::default();
    let sink = samples.clone();
    let server = prepared
        .spawn_http_server_observed(
            if delayed {
                vec![("TRIGGER_PROBE_DELAY_MS".into(), "200".into())]
            } else {
                vec![]
            },
            addr,
            None,
            None,
            Some(Arc::new(move |sample| sink.lock().unwrap().push(sample))),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (server, addr, samples)
}

async fn request(addr: SocketAddr, path: &str) -> Result<u16, ()> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake::<_, _, Empty<bytes::Bytes>>(
            TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let request = hyper::Request::builder()
        .uri(format!("http://probe{path}"))
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let result = async {
        let response = sender.send_request(request).await.map_err(|_| ())?;
        let status = response.status().as_u16();
        response.into_body().collect().await.map_err(|_| ())?;
        Ok(status)
    }
    .await;
    drop(sender);
    task.abort();
    result
}

async fn count(samples: &Samples, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if samples.lock().unwrap().len() >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(samples.lock().unwrap().len(), expected);
}

async fn success_and_trap(p3: bool) {
    let (server, addr, samples) = start(p3, false).await;
    assert_eq!(request(addr, "/health").await.unwrap(), 200);
    count(&samples, 1).await;
    assert_eq!(
        request(addr, "/_platform/triggers/tick").await.unwrap(),
        403
    );
    assert!(request(
        addr,
        if p3 {
            "/probe/http"
        } else {
            "/probe/tcp/connect"
        }
    )
    .await
    .is_err());
    count(&samples, 2).await;
    {
        let samples = samples.lock().unwrap();
        assert_eq!(samples[0].status_code, 200);
        assert!(samples[0].error.is_none());
        assert!(!samples[0].is_trap);
        assert_eq!(
            samples[1].status_code, 0,
            "trap must not invent an HTTP response"
        );
        assert!(samples[1].is_trap);
        assert_eq!(samples[1].error, Some("http_execution_error"));
        for sample in samples.iter() {
            assert!(sample.fuel_consumed > 0);
            assert!(sample.fuel_consumed <= sample.fuel_limit);
            assert!(sample.ram_bytes > 0);
        }
    }
    server.shutdown_tx.send(()).unwrap();
    let stats = server.task.await.unwrap();
    assert_eq!(
        stats.fuel_consumed,
        samples
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.fuel_consumed)
            .sum::<u64>()
    );
    assert!(stats.fuel_consumed > 0);
    assert_eq!(
        samples.lock().unwrap().len(),
        2,
        "service shutdown must not count another request"
    );
}

#[tokio::test]
async fn p2_guest_success_trap_and_admission_are_measured_once() {
    tokio::time::timeout(Duration::from_secs(60), success_and_trap(false))
        .await
        .unwrap();
}

#[tokio::test]
async fn p3_guest_success_trap_and_admission_are_measured_once() {
    tokio::time::timeout(Duration::from_secs(60), success_and_trap(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn native_trigger_success_timeout_and_cancel_are_measured_once() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (server, _, samples) = start(false, false).await;
        assert_eq!(
            server
                .trigger_handle
                .invoke(schedule(), 1000, "test-node".into())
                .await
                .unwrap(),
            200
        );
        count(&samples, 1).await;
        assert_eq!(samples.lock().unwrap()[0].status_code, 200);
        server.shutdown_tx.send(()).unwrap();
        let stats = server.task.await.unwrap();
        assert_eq!(
            stats.fuel_consumed,
            samples
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.fuel_consumed)
                .sum::<u64>()
        );

        let (server, _, samples) = start(false, true).await;
        assert!(server
            .trigger_handle
            .invoke(schedule(), 1000, "test-node".into())
            .await
            .unwrap_err()
            .contains("timeout"));
        count(&samples, 1).await;
        let handle = server.trigger_handle.clone();
        let task =
            tokio::spawn(async move { handle.invoke(schedule(), 2000, "test-node".into()).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await;
        count(&samples, 2).await;
        for sample in samples.lock().unwrap().iter() {
            assert!(sample.fuel_consumed > 0);
            assert!(sample.ram_bytes > 0);
            assert_eq!(sample.status_code, 0);
            assert_eq!(sample.error, Some("http_execution_cancelled"));
            assert!(!sample.is_trap);
        }
        // Shutdown must join a still-running guest before reading billing fuel.
        let handle = server.trigger_handle.clone();
        let pending =
            tokio::spawn(async move { handle.invoke(schedule(), 3000, "test-node".into()).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        server.shutdown_tx.send(()).unwrap();
        let stats = server.task.await.unwrap();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(
            stats.fuel_consumed,
            samples
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.fuel_consumed)
                .sum::<u64>()
        );
        assert_eq!(samples.lock().unwrap().len(), 3);
    })
    .await
    .unwrap();
}
