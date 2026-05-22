use prometheus::{Gauge, GaugeVec, IntCounterVec, Opts, Registry};
use tokio::time::{interval, Duration};

const JETSTREAM_STREAMS: &[&str] = &["DEPLOY", "CONTROL", "NODE", "HEALTH", "PLATFORM", "EBPF"];

#[derive(Clone)]
pub struct NatsMetrics {
    pub consumer_pending: GaugeVec,
    pub consumer_redelivered: IntCounterVec,
    pub stream_bytes: GaugeVec,
    pub stream_messages: GaugeVec,
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

        let stream_bytes = GaugeVec::new(
            Opts::new(
                "nats_stream_bytes",
                "Total bytes stored in each JetStream stream",
            ),
            &["stream"],
        )
        .unwrap();
        registry.register(Box::new(stream_bytes.clone())).unwrap();

        let stream_messages = GaugeVec::new(
            Opts::new(
                "nats_stream_messages",
                "Total messages stored in each JetStream stream",
            ),
            &["stream"],
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

fn monitored_stream_names() -> &'static [&'static str] {
    JETSTREAM_STREAMS
}

pub async fn nats_monitor_loop(client: async_nats::Client, metrics: NatsMetrics) {
    let mut ticker = interval(Duration::from_secs(30));
    let node_id = std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string());

    let js = async_nats::jetstream::new(client.clone());
    let mut was_connected = false;
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

        let mut jetstream_healthy = true;
        for stream_name in monitored_stream_names() {
            match js.get_stream(stream_name).await {
                Ok(mut stream) => match stream.info().await {
                    Ok(info) => {
                        metrics
                            .stream_bytes
                            .with_label_values(&[stream_name])
                            .set(info.state.bytes as f64);
                        metrics
                            .stream_messages
                            .with_label_values(&[stream_name])
                            .set(info.state.messages as f64);
                    }
                    Err(_) => {
                        jetstream_healthy = false;
                    }
                },
                Err(_) => {
                    metrics
                        .stream_bytes
                        .with_label_values(&[stream_name])
                        .set(0.0);
                    metrics
                        .stream_messages
                        .with_label_values(&[stream_name])
                        .set(0.0);
                    jetstream_healthy = false;
                }
            }
        }

        if !jetstream_healthy && state != async_nats::connection::State::Connected {
            metrics.connection_healthy.set(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::monitored_stream_names;

    #[test]
    fn test_monitored_stream_names_match_expected_streams() {
        assert_eq!(
            monitored_stream_names(),
            &["DEPLOY", "CONTROL", "NODE", "HEALTH", "PLATFORM", "EBPF"]
        );
    }
}
