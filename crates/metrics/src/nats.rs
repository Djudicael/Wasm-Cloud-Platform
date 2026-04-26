// crates/metrics/src/nats.rs
use prometheus::{Gauge, GaugeVec, IntCounterVec, Opts, Registry};
use tokio::time::{interval, Duration};

#[derive(Clone)]
pub struct NatsMetrics {
    pub consumer_pending: GaugeVec,
    pub consumer_redelivered: IntCounterVec,
    pub stream_bytes: Gauge,
    pub stream_messages: Gauge,
    pub connection_healthy: Gauge,
    pub reconnect_count: IntCounterVec,
}

impl NatsMetrics {
    pub fn register(registry: &Registry) -> Self {
        let consumer_pending = GaugeVec::new(
            Opts::new(
                "nats_consumer_pending_messages",
                "Number of unprocessed messages in a JetStream consumer",
            ),
            &["stream", "consumer"],
        )
        .unwrap();
        registry
            .register(Box::new(consumer_pending.clone()))
            .unwrap();

        let consumer_redelivered = IntCounterVec::new(
            Opts::new(
                "nats_consumer_redelivered_total",
                "Total redelivered messages",
            ),
            &["stream", "consumer"],
        )
        .unwrap();
        registry
            .register(Box::new(consumer_redelivered.clone()))
            .unwrap();

        let stream_bytes = Gauge::new(
            "nats_stream_bytes",
            "Total bytes stored in the main JetStream",
        )
        .unwrap();
        registry.register(Box::new(stream_bytes.clone())).unwrap();

        let stream_messages = Gauge::new(
            "nats_stream_messages",
            "Total messages stored in the main JetStream",
        )
        .unwrap();
        registry
            .register(Box::new(stream_messages.clone()))
            .unwrap();

        let connection_healthy = Gauge::new(
            "nats_connection_healthy",
            "1 if connected to NATS, 0 otherwise",
        )
        .unwrap();
        registry
            .register(Box::new(connection_healthy.clone()))
            .unwrap();

        let reconnect_count = IntCounterVec::new(
            Opts::new("nats_reconnect_total", "Total NATS reconnection events"),
            &["node"],
        )
        .unwrap();
        registry
            .register(Box::new(reconnect_count.clone()))
            .unwrap();

        NatsMetrics {
            consumer_pending,
            consumer_redelivered,
            stream_bytes,
            stream_messages,
            connection_healthy,
            reconnect_count,
        }
    }
}

pub async fn nats_monitor_loop(client: async_nats::Client, metrics: NatsMetrics) {
    let mut ticker = interval(Duration::from_secs(30));
    let node_id = std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string());

    let js = async_nats::jetstream::new(client.clone());
    let mut was_connected = false;
    // Track whether we've ever successfully connected at least once.
    // Only increment reconnect_count on transitions from disconnected →
    // connected AFTER the first connection, so the initial connect isn't
    // counted as a "reconnect".
    let mut ever_connected = false;

    loop {
        ticker.tick().await;

        let state = client.connection_state();
        if state == async_nats::connection::State::Connected {
            metrics.connection_healthy.set(1.0);
            if !was_connected && ever_connected {
                metrics.reconnect_count.with_label_values(&[&node_id]).inc();
            }
            was_connected = true;
            ever_connected = true;
        } else {
            metrics.connection_healthy.set(0.0);
            was_connected = false;
        }

        match js.get_stream("WASM_PLATFORM").await {
            Ok(mut stream) => {
                if let Ok(info) = stream.info().await {
                    metrics.stream_bytes.set(info.state.bytes as f64);
                    metrics.stream_messages.set(info.state.messages as f64);
                }

                // Track JetStream API connection health
                metrics.connection_healthy.set(1.0);
            }
            Err(_) => {
                metrics.connection_healthy.set(0.0);
            }
        }
    }
}
