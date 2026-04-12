use common::{
    error::PlatformError,
    types::{AppConfig, AppId},
};
use storage::Store;

pub async fn handle_config_update(
    store: &Store,
    app_id: &AppId,
    new_config: AppConfig,
) -> Result<(), PlatformError> {
    // 1. Persist the new config
    store.save_config(&new_config)?;
    tracing::info!(app = %app_id.0, "config updated, effective on next instance spawn");

    // 2. Mark existing instances for graceful drain:
    //    - They finish their current requests.
    //    - New requests go to newly spawned instances that pick up the new config.
    //    (See step 10: Deployment Protocol for the full drain logic)

    Ok(())
}
