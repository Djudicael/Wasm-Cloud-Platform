pub mod collector;
pub mod export;
pub mod report;

pub use collector::BillingCollector;
pub use collector::BillingInput;
pub use common::billing::BillingRecord;
pub use export::{start_export_loop, BillingExporter, FileExporter, S3Exporter};
pub use report::generate_report;

#[derive(Debug)]
pub enum ChainError {
    BrokenLink {
        seq: u64,
        expected: String,
        actual: String,
    },
    TamperedRecord {
        seq: u64,
        expected: String,
        actual: String,
    },
}

pub fn verify_chain(records: &[BillingRecord]) -> Result<u64, ChainError> {
    let mut expected_prev = String::new();

    for record in records.iter() {
        if record.prev_hash != expected_prev {
            return Err(ChainError::BrokenLink {
                seq: record.seq,
                expected: expected_prev,
                actual: record.prev_hash.clone(),
            });
        }

        let computed = record.compute_hash();
        if computed != record.record_hash {
            return Err(ChainError::TamperedRecord {
                seq: record.seq,
                expected: computed,
                actual: record.record_hash.clone(),
            });
        }

        expected_prev = record.record_hash.clone();
    }

    Ok(records.len() as u64)
}

#[cfg(test)]
mod tests {
    use common::billing::BillingRecord;

    use super::{verify_chain, ChainError};

    fn create_test_record(seq: u64, prev_hash: &str, fuel: u64) -> BillingRecord {
        let record = BillingRecord {
            seq,
            prev_hash: prev_hash.to_string(),
            tenant_id: "test-tenant".to_string(),
            app_id: "test-app:v1".to_string(),
            instance_id: "instance-1".to_string(),
            node_id: "node-0".to_string(),
            timestamp_ms: 1712400000000,
            fuel_consumed: fuel,
            fuel_quota: 100_000_000,
            ram_bytes: 1024,
            wall_clock_ms: 10,
            status_code: 200,
            is_trap: false,
            record_hash: String::new(),
        };
        let hash = record.compute_hash();
        BillingRecord {
            record_hash: hash,
            ..record
        }
    }

    #[test]
    fn test_hash_chain_verification() {
        let mut records = Vec::new();
        let mut prev = String::new();

        for i in 1..=100 {
            let record = create_test_record(i, &prev, 1000 * i);
            prev = record.record_hash.clone();
            records.push(record);
        }

        assert_eq!(verify_chain(&records).unwrap(), 100);
    }

    #[test]
    fn test_tampered_record_detection() {
        let mut records = Vec::new();
        let mut prev = String::new();

        for i in 1..=10 {
            let record = create_test_record(i, &prev, 1000);
            prev = record.record_hash.clone();
            records.push(record);
        }

        let mut tampered = records[5].clone();
        tampered.fuel_consumed = 999999;
        records[5] = tampered;

        let result = verify_chain(&records);
        assert!(matches!(result, Err(ChainError::TamperedRecord { .. })));
    }

    #[test]
    fn test_broken_link_detection() {
        let mut records = Vec::new();
        let mut prev = String::new();

        for i in 1..=10 {
            let record = create_test_record(i, &prev, 1000);
            prev = record.record_hash.clone();
            records.push(record);
        }

        records.remove(5);
        let result = verify_chain(&records);
        assert!(matches!(result, Err(ChainError::BrokenLink { .. })));
    }
}
