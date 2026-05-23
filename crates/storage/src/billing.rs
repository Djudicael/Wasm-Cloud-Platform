use crate::tables::BILLING;
use crate::Store;
use common::billing::BillingRecord;
use common::error::PlatformError;
use redb::{ReadableDatabase, ReadableTable};

impl Store {
    fn parse_billing_key(key: &str) -> Option<(&str, u64)> {
        let (node_id, seq_str) = key.rsplit_once(':')?;
        let seq = seq_str.parse::<u64>().ok()?;
        Some((node_id, seq))
    }

    pub fn write_billing_record(&self, record: &BillingRecord) -> Result<(), PlatformError> {
        let key = format!("{}:{}", record.node_id, record.seq);
        let json = serde_json::to_string(record).map_err(PlatformError::storage_source)?;

        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(BILLING)
                .map_err(PlatformError::storage_source)?;
            table
                .insert(key.as_str(), json.as_str())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    pub fn get_billing_sequence(&self) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut max_seq = 0u64;
        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (k, _) = entry.map_err(PlatformError::storage_source)?;
            let key_str = k.value();
            if let Some((_, seq)) = Self::parse_billing_key(key_str) {
                max_seq = max_seq.max(seq);
            }
        }
        Ok(max_seq)
    }

    pub fn get_billing_sequence_for_node(&self, node_id: &str) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut max_seq = 0u64;
        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (k, _) = entry.map_err(PlatformError::storage_source)?;
            let key_str = k.value();
            if let Some((record_node_id, seq)) = Self::parse_billing_key(key_str) {
                if record_node_id == node_id {
                    max_seq = max_seq.max(seq);
                }
            }
        }

        Ok(max_seq)
    }

    pub fn get_last_billing_hash(&self) -> Result<String, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut last_record: Option<BillingRecord> = None;
        let mut max_seq = 0u64;

        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (_, v) = entry.map_err(PlatformError::storage_source)?;
            let record: BillingRecord =
                serde_json::from_str(v.value()).map_err(PlatformError::storage_source)?;

            if record.seq > max_seq {
                max_seq = record.seq;
                last_record = Some(record);
            }
        }

        Ok(last_record.map(|r| r.record_hash).unwrap_or_default())
    }

    pub fn get_last_billing_hash_for_node(&self, node_id: &str) -> Result<String, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut last_record: Option<BillingRecord> = None;
        let mut max_seq = 0u64;

        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (_, v) = entry.map_err(PlatformError::storage_source)?;
            let record: BillingRecord =
                serde_json::from_str(v.value()).map_err(PlatformError::storage_source)?;

            if record.node_id == node_id && record.seq > max_seq {
                max_seq = record.seq;
                last_record = Some(record);
            }
        }

        Ok(last_record.map(|r| r.record_hash).unwrap_or_default())
    }

    pub fn get_billing_records_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<BillingRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut records = Vec::new();
        let prefix = format!("{}:", node_id);

        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (k, v) = entry.map_err(PlatformError::storage_source)?;
            let key_str = k.value();
            if key_str.starts_with(&prefix) {
                let record: BillingRecord =
                    serde_json::from_str(v.value()).map_err(PlatformError::storage_source)?;
                records.push(record);
            }
        }

        Ok(records)
    }

    pub fn get_all_billing_records(&self) -> Result<Vec<BillingRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(BILLING)
            .map_err(PlatformError::storage_source)?;

        let mut records = Vec::new();

        for entry in table.iter().map_err(PlatformError::storage_source)? {
            let (_, v) = entry.map_err(PlatformError::storage_source)?;
            let record: BillingRecord =
                serde_json::from_str(v.value()).map_err(PlatformError::storage_source)?;
            records.push(record);
        }

        Ok(records)
    }

    pub fn read_unexported_billing_records(
        &self,
        limit: usize,
    ) -> Result<Vec<BillingRecord>, PlatformError> {
        let watermark = self.get_billing_export_watermark()?;
        let all_records = self.get_all_billing_records()?;

        let unexported: Vec<BillingRecord> = all_records
            .into_iter()
            .filter(|r| r.seq > watermark)
            .take(limit)
            .collect();

        Ok(unexported)
    }

    pub fn get_billing_export_watermark(&self) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(PlatformError::storage_source)?;
        let table = tx
            .open_table(crate::tables::SCHEMA_META)
            .map_err(PlatformError::storage_source)?;

        match table.get("billing_export_watermark") {
            Ok(Some(v)) => {
                let val = v.value();
                Ok(val.parse::<u64>().unwrap_or(0))
            }
            Ok(None) => Ok(0),
            Err(_) => Ok(0),
        }
    }

    pub fn set_billing_export_watermark(&self, seq: u64) -> Result<(), PlatformError> {
        let tx = self
            .db
            .begin_write()
            .map_err(PlatformError::storage_source)?;
        {
            let mut table = tx
                .open_table(crate::tables::SCHEMA_META)
                .map_err(PlatformError::storage_source)?;
            table
                .insert("billing_export_watermark", seq.to_string().as_str())
                .map_err(PlatformError::storage_source)?;
        }
        tx.commit().map_err(PlatformError::storage_source)
    }

    pub fn read_billing_records_for_tenant(
        &self,
        tenant_id: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<Vec<BillingRecord>, PlatformError> {
        let all_records = self.get_all_billing_records()?;
        let filtered: Vec<BillingRecord> = all_records
            .into_iter()
            .filter(|r| {
                r.tenant_id == tenant_id && r.timestamp_ms >= start_ms && r.timestamp_ms < end_ms
            })
            .collect();
        Ok(filtered)
    }

    pub fn read_billing_records_for_app(
        &self,
        app_id: &str,
    ) -> Result<Vec<BillingRecord>, PlatformError> {
        let all_records = self.get_all_billing_records()?;
        let filtered: Vec<BillingRecord> = all_records
            .into_iter()
            .filter(|r| r.app_id == app_id)
            .collect();
        Ok(filtered)
    }

    pub fn read_all_billing_records(&self) -> Result<Vec<BillingRecord>, PlatformError> {
        self.get_all_billing_records()
    }
}
