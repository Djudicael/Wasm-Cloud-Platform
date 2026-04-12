use async_nats::Client;
use common::types::AppId;
use secrets::LocalSecretProvider;
use secrets::SecretProvider;
use tokio_stream::StreamExt;

/// Subject pattern: "secrets.update.<app_id>"
pub async fn handle_secret_rotation(nats: &Client, secret_provider: &LocalSecretProvider) {
    let mut sub = nats.subscribe("secrets.update.>").await.unwrap();
    while let Some(msg) = sub.next().await {
        // Subject: secrets.update.api-users:v2
        let app_id_str = msg.subject.strip_prefix("secrets.update.").unwrap_or("");
        let app_id = AppId(app_id_str.to_string());

        // The NATS message body contains JSON: { "key": "DATABASE_URL", "value": "postgres://..." }
        if let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let key = payload["key"].as_str().unwrap_or("");
            let value = payload["value"].as_str().unwrap_or("");
            if !key.is_empty() {
                match secret_provider.set(&app_id, key, value).await {
                    Ok(_) => tracing::info!(app = app_id_str, key, "secret rotated"),
                    Err(e) => {
                        tracing::error!(app = app_id_str, key, error = %e, "rotation failed")
                    }
                }
            }
        }
    }
}
