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
use events::Event;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct NatsBus {
    client: Client,
}

impl NatsBus {
    /// Connect to the NATS server.
    pub async fn connect(url: &str) -> Result<Self, PlatformError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| PlatformError::Messaging(format!("NATS connect: {e}")))?;
        tracing::info!(url, "connected to NATS");
        Ok(NatsBus { client })
    }

    /// Connect to the NATS server securely using a credentials file.
    pub async fn connect_secure(url: &str, creds_path: &str) -> Result<Self, PlatformError> {
        let options = async_nats::ConnectOptions::with_credentials_file(creds_path)
            .await
            .map_err(|e| PlatformError::Messaging(format!("failed to load creds: {e}")))?;
        let client = options
            .connect(url)
            .await
            .map_err(|e| PlatformError::Messaging(format!("NATS secure connect: {e}")))?;
        tracing::info!(url, "connected to NATS securely");
        Ok(NatsBus { client })
    }

    /// Publish an event to the appropriate subject.
    pub async fn publish(&self, event: &Event) -> Result<(), PlatformError> {
        let subject = event.subject();
        let payload =
            serde_json::to_vec(event).map_err(|e| PlatformError::Messaging(e.to_string()))?;
        self.client
            .publish(subject.clone(), payload.into())
            .await
            .map_err(|e| PlatformError::Messaging(format!("publish to {subject}: {e}")))?;
        Ok(())
    }

    /// Subscribe to a subject pattern and return a stream of Events.
    pub async fn subscribe<F, Fut>(&self, subject: &str, handler: F) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut sub = self
            .client
            .subscribe(subject.to_string())
            .await
            .map_err(|e| PlatformError::Messaging(format!("subscribe to {subject}: {e}")))?;

        let subject = subject.to_string();
        tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                match serde_json::from_slice::<Event>(&msg.payload) {
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
        .map_err(|e| PlatformError::Messaging(e.to_string()))?;

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
        .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        // Create "NODE" stream for node load and cluster events
        js.get_or_create_stream(StreamConfig {
            name: "NODE".to_string(),
            subjects: vec![
                "node.load.>".to_string(),
                "cluster.>".to_string(),
            ],
            max_messages: 10_000,
            ..Default::default()
        })
        .await
        .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        Ok(())
    }

    /// Subscribe to a durable JetStream consumer, acknowledging messages.
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
            .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        let consumer = stream
            .get_or_create_consumer(
                consumer_name,
                PullConfig {
                    durable_name: Some(consumer_name.to_string()),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        tokio::spawn(async move {
            while let Some(Ok(msg)) = messages.next().await {
                match serde_json::from_slice::<Event>(&msg.payload) {
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
                        let _ = msg.ack_with(async_nats::jetstream::AckKind::Nak(None)).await;
                    }
                }
            }
        });
        Ok(())
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}
