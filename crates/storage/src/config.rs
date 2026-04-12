// crates/storage/src/config.rs
use crate::{tables::CONFIGS, Store};
use common::{
    error::PlatformError,
    types::{AppConfig, AppId},
};
use redb::ReadableTable;

impl Store {
    pub fn save_config(&self, config: &AppConfig) -> Result<(), PlatformError> {
        let json =
            serde_json::to_string(config).map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(CONFIGS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(config.id.0.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn load_config(&self, id: &AppId) -> Result<Option<AppConfig>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table
            .get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            Some(v) => {
                let config = serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Update a single env var without touching the rest of the config.
    /// Safe for live config updates without restart.
    pub fn set_env_var(&self, id: &AppId, key: &str, value: &str) -> Result<(), PlatformError> {
        let mut config = self
            .load_config(id)?
            .ok_or_else(|| PlatformError::AppNotFound(id.0.clone()))?;
        config.env_vars.insert(key.to_string(), value.to_string());
        self.save_config(&config)
    }

    /// List all deployed app IDs.
    pub fn list_apps(&self) -> Result<Vec<AppId>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (k, _) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            ids.push(AppId(k.value().to_string()));
        }
        Ok(ids)
    }
}
