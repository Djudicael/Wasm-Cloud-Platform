use crate::{tables::ROUTES, Store};
use common::{error::PlatformError, types::Route};
use redb::{ReadableDatabase, ReadableTable};

impl Store {
    pub fn save_route(&self, route: &Route) -> Result<(), PlatformError> {
        let json =
            serde_json::to_string(route).map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ROUTES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(route.host.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn delete_route(&self, host: &str) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(ROUTES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .remove(host)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn list_routes(&self) -> Result<Vec<Route>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(ROUTES)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut routes = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (_, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let route: Route = serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            routes.push(route);
        }
        Ok(routes)
    }

    pub fn load_route(&self, host: &str) -> Result<Option<Route>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(ROUTES)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table
            .get(host)
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    pub fn save_route_if_newer(&self, route: &Route) -> Result<bool, PlatformError> {
        if let Some(existing) = self.load_route(&route.host)? {
            if existing.updated_at >= route.updated_at {
                tracing::warn!(
                    host = %route.host,
                    "route update ignored (existing is newer)"
                );
                return Ok(false);
            }
        }
        self.save_route(route)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::AppId;
    use tempfile::NamedTempFile;

    #[test]
    fn test_route_storage_lifecycle() {
        let temp_file = NamedTempFile::new().unwrap();
        let store = Store::open(temp_file.path()).unwrap();

        let route = Route {
            host: "api.myapp.com".to_string(),
            app_id: AppId("myapp".to_string()),
            path_prefix: "/".to_string(),
            strip_prefix: false,
            created_at: 100,
            updated_at: 100,
        };

        // 1. save and list
        store.save_route(&route).unwrap();
        let routes = store.list_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].host, "api.myapp.com");

        // 2. load specific
        let loaded = store.load_route("api.myapp.com").unwrap().unwrap();
        assert_eq!(loaded.app_id.0, "myapp");

        // 3. save if newer (older timestamp)
        let older_route = Route {
            updated_at: 50,
            ..route.clone()
        };
        let updated = store.save_route_if_newer(&older_route).unwrap();
        assert!(!updated);

        // 4. save if newer (newer timestamp)
        let newer_route = Route {
            updated_at: 200,
            ..route.clone()
        };
        let updated = store.save_route_if_newer(&newer_route).unwrap();
        assert!(updated);

        let loaded_new = store.load_route("api.myapp.com").unwrap().unwrap();
        assert_eq!(loaded_new.updated_at, 200);

        // 5. delete
        store.delete_route("api.myapp.com").unwrap();
        let loaded_deleted = store.load_route("api.myapp.com").unwrap();
        assert!(loaded_deleted.is_none());
        assert!(store.list_routes().unwrap().is_empty());
    }
}
