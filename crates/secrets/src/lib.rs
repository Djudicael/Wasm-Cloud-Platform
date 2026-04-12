use async_trait::async_trait;
use common::error::PlatformError;
use common::types::AppId;

#[async_trait]
pub trait SecretProvider: Send + Sync {
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError>;
}

pub fn noop() {}
