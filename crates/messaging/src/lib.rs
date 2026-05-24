pub mod events;
pub mod publisher;
pub mod reconnect;

#[cfg(test)]
mod tests;

use async_nats::jetstream::{
    self, consumer::pull::Config as PullConfig, stream::Config as StreamConfig,
};
use async_nats::Client;
use common::error::PlatformError;
use common::protocol::{MessageEnvelope, PROTOCOL_VERSION};
use events::Event;
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt;

const DURABLE_MAX_DELIVER: i64 = 3;
pub const QUARANTINE_STREAM: &str = "QUARANTINE";
const QUARANTINE_SUBJECT_PREFIX: &str = "quarantine";
pub const DEPLOY_STREAM_SUBJECTS: &[&str] = &["deploy.>", "routes.>"];
pub const CONTROL_STREAM_SUBJECTS: &[&str] = &[
    "instance.ready.>",
    "instance.dead.>",
    "secrets.update.>",
    "config.update.>",
    "gateway.config.>",
];
pub const NODE_STREAM_SUBJECTS: &[&str] =
    &["node.load.>", "cluster.node_joined.>", "cluster.snapshot.>"];
pub const HEALTH_STREAM_SUBJECTS: &[&str] =
    &["cluster.health.changed.>", "cluster.health.snapshot.>"];
pub const PLATFORM_STREAM_SUBJECTS: &[&str] = &[
    "platform.upgrade.>",
    "platform.upgrade_complete.>",
    "platform.draining.>",
    "config.hot_reload.>",
];
pub const EBPF_STREAM_SUBJECTS: &[&str] = &[
    "ebpf.pressure.*",
    "ebpf.pressure.recovered.*",
    "ebpf.security.incident.*",
];
pub const QUARANTINE_STREAM_SUBJECTS: &[&str] = &["quarantine.>"];
pub const JETSTREAM_STREAM_SUBJECT_SPECS: &[(&str, &[&str])] = &[
    ("DEPLOY", DEPLOY_STREAM_SUBJECTS),
    ("CONTROL", CONTROL_STREAM_SUBJECTS),
    ("NODE", NODE_STREAM_SUBJECTS),
    ("HEALTH", HEALTH_STREAM_SUBJECTS),
    ("PLATFORM", PLATFORM_STREAM_SUBJECTS),
    ("EBPF", EBPF_STREAM_SUBJECTS),
    (QUARANTINE_STREAM, QUARANTINE_STREAM_SUBJECTS),
];

#[derive(Debug, serde::Serialize)]
struct QuarantinedJetStreamMessage {
    stream: String,
    consumer: String,
    original_subject: String,
    stream_sequence: u64,
    consumer_sequence: u64,
    delivered: i64,
    reason: String,
    quarantined_at: String,
    payload: Vec<u8>,
}

#[derive(Clone)]
/// NATS message bus for publish/subscribe.
/// Cloning is cheap because [`async_nats::Client`] uses internal `Arc`s.
pub struct NatsBus {
    client: Client,
    /// Node ID included in every published `MessageEnvelope` so receivers
    /// can identify the sender and check protocol compatibility.
    node_id: String,
}

impl std::fmt::Debug for NatsBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsBus")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl NatsBus {
    async fn create_stream_with_retry(
        js: &jetstream::Context,
        name: &str,
        subjects: &[&str],
    ) -> Result<(), PlatformError> {
        const MAX_ATTEMPTS: usize = 20;
        const RETRY_DELAY: Duration = Duration::from_millis(250);

        for attempt in 1..=MAX_ATTEMPTS {
            let result = js
                .get_or_create_stream(StreamConfig {
                    name: name.to_string(),
                    subjects: subjects.iter().map(|subject| subject.to_string()).collect(),
                    max_messages: 10_000,
                    ..Default::default()
                })
                .await;

            match result {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let error_text = error.to_string();
                    let retryable = error_text.contains("jetstream unavailable")
                        || error_text.contains("JetStreamUnavailable")
                        || error_text.contains("503");

                    if retryable && attempt < MAX_ATTEMPTS {
                        tracing::debug!(
                            stream = name,
                            attempt,
                            error = %error,
                            "JetStream not ready yet, retrying stream creation"
                        );
                        sleep(RETRY_DELAY).await;
                        continue;
                    }

                    return Err(PlatformError::messaging_source(error));
                }
            }
        }

        Err(PlatformError::messaging(
            "exhausted JetStream setup retries unexpectedly",
        ))
    }

    /// Connect to the NATS server.
    pub async fn connect(url: &str) -> Result<Self, PlatformError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| PlatformError::messaging(format!("NATS connect: {e}")))?;
        tracing::info!(url, "connected to NATS");
        Ok(NatsBus {
            client,
            node_id: "unknown".to_string(),
        })
    }

    /// Connect to the NATS server securely using a credentials file.
    pub async fn connect_secure(url: &str, creds_path: &str) -> Result<Self, PlatformError> {
        let options = async_nats::ConnectOptions::with_credentials_file(creds_path)
            .await
            .map_err(|e| PlatformError::messaging(format!("failed to load creds: {e}")))?;
        let client = options
            .connect(url)
            .await
            .map_err(|e| PlatformError::messaging(format!("NATS secure connect: {e}")))?;
        tracing::info!(url, "connected to NATS securely");
        Ok(NatsBus {
            client,
            node_id: "unknown".to_string(),
        })
    }

    /// Set the node ID used in published `MessageEnvelope` headers.
    /// Must be called before any `publish()` calls for the sender field
    /// to be meaningful.
    pub fn set_node_id(&mut self, node_id: String) {
        self.node_id = node_id;
    }

    /// Publish an event to the appropriate subject, wrapped in a `MessageEnvelope`
    /// that carries the protocol version and sender identity.
    ///
    /// Subscribers can check `envelope.is_compatible()` before processing the
    /// payload, enabling safe rolling upgrades across protocol versions.
    pub async fn publish(&self, event: &Event) -> Result<(), PlatformError> {
        let subject = event.subject();
        let envelope = MessageEnvelope::new(&self.node_id, event.clone());
        let payload = serde_json::to_vec(&envelope).map_err(PlatformError::messaging_source)?;
        self.client
            .publish(subject.clone(), payload.into())
            .await
            .map_err(|e| PlatformError::messaging(format!("publish to {subject}: {e}")))?;
        Ok(())
    }

    /// Subscribe to a subject pattern and return a stream of Events.
    ///
    /// Messages are expected to be wrapped in a `MessageEnvelope`. If the
    /// envelope cannot be deserialized, a bare `Event` is tried as a fallback
    /// for backward compatibility with older nodes that publish without an envelope.
    /// Incompatible protocol versions are logged and skipped.
    pub async fn subscribe<F, Fut>(&self, subject: &str, handler: F) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut sub = self
            .client
            .subscribe(subject.to_string())
            .await
            .map_err(|e| PlatformError::messaging(format!("subscribe to {subject}: {e}")))?;

        let subject = subject.to_string();
        tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                match Self::deserialize_event(&msg.payload) {
                    Ok(event) => handler(event).await,
                    Err(e) => tracing::warn!(
                        subject = %subject,
                        error = %e,
                        "failed to deserialize NATS message"
                    ),
                }
            }
        });
        Ok(())
    }

    /// Create durable JetStream subjects for deployment events.
    pub async fn setup_jetstream(&self) -> Result<(), PlatformError> {
        let js = jetstream::new(self.client.clone());

        Self::create_stream_with_retry(&js, "DEPLOY", DEPLOY_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, "CONTROL", CONTROL_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, "NODE", NODE_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, "HEALTH", HEALTH_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, "PLATFORM", PLATFORM_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, "EBPF", EBPF_STREAM_SUBJECTS).await?;
        Self::create_stream_with_retry(&js, QUARANTINE_STREAM, QUARANTINE_STREAM_SUBJECTS).await?;

        Ok(())
    }

    /// Subscribe to a durable JetStream consumer, acknowledging messages.
    ///
    /// `filter_subject` narrows the consumer to a specific subset of subjects
    /// inside the stream. This is important for correctness when a single
    /// stream contains multiple event classes.
    ///
    /// Messages are expected to be wrapped in a `MessageEnvelope`. If the
    /// envelope cannot be deserialized, a bare `Event` is tried as a fallback
    /// for backward compatibility. Incompatible protocol versions or handler
    /// failures are NAK'd so they can be redelivered (up to the consumer's
    /// retry limit).
    pub async fn subscribe_durable<F, Fut>(
        &self,
        stream_name: &str,
        consumer_name: &str,
        filter_subject: Option<&str>,
        handler: F,
    ) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), PlatformError>> + Send + 'static,
    {
        let js = jetstream::new(self.client.clone());
        let stream = js
            .get_stream(stream_name)
            .await
            .map_err(PlatformError::messaging_source)?;

        let filter_subject = filter_subject.unwrap_or_default().to_string();
        let consumer = stream
            .get_or_create_consumer(
                consumer_name,
                PullConfig {
                    durable_name: Some(consumer_name.to_string()),
                    filter_subject,
                    // Give the consumer 30 seconds to process and ACK each message
                    // before it becomes eligible for redelivery.
                    ack_wait: std::time::Duration::from_secs(30),
                    // Redeliver up to 3 times before giving up on a message.
                    // Prevents infinite redelivery loops for poison messages.
                    max_deliver: 3,
                    ..Default::default()
                },
            )
            .await
            .map_err(PlatformError::messaging_source)?;

        let mut messages = consumer
            .messages()
            .await
            .map_err(PlatformError::messaging_source)?;

        let quarantine_client = self.client.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = messages.next().await {
                match Self::deserialize_event(&msg.payload) {
                    Ok(event) => match handler(event).await {
                        Ok(()) => {
                            let _ = msg.ack().await;
                        }
                        Err(e) => {
                            if Self::delivery_exhausted(&msg) {
                                Self::quarantine_message(
                                    &quarantine_client,
                                    &msg,
                                    format!("handler failed after retry exhaustion: {e}"),
                                )
                                .await;
                                let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                                continue;
                            }
                            tracing::warn!(
                                error = %e,
                                "JetStream handler failed; NAKing for redelivery"
                            );
                            let _ = msg
                                .ack_with(async_nats::jetstream::AckKind::Nak(None))
                                .await;
                        }
                    },
                    Err(e) => {
                        if Self::delivery_exhausted(&msg) {
                            Self::quarantine_message(
                                &quarantine_client,
                                &msg,
                                format!("deserialization failed after retry exhaustion: {e}"),
                            )
                            .await;
                            let _ = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                            continue;
                        }
                        tracing::warn!(
                            error = %e,
                            "failed to deserialize NATS JetStream message"
                        );
                        // NAK malformed messages so they can be redelivered (up to retry limit)
                        // This prevents permanent data loss if the message format changes
                        let _ = msg
                            .ack_with(async_nats::jetstream::AckKind::Nak(None))
                            .await;
                    }
                }
            }
        });
        Ok(())
    }

    fn delivery_exhausted(msg: &async_nats::jetstream::Message) -> bool {
        msg.info()
            .map(|info| info.delivered >= DURABLE_MAX_DELIVER)
            .unwrap_or(false)
    }

    async fn quarantine_message(
        client: &Client,
        msg: &async_nats::jetstream::Message,
        reason: String,
    ) {
        let info = match msg.info() {
            Ok(info) => info,
            Err(error) => {
                tracing::error!(error = %error, "failed to read JetStream message metadata for quarantine");
                return;
            }
        };
        let record = QuarantinedJetStreamMessage {
            stream: info.stream.to_string(),
            consumer: info.consumer.to_string(),
            original_subject: msg.subject.to_string(),
            stream_sequence: info.stream_sequence,
            consumer_sequence: info.consumer_sequence,
            delivered: info.delivered,
            reason,
            quarantined_at: chrono::Utc::now().to_rfc3339(),
            payload: msg.payload.to_vec(),
        };
        let subject = format!(
            "{QUARANTINE_SUBJECT_PREFIX}.{}.{}",
            sanitize_subject_token(&record.stream),
            sanitize_subject_token(&record.consumer)
        );
        match serde_json::to_vec(&record) {
            Ok(payload) => {
                if let Err(error) = client.publish(subject, payload.into()).await {
                    tracing::error!(error = %error, "failed to publish quarantined JetStream message");
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "failed to serialize quarantined JetStream message");
            }
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Deserialize a NATS payload into an `Event`.
    ///
    /// Tries `MessageEnvelope<Event>` first (the canonical wire format).
    /// If the envelope is present but the protocol version is incompatible,
    /// returns an error so the caller can NAK the message.
    /// Falls back to bare `Event` deserialization for backward compatibility
    /// with nodes that have not yet adopted the envelope format.
    fn deserialize_event(payload: &[u8]) -> Result<Event, String> {
        // Try envelope-wrapped format first
        if let Ok(envelope) = serde_json::from_slice::<MessageEnvelope<Event>>(payload) {
            if !envelope.is_compatible() {
                return Err(format!(
                    "incompatible protocol version {} (current: {}, min: {})",
                    envelope.protocol_version,
                    PROTOCOL_VERSION,
                    common::protocol::MIN_COMPATIBLE_PROTOCOL,
                ));
            }
            return Ok(envelope.payload);
        }

        // Fallback: bare Event (backward compatibility with older nodes)
        serde_json::from_slice::<Event>(payload).map_err(|e| format!("deserialization failed: {e}"))
    }

    /// Wait for the first event matching the subject pattern.
    /// This is useful for cluster bootstrap where we need to wait for StateSnapshot.
    pub async fn wait_for_event(&self, subject_pattern: &str) -> Result<Event, PlatformError> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        let mut sub = self
            .client
            .subscribe(subject_pattern.to_string())
            .await
            .map_err(|e| {
                PlatformError::messaging(format!("subscribe to {subject_pattern}: {e}"))
            })?;

        tokio::spawn(async move {
            if let Some(msg) = sub.next().await {
                if let Ok(envelope) = serde_json::from_slice::<MessageEnvelope<Event>>(&msg.payload)
                {
                    let _ = tx.send(envelope.payload);
                } else if let Ok(event) = serde_json::from_slice::<Event>(&msg.payload) {
                    let _ = tx.send(event);
                }
            }
        });

        rx.await
            .map_err(|_| PlatformError::messaging("timeout waiting for event".to_string()))
    }
}

fn sanitize_subject_token(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
