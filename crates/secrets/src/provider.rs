use async_trait::async_trait;
use common::{error::PlatformError, types::AppId};

/// Abstraction over different secret backends.
/// The Supervisor uses this to get secret values at spawn time.
#[async_trait]
pub trait SecretProvider: Send + Sync + 'static {
    /// Get the plaintext value of a secret for the given app.
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError>;

    /// Set (or update) a secret.
    async fn set(&self, app_id: &AppId, key: &str, value: &str) -> Result<(), PlatformError>;

    /// Delete a secret.
    async fn delete(&self, app_id: &AppId, key: &str) -> Result<(), PlatformError>;

    /// List all secret keys for an app.
    async fn list_keys(&self, app_id: &AppId) -> Result<Vec<String>, PlatformError>;
}
