use crate::{tables, Store};
use common::auth::AuthConfig;
use common::error::PlatformError;
use common::types::{ApiKeyRecord, ClusterNodeRecord, GatewayRouteConfig};
use redb::ReadableDatabase;

impl Store {
    pub fn save_meta(&self, key: &str, value: &str) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.insert(key, value)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_meta(&self, key: &str) -> Result<Option<String>, redb::Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(tables::SCHEMA_META)?;
        Ok(table.get(key)?.map(|v| v.value().to_string()))
    }

    pub fn delete_meta(&self, key: &str) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.remove(key)?;
        }
        tx.commit()?;
        Ok(())
    }

    const AUTH_CONFIG_KEY: &'static str = "auth_config_override";

    pub fn save_auth_config(&self, config: &AuthConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize auth config", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META).map_err(|e| {
                PlatformError::storage_with_msg("failed to open SCHEMA_META table", e)
            })?;
            table
                .insert(Self::AUTH_CONFIG_KEY, json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write auth config", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit auth config", e))?;
        Ok(())
    }

    pub fn load_auth_config(&self) -> Result<Option<AuthConfig>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx
            .open_table(tables::SCHEMA_META)
            .map_err(|e| PlatformError::storage_with_msg("failed to open SCHEMA_META table", e))?;
        match table
            .get(Self::AUTH_CONFIG_KEY)
            .map_err(|e| PlatformError::storage_with_msg("failed to read auth config", e))?
        {
            Some(v) => {
                let config: AuthConfig = serde_json::from_str(v.value()).map_err(|e| {
                    PlatformError::storage_with_msg(
                        "failed to deserialize persisted auth config - falling back to TOML file",
                        e,
                    )
                })?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    pub fn delete_auth_config(&self) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META).map_err(|e| {
                PlatformError::storage_with_msg("failed to open SCHEMA_META table", e)
            })?;
            table
                .remove(Self::AUTH_CONFIG_KEY)
                .map_err(|e| PlatformError::storage_with_msg("failed to delete auth config", e))?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit auth config deletion", e)
        })?;
        Ok(())
    }

    pub fn save_cluster_node(&self, node: &ClusterNodeRecord) -> Result<(), PlatformError> {
        let json = serde_json::to_string(node)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize cluster node", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
                PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
            })?;
            table
                .insert(node.node_id.as_str(), json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write cluster node", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit cluster node", e))?;
        Ok(())
    }

    pub fn load_cluster_node(
        &self,
        node_id: &str,
    ) -> Result<Option<ClusterNodeRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
            PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
        })?;
        match table
            .get(node_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read cluster node", e))?
        {
            Some(value) => {
                let node = serde_json::from_str(value.value()).map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize cluster node", e)
                })?;
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    pub fn list_cluster_nodes(&self) -> Result<Vec<ClusterNodeRecord>, PlatformError> {
        use redb::ReadableTable;
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::CLUSTER_NODES).map_err(|e| {
            PlatformError::storage_with_msg("failed to open CLUSTER_NODES table", e)
        })?;
        let mut nodes = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::storage_with_msg("failed to iterate cluster nodes", e))?
        {
            let (_key, value) = entry.map_err(|e| {
                PlatformError::storage_with_msg("failed to read cluster node entry", e)
            })?;
            let node: ClusterNodeRecord = serde_json::from_str(value.value()).map_err(|e| {
                PlatformError::storage_with_msg("failed to deserialize cluster node", e)
            })?;
            nodes.push(node);
        }
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(nodes)
    }

    pub fn save_gateway_config(
        &self,
        app_id: &str,
        config: &GatewayRouteConfig,
    ) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config).map_err(|e| {
            PlatformError::storage_with_msg("failed to serialize gateway config", e)
        })?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
                PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
            })?;
            table.insert(app_id, json.as_str()).map_err(|e| {
                PlatformError::storage_with_msg("failed to write gateway config", e)
            })?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit gateway config", e))?;
        Ok(())
    }

    pub fn load_gateway_config(
        &self,
        app_id: &str,
    ) -> Result<Option<GatewayRouteConfig>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
            PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
        })?;
        match table
            .get(app_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read gateway config", e))?
        {
            Some(v) => {
                let config: GatewayRouteConfig = serde_json::from_str(v.value()).map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize gateway config", e)
                })?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    pub fn delete_gateway_config(&self, app_id: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
                PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
            })?;
            table.remove(app_id).map_err(|e| {
                PlatformError::storage_with_msg("failed to delete gateway config", e)
            })?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit gateway config deletion", e)
        })?;
        Ok(())
    }

    pub fn list_gateway_configs(&self) -> Result<Vec<(String, GatewayRouteConfig)>, PlatformError> {
        use redb::ReadableTable;
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx.open_table(tables::GATEWAY_CONFIGS).map_err(|e| {
            PlatformError::storage_with_msg("failed to open GATEWAY_CONFIGS table", e)
        })?;
        let mut configs = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::storage_with_msg("failed to iterate gateway configs", e))?
        {
            let (k, v) = entry.map_err(|e| {
                PlatformError::storage_with_msg("failed to read gateway config entry", e)
            })?;
            let app_id = k.value().to_string();
            let config: GatewayRouteConfig = serde_json::from_str(v.value()).map_err(|e| {
                PlatformError::storage_with_msg("failed to deserialize gateway config", e)
            })?;
            configs.push((app_id, config));
        }
        Ok(configs)
    }

    pub fn save_api_keys(&self, app_id: &str, keys: &[ApiKeyRecord]) -> Result<(), PlatformError> {
        let json = serde_json::to_string(keys)
            .map_err(|e| PlatformError::storage_with_msg("failed to serialize api keys", e))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx
                .open_table(tables::API_KEYS)
                .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
            table
                .insert(app_id, json.as_str())
                .map_err(|e| PlatformError::storage_with_msg("failed to write api keys", e))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::storage_with_msg("failed to commit api keys", e))?;
        Ok(())
    }

    pub fn load_api_keys(&self, app_id: &str) -> Result<Vec<ApiKeyRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin read transaction", e))?;
        let table = tx
            .open_table(tables::API_KEYS)
            .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
        match table
            .get(app_id)
            .map_err(|e| PlatformError::storage_with_msg("failed to read api keys", e))?
        {
            Some(v) => {
                let keys: Vec<ApiKeyRecord> = serde_json::from_str(v.value()).map_err(|e| {
                    PlatformError::storage_with_msg("failed to deserialize api keys", e)
                })?;
                Ok(keys)
            }
            None => Ok(Vec::new()),
        }
    }

    pub fn delete_api_keys(&self, app_id: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::storage_with_msg("failed to begin write transaction", e))?;
        {
            let mut table = tx
                .open_table(tables::API_KEYS)
                .map_err(|e| PlatformError::storage_with_msg("failed to open API_KEYS table", e))?;
            table
                .remove(app_id)
                .map_err(|e| PlatformError::storage_with_msg("failed to delete api keys", e))?;
        }
        tx.commit().map_err(|e| {
            PlatformError::storage_with_msg("failed to commit api keys deletion", e)
        })?;
        Ok(())
    }
}
