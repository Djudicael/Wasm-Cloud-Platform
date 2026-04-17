use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteChangeWebhook {
    pub action: String,
    pub hostname: String,
    pub app_id: String,
    pub node_ips: Vec<String>,
}

pub struct DnsWebhookClient {
    endpoint: String,
    auth_token: String,
    client: reqwest::Client,
}

impl DnsWebhookClient {
    pub fn new(endpoint: String, auth_token: String) -> Self {
        DnsWebhookClient {
            endpoint,
            auth_token,
            client: reqwest::Client::new(),
        }
    }

    pub async fn notify(&self, payload: &RouteChangeWebhook) {
        match self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.auth_token)
            .json(payload)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(
                    action = %payload.action,
                    host = %payload.hostname,
                    "DNS webhook delivered successfully"
                );
            }
            Ok(resp) => {
                tracing::warn!(
                    action = %payload.action,
                    host = %payload.hostname,
                    status = %resp.status(),
                    "DNS webhook returned non-success status"
                );
            }
            Err(e) => {
                tracing::warn!(
                    action = %payload.action,
                    host = %payload.hostname,
                    error = %e,
                    "DNS webhook delivery failed"
                );
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct DnsWebhookManager {
    client: Option<Arc<DnsWebhookClient>>,
    node_ips: Arc<RwLock<Vec<String>>>,
}

impl DnsWebhookManager {
    pub fn new(endpoint: Option<String>, token: Option<String>) -> Option<Self> {
        match (endpoint, token) {
            (Some(endpoint), Some(token)) => Some(DnsWebhookManager {
                client: Some(Arc::new(DnsWebhookClient::new(endpoint, token))),
                node_ips: Arc::new(RwLock::new(Vec::new())),
            }),
            _ => None,
        }
    }

    pub async fn set_node_ips(&self, ips: Vec<String>) {
        let mut guard = self.node_ips.write().await;
        *guard = ips;
    }

    pub async fn notify_route_change(&self, action: &str, hostname: &str, app_id: &str) {
        let Some(client) = &self.client else {
            return;
        };

        let node_ips = self.node_ips.read().await.clone();

        let payload = RouteChangeWebhook {
            action: action.to_string(),
            hostname: hostname.to_string(),
            app_id: app_id.to_string(),
            node_ips,
        };

        client.notify(&payload).await;
    }
}
