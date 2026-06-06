use crate::{tables, Store, CURRENT_SCHEMA_VERSION};
use redb::ReadableDatabase;

impl Store {
    pub(crate) fn read_schema_version(&self) -> Result<u32, redb::Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(tables::SCHEMA_META)?;
        Ok(table
            .get("version")?
            .map(|v| v.value().parse::<u32>().unwrap_or(0))
            .unwrap_or(0))
    }

    pub(crate) fn write_schema_version(&self, version: u32) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(tables::SCHEMA_META)?;
            table.insert("version", version.to_string().as_str())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn run_migrations(&self) -> Result<(), redb::Error> {
        let current = self.read_schema_version()?;

        tracing::info!(
            current = current,
            target = CURRENT_SCHEMA_VERSION,
            "Checking schema version"
        );

        if current == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }
        if current > CURRENT_SCHEMA_VERSION {
            return Err(redb::Error::Corrupted(format!(
                "Database schema version {current} is NEWER than the binary supports ({CURRENT_SCHEMA_VERSION}). \
                 Downgrade is not supported. Use a newer binary or restore from backup."
            )));
        }

        for v in current..CURRENT_SCHEMA_VERSION {
            let target = v + 1;
            tracing::info!(from = v, to = target, "Running migration");
            self.apply_migration_transactional(target)?;
            tracing::info!(version = target, "Migration complete");
        }

        Ok(())
    }

    fn apply_migration_transactional(&self, target_version: u32) -> Result<(), redb::Error> {
        match target_version {
            1 => {
                tracing::info!("schema v1: initial schema created");
                self.write_schema_version(1)?;
            }
            2 => self.migrate_v1_to_v2()?,
            3 => self.migrate_v2_to_v3()?,
            4 => self.migrate_v3_to_v4()?,
            5 => self.migrate_v4_to_v5()?,
            6 => self.migrate_v5_to_v6()?,
            7 => self.migrate_v6_to_v7()?,
            8 => self.migrate_v7_to_v8()?,
            n => {
                return Err(redb::Error::Corrupted(format!(
                    "Unknown migration target: {n}"
                )))
            }
        }
        Ok(())
    }

    fn migrate_v1_to_v2(&self) -> Result<(), redb::Error> {
        use crate::tables::CONFIGS;
        use redb::ReadableTable;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONFIGS)?;
        let records: Vec<(String, String)> = table
            .iter()?
            .filter_map(|e: Result<_, _>| match e {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::warn!(error = %err, "error iterating configs table entry during v1->v2 migration, skipping");
                    None
                }
            })
            .map(|(k, v)| (k.value().to_string(), v.value().to_string()))
            .collect();
        drop(table);
        drop(tx);

        let write_tx = self.db.begin_write()?;
        {
            let mut table = write_tx.open_table(CONFIGS)?;
            for (key, json_str) in records {
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    if val.get("db_max_connections").is_none() {
                        val["db_max_connections"] = serde_json::json!(10);
                    }
                    let new_json = serde_json::to_string(&val)
                        .map_err(|e| redb::Error::Corrupted(format!("re-serialize failed: {e}")))?;
                    table.insert(key.as_str(), new_json.as_str())?;
                }
            }
            drop(table);
            let mut meta_table = write_tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "2")?;
        }
        write_tx.commit()?;
        tracing::info!("v1->v2: added db_max_connections to all app configs");
        Ok(())
    }

    fn migrate_v2_to_v3(&self) -> Result<(), redb::Error> {
        use crate::tables::CONFIGS;
        use redb::ReadableTable;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(CONFIGS)?;
        let records: Vec<(String, String)> = table
            .iter()?
            .filter_map(|e: Result<_, _>| match e {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::warn!(error = %err, "error iterating configs table entry during v2->v3 migration, skipping");
                    None
                }
            })
            .map(|(k, v)| (k.value().to_string(), v.value().to_string()))
            .collect();
        drop(table);
        drop(tx);

        let write_tx = self.db.begin_write()?;
        {
            let mut table = write_tx.open_table(CONFIGS)?;
            for (key, json_str) in records {
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    if val.get("rate_limit").is_none() {
                        val["rate_limit"] = serde_json::json!({
                            "requests_per_second": 1000,
                            "burst_capacity": 50,
                            "per_ip_limit": 100
                        });
                    }
                    let new_json = serde_json::to_string(&val)
                        .map_err(|e| redb::Error::Corrupted(format!("re-serialize failed: {e}")))?;
                    table.insert(key.as_str(), new_json.as_str())?;
                }
            }
            drop(table);
            let mut meta_table = write_tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "3")?;
        }
        write_tx.commit()?;
        tracing::info!("v2->v3: added rate_limit to all app configs");
        Ok(())
    }

    fn migrate_v3_to_v4(&self) -> Result<(), redb::Error> {
        use crate::tables::{BILLING, KEK};
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(BILLING)?;
            let _ = tx.open_table(KEK)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "4")?;
        }
        tx.commit()?;
        tracing::info!("v3->v4: ensured BILLING and KEK tables exist");
        Ok(())
    }

    fn migrate_v4_to_v5(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::GATEWAY_CONFIGS)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "5")?;
        }
        tx.commit()?;
        tracing::info!("v4->v5: ensured GATEWAY_CONFIGS table exists");
        Ok(())
    }

    fn migrate_v5_to_v6(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::API_KEYS)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "6")?;
        }
        tx.commit()?;
        tracing::info!("v5->v6: ensured API_KEYS table exists");
        Ok(())
    }

    fn migrate_v6_to_v7(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::CLUSTER_NODES)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "7")?;
        }
        tx.commit()?;
        tracing::info!("v6->v7: ensured CLUSTER_NODES table exists");
        Ok(())
    }

    fn migrate_v7_to_v8(&self) -> Result<(), redb::Error> {
        let tx = self.db.begin_write()?;
        {
            let _ = tx.open_table(tables::ARTIFACT_VERIFICATIONS)?;
            let mut meta_table = tx.open_table(tables::SCHEMA_META)?;
            meta_table.insert("version", "8")?;
        }
        tx.commit()?;
        tracing::info!("v7->v8: ensured ARTIFACT_VERIFICATIONS table exists");
        Ok(())
    }
}
