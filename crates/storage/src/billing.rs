use crate::tables::BILLING;
use crate::Store;
use common::billing::BillingRecord;
use common::error::PlatformError;
use redb::ReadableTable;

impl Store {
    pub fn write_billing_record(&self, record: &BillingRecord) -> Result<(), PlatformError> {
        let key = format!("{}:{}", record.node_id, record.seq);
        let json =
            serde_json::to_string(record).map_err(|e| PlatformError::Storage(e.to_string()))?;

        let tx = self
            .db
            .begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(BILLING)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert(key.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn get_billing_sequence(&self) -> Result<u64, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(BILLING)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut max_seq = 0u64;
        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (k, _) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let key_str = k.value();
            if let Some(seq_str) = key_str.strip_prefix("node-") {
                if let Some(seq) = seq_str
                    .split(':')
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                {
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
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(BILLING)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut last_record: Option<BillingRecord> = None;
        let mut max_seq = 0u64;

        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (_, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let record: BillingRecord = serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;

            if record.seq > max_seq {
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
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(BILLING)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut records = Vec::new();
        let prefix = format!("{}:", node_id);

        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (k, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let key_str = k.value();
            if key_str.starts_with(&prefix) {
                let record: BillingRecord = serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                records.push(record);
            }
        }

        Ok(records)
    }

    pub fn get_all_billing_records(&self) -> Result<Vec<BillingRecord>, PlatformError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(BILLING)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut records = Vec::new();

        for entry in table
            .iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            let (_, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let record: BillingRecord = serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
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
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx
            .open_table(crate::tables::SCHEMA_META)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

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
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx
                .open_table(crate::tables::SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table
                .insert("billing_export_watermark", seq.to_string().as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))
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
