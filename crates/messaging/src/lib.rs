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
use tokio_stream::StreamExt;

#[derive(Clone)]
/// NATS message bus for publish/subscribe.
/// Cloning is cheap because [`async_nats::Client`] uses internal `Arc`s.
pub struct NatsBus {
    client: Client,
    /// Node ID included in every published `MessageEnvelope` so receivers
    /// can identify the sender and check protocol compatibility.
    node_id: String,
}

impl NatsBus {
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

        // Create the "DEPLOY" stream that retains deploy events
        js.get_or_create_stream(StreamConfig {
            name: "DEPLOY".to_string(),
            subjects: vec!["deploy.>".to_string(), "routes.>".to_string()],
            max_messages: 10_000,
            ..Default::default()
        })
        .await
        .map_err(PlatformError::messaging_source)?;

        // Create "CONTROL" stream for instance, secrets, config events
        js.get_or_create_stream(StreamConfig {
            name: "CONTROL".to_string(),
            subjects: vec![
                "instance.ready.>".to_string(),
                "instance.dead.>".to_string(),
                "secrets.update.>".to_string(),
                "config.update.>".to_string(),
            ],
            max_messages: 10_000,
            ..Default::default()
        })
        .await
        .map_err(PlatformError::messaging_source)?;

        // Create "NODE" stream for node load and cluster events
        js.get_or_create_stream(StreamConfig {
            name: "NODE".to_string(),
            subjects: vec!["node.load.>".to_string(), "cluster.>".to_string()],
            max_messages: 10_000,
            ..Default::default()
        })
        .await
        .map_err(PlatformError::messaging_source)?;

        Ok(())
    }

    /// Subscribe to a durable JetStream consumer, acknowledging messages.
    ///
    /// Messages are expected to be wrapped in a `MessageEnvelope`. If the
    /// envelope cannot be deserialized, a bare `Event` is tried as a fallback
    /// for backward compatibility. Incompatible protocol versions are NAK'd
    /// so they can be redelivered to a compatible node.
    pub async fn subscribe_durable<F, Fut>(
        &self,
        stream_name: &str,
        consumer_name: &str,
        handler: F,
    ) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let js = jetstream::new(self.client.clone());
        let stream = js
            .get_stream(stream_name)
            .await
            .map_err(PlatformError::messaging_source)?;

        let consumer = stream
            .get_or_create_consumer(
                consumer_name,
                PullConfig {
                    durable_name: Some(consumer_name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .map_err(PlatformError::messaging_source)?;

        let mut messages = consumer
            .messages()
            .await
            .map_err(PlatformError::messaging_source)?;

        tokio::spawn(async move {
            while let Some(Ok(msg)) = messages.next().await {
                match Self::deserialize_event(&msg.payload) {
                    Ok(event) => {
                        handler(event).await;
                        let _ = msg.ack().await;
                    }
                    Err(e) => {
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
                if let Ok(event) = serde_json::from_slice::<Event>(&msg.payload) {
                    let _ = tx.send(event);
                }
            }
        });

        rx.await
            .map_err(|_| PlatformError::messaging("timeout waiting for event".to_string()))
    }
}
