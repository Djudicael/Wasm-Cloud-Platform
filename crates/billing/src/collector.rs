use crate::report::generate_report;
use crate::verify_chain;
use common::billing::BillingRecord;
use storage::Store;
use tokio::sync::mpsc;
use tracing::{error, warn};

const CHANNEL_CAPACITY: usize = 50_000;

pub struct BillingCollector {
    tx: mpsc::Sender<BillingInput>,
}

pub struct BillingInput {
    pub tenant_id: String,
    pub app_id: String,
    pub instance_id: String,
    pub node_id: String,
    pub fuel_consumed: u64,
    pub fuel_quota: u64,
    pub ram_bytes: u64,
    pub wall_clock_ms: u64,
    pub status_code: u16,
    pub is_trap: bool,
}

impl BillingCollector {
    pub fn start(store: Store, node_id: String) -> Self {
        let (tx, rx) = mpsc::channel::<BillingInput>(CHANNEL_CAPACITY);
        tokio::spawn(billing_writer_loop(rx, store, node_id));
        BillingCollector { tx }
    }

    pub fn tx(&self) -> mpsc::Sender<BillingInput> {
        self.tx.clone()
    }

    pub fn record(&self, input: BillingInput) {
        if self.tx.try_send(input).is_err() {
            warn!("billing channel full, dropping record — this should be investigated");
        }
    }
}

async fn billing_writer_loop(mut rx: mpsc::Receiver<BillingInput>, store: Store, node_id: String) {
    let mut seq = store.get_billing_sequence().unwrap_or(0);
    let mut prev_hash = store.get_last_billing_hash().unwrap_or_default();

    while let Some(input) = rx.recv().await {
        seq += 1;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut record = BillingRecord {
            seq,
            prev_hash: prev_hash.clone(),
            tenant_id: input.tenant_id,
            app_id: input.app_id,
            instance_id: input.instance_id,
            node_id: node_id.clone(),
            timestamp_ms,
            fuel_consumed: input.fuel_consumed,
            fuel_quota: input.fuel_quota,
            ram_bytes: input.ram_bytes,
            wall_clock_ms: input.wall_clock_ms,
            status_code: input.status_code,
            is_trap: input.is_trap,
            record_hash: String::new(),
        };

        record.record_hash = record.compute_hash();
        prev_hash = record.record_hash.clone();

        if let Err(e) = store.write_billing_record(&record) {
            error!(seq, error = %e, "failed to write billing record");
        }
    }
}

pub fn verify_node_billing_chain(store: &Store, node_id: &str) -> Result<u64, String> {
    let records = store
        .get_billing_records_for_node(node_id)
        .map_err(|e| e.to_string())?;

    if records.is_empty() {
        return Ok(0);
    }

    let mut sorted_records = records.clone();
    sorted_records.sort_by_key(|r| r.seq);

    verify_chain(&sorted_records).map_err(|e| format!("{:?}", e))
}

pub fn generate_tenant_billing_report(
    store: &Store,
    tenant_id: &str,
    start_ms: u64,
    end_ms: u64,
) -> Result<crate::report::TenantBillingReport, String> {
    let all_records = store.get_all_billing_records().map_err(|e| e.to_string())?;

    let tenant_records: Vec<&BillingRecord> = all_records
        .iter()
        .filter(|r| {
            r.tenant_id == tenant_id && r.timestamp_ms >= start_ms && r.timestamp_ms < end_ms
        })
        .collect();

    Ok(generate_report(
        &tenant_records
            .iter()
            .map(|r| (*r).clone())
            .collect::<Vec<_>>(),
        tenant_id,
        start_ms,
        end_ms,
    ))
}

pub fn get_tenant_list(store: &Store) -> Result<Vec<String>, String> {
    let records = store.get_all_billing_records().map_err(|e| e.to_string())?;

    let mut tenants: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in records {
        tenants.insert(record.tenant_id);
    }

    Ok(tenants.into_iter().collect())
}
