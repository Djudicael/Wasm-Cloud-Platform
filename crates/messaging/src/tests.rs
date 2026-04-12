#[cfg(test)]
mod tests {
    use crate::{events::Event, NatsBus};
    use common::types::{AppConfig, AppId};
    use std::time::Duration;
    use testcontainers::{core::ContainerPort, runners::AsyncRunner, GenericImage, ImageExt};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_pub_sub_deploy_app() {
        let image = GenericImage::new("nats", "latest")
            .with_mapped_port(4222, ContainerPort::Tcp(4222))
            .with_cmd(vec!["-js"]); // enable JetStream
        let _container = image.start().await.expect("Failed to start NATS container");

        // Wait for NATS to boot up
        tokio::time::sleep(Duration::from_secs(2)).await;

        let url = "nats://127.0.0.1:4222".to_string();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");

        let (tx, mut rx) = mpsc::channel(1);

        // 1. Subscribe to the exact subject
        bus.subscribe("deploy.app.new", move |event| {
            let tx = tx.clone();
            async move {
                tx.send(event).await.unwrap();
            }
        })
        .await
        .unwrap();

        // Give the NATS server a few milliseconds to register the subscription
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 2. Publish an event
        let app_id = AppId::new("test-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id),
            wasm_bytes: vec![10, 20, 30],
            expected_hash: None,
        };

        bus.publish(&event).await.unwrap();

        // 3. Wait for the event to arrive at the subscriber
        let received = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out waiting for NATS message")
            .expect("Channel closed");

        // 4. Verify the contents
        match received {
            Event::DeployApp {
                app_id: recv_id,
                wasm_bytes,
                ..
            } => {
                assert_eq!(recv_id.0, "test-app:v1");
                assert_eq!(wasm_bytes, vec![10, 20, 30]);
            }
            _ => panic!("Received unexpected event variant"),
        }
    }

    #[tokio::test]
    async fn test_jetstream_durable_replay() {
        let image = GenericImage::new("nats", "latest")
            .with_mapped_port(4223, ContainerPort::Tcp(4222))
            .with_cmd(vec!["-js"]); // enable JetStream
        let _container = image.start().await.expect("Failed to start NATS container");

        // Wait for NATS to boot up
        tokio::time::sleep(Duration::from_secs(2)).await;

        let url = "nats://127.0.0.1:4223".to_string();
        let bus = NatsBus::connect(&url)
            .await
            .expect("Failed to connect to NATS");

        // 1. Set up JetStream (creates the DEPLOY stream if it doesn't exist)
        bus.setup_jetstream().await.unwrap();

        // 2. Publish an event BEFORE the durable consumer is created
        let app_id = AppId::new("durable-app", "v1");
        let event = Event::DeployApp {
            app_id: app_id.clone(),
            config: AppConfig::default_for(app_id.clone()),
            wasm_bytes: vec![99, 99, 99],
            expected_hash: None,
        };

        bus.publish(&event).await.unwrap();

        // Give JetStream a moment to persist the message
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (tx, mut rx) = mpsc::channel(10);

        // Generate a random consumer name so this test doesn't conflict with previous runs
        let consumer_name = format!("test_consumer_{}", uuid::Uuid::new_v4().simple());

        // 3. Create the durable consumer
        bus.subscribe_durable("DEPLOY", &consumer_name, move |event| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(event).await; // Ignore send errors if test already finished
            }
        })
        .await
        .unwrap();

        // 4. Wait for the replay of the missed message
        let received = timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("Timed out waiting for JetStream to replay the message")
            .expect("Channel closed");

        match received {
            Event::DeployApp {
                app_id: recv_id,
                wasm_bytes,
                ..
            } => {
                assert_eq!(recv_id.0, "durable-app:v1");
                assert_eq!(wasm_bytes, vec![99, 99, 99]);
            }
            _ => panic!("Received unexpected event variant from JetStream"),
        }
    }
}
