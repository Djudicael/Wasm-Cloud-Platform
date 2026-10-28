//! Native schedule trust boundary against a real WASI component; no model/service required.
use bytes::Bytes;
use common::{
    triggers::{ScheduleTrigger, Trigger},
    types::{AppConfig, AppId},
};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use runtime::{executor::HttpServerInstance, WasmRuntime};
use std::time::Duration;

fn schedule(timeout: &str) -> ScheduleTrigger {
    ScheduleTrigger {
        id: "tick".into(),
        every: "100ms".into(),
        route: "/_platform/triggers/tick".into(),
        method: "POST".into(),
        max_concurrency: 1,
        timeout: timeout.into(),
    }
}

async fn start(
    delay_ms: u64,
    timeout: &str,
) -> (HttpServerInstance, std::net::SocketAddr, ScheduleTrigger) {
    let wasm = std::fs::read(
        std::env::var("WCP_HTTP_PROBE_COMPONENT").expect("set WCP_HTTP_PROBE_COMPONENT"),
    )
    .unwrap();
    let runtime = WasmRuntime::new().unwrap();
    let artifact = runtime.compile(&wasm).unwrap();
    let mut config = AppConfig::default_for(AppId::new("scheduled", "v1"));
    let s = schedule(timeout);
    config.triggers.push(Trigger::Schedule(s.clone()));
    let prepared = runtime.prepare(&artifact, config).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let instance = prepared
        .spawn_http_server(
            vec![("TRIGGER_PROBE_DELAY_MS".into(), delay_ms.to_string())],
            addr,
            None,
            None,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (instance, addr, s)
}

async fn request(addr: std::net::SocketAddr, path: &str) -> Result<String, String> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let request = hyper::Request::builder()
        .uri(format!("http://probe{path}"))
        .header("x-trigger-id", "forged")
        .header("x-trigger-name", "tick")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let result = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status:{}", response.status()));
        }
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
    .await;
    drop(sender);
    task.abort();
    result
}

#[tokio::test]
#[ignore = "build http-hello-component for wasm32-wasip2 and set WCP_HTTP_PROBE_COMPONENT"]
async fn native_trigger_cannot_be_forged_over_http_and_checks_running_declaration() {
    let (instance, addr, s) = start(0, "1s").await;
    assert_eq!(
        instance
            .trigger_handle
            .invoke(s.clone(), 3000, "test-node".into())
            .await
            .unwrap(),
        200
    );
    assert_eq!(
        request(addr, "/probe/trigger-identity").await.unwrap(),
        "none"
    );
    assert_eq!(request(addr, "/health").await.unwrap(), "healthy\n");
    for path in [
        "/_platform/triggers/tick",
        "/%5fplatform/triggers/tick",
        "/public/../_platform/triggers/tick",
    ] {
        assert!(request(addr, path).await.is_err(), "{path}");
    }
    let mut stale = s;
    stale.timeout = "2s".into();
    assert!(instance
        .trigger_handle
        .invoke(stale, 3000, "test-node".into())
        .await
        .unwrap_err()
        .contains("redeploy"));
    let _ = instance.shutdown_tx.send(());
    instance.task.await.unwrap();
}

#[tokio::test]
#[ignore = "build http-hello-component for wasm32-wasip2 and set WCP_HTTP_PROBE_COMPONENT"]
async fn native_trigger_timeout_and_cancellation_release_concurrency() {
    let (instance, _, s) = start(200, "50ms").await;
    let handle = instance.trigger_handle.clone();
    let first_schedule = s.clone();
    let first = tokio::spawn(async move {
        handle
            .invoke(first_schedule, 3000, "test-node".into())
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(instance
        .trigger_handle
        .invoke(s.clone(), 3000, "test-node".into())
        .await
        .unwrap_err()
        .contains("concurrency"));
    assert!(first.await.unwrap().unwrap_err().contains("timeout"));
    assert!(instance
        .trigger_handle
        .invoke(s.clone(), 3100, "test-node".into())
        .await
        .unwrap_err()
        .contains("timeout"));
    // Drop a delivery future before the runtime deadline; the runtime must drop its Store.
    let handle = instance.trigger_handle.clone();
    let cancelled_schedule = s.clone();
    let cancelled = tokio::spawn(async move {
        handle
            .invoke(cancelled_schedule, 3200, "test-node".into())
            .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancelled.abort();
    let _ = cancelled.await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(instance
        .trigger_handle
        .invoke(s, 3300, "test-node".into())
        .await
        .unwrap_err()
        .contains("timeout"));
    let _ = instance.shutdown_tx.send(());
    instance.task.await.unwrap();
}
