use super::*;
use futures::StreamExt;

/// Regression for recovery callbacks invoked by a non-Tokio thread.
#[tokio::test]
#[ignore = "requires WASM_TEST_OWNED_NATS_URL from an owned local rehearsal"]
async fn recovery_callback_publishes_from_foreign_thread() {
    let url = std::env::var("WASM_TEST_OWNED_NATS_URL").expect("owned local NATS URL required");
    let bus = messaging::NatsBus::connect(&url).await.unwrap();
    let node_id = format!("callback-regression-{}", std::process::id());
    let subject = format!("ebpf.pressure.recovered.{node_id}");
    let mut messages = bus.client().subscribe(subject.clone()).await.unwrap();
    bus.client().flush().await.unwrap();
    let (supervisor_tx, _rx) = tokio::sync::mpsc::channel(2);
    let callback = NodeEbpfCallbacks {
        runtime: tokio::runtime::Handle::current(),
        backpressure: proxy::backpressure::BackpressureSignal::new(),
        nats_health: Arc::new(NatsHealth::new()),
        bus,
        node_id: node_id.clone(),
        supervisor_tx,
    };
    std::thread::spawn(move || callback.publish_node_pressure_recovered(&node_id))
        .join()
        .expect("recovery callback must not need a runtime on its caller thread");
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), messages.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.subject.as_str(), subject);
    assert!(serde_json::from_slice::<serde_json::Value>(&message.payload).is_ok());
}
