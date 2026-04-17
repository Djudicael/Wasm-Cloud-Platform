use anyhow::Result;
use billing::{verify_chain, ChainError};
use std::path::Path;

pub async fn report(store_path: &str, tenant_id: &str, start_ms: u64, end_ms: u64) -> Result<()> {
    let store = storage::Store::open(Path::new(store_path))?;
    let records = store.read_billing_records_for_tenant(tenant_id, start_ms, end_ms)?;

    let report = billing::report::generate_report(&records, tenant_id, start_ms, end_ms);

    println!("Tenant Billing Report");
    println!("====================");
    println!("Tenant: {}", report.tenant_id);
    println!(
        "Period: {} - {}",
        report.period_start_ms, report.period_end_ms
    );
    println!();
    println!("Total Requests: {}", report.total_requests);
    println!("Total Fuel Consumed: {}", report.total_fuel_consumed);
    println!("Total Wall Clock: {} ms", report.total_wall_clock_ms);
    println!("Peak RAM: {} bytes", report.peak_ram_bytes);
    println!("Trap Count: {}", report.trap_count);
    println!();
    println!("Per-App Breakdown:");
    println!("-----------------");
    for app in &report.per_app {
        println!(
            "  {}: {} requests, {} fuel (avg {} fuel/req), {} ms wall, {} traps",
            app.app_id,
            app.request_count,
            app.fuel_consumed,
            app.avg_fuel_per_request,
            app.wall_clock_ms,
            app.trap_count
        );
    }

    Ok(())
}

pub async fn verify(store_path: &str) -> Result<()> {
    let store = storage::Store::open(Path::new(store_path))?;
    let records = store.read_all_billing_records()?;

    match verify_chain(&records) {
        Ok(count) => {
            println!("Verified {} billing records — chain is consistent ✓", count);
        }
        Err(e) => match e {
            ChainError::BrokenLink {
                seq,
                expected,
                actual,
            } => {
                anyhow::bail!(
                    "Chain broken at record {}: expected prev_hash={}, found {}",
                    seq,
                    expected,
                    actual
                );
            }
            ChainError::TamperedRecord {
                seq,
                expected,
                actual,
            } => {
                anyhow::bail!(
                    "Tampering detected at record {}: hash mismatch (expected={}, found={})",
                    seq,
                    expected,
                    actual
                );
            }
        },
    }

    Ok(())
}

pub async fn records(
    store_path: &str,
    app_id: Option<&str>,
    tenant_id: Option<&str>,
    last: Option<usize>,
) -> Result<()> {
    let store = storage::Store::open(Path::new(store_path))?;

    let records = if let Some(app) = app_id {
        store.read_billing_records_for_app(app)?
    } else if let Some(tenant) = tenant_id {
        store.read_billing_records_for_tenant(tenant, 0, u64::MAX)?
    } else {
        store.read_all_billing_records()?
    };

    let count = records.len();
    let to_show = last.map_or(count, |n| n.min(count));
    let start_idx = count.saturating_sub(to_show);

    println!("Billing Records (showing {} of {} total)", to_show, count);
    println!("=========================================");

    for record in records.iter().skip(start_idx) {
        println!(
            "[{}] {} -> {}: fuel={}, status={}, trap={}",
            record.seq,
            record.tenant_id,
            record.app_id,
            record.fuel_consumed,
            record.status_code,
            record.is_trap
        );
    }

    Ok(())
}

pub async fn export(store_path: &str, output_path: &str) -> Result<()> {
    let store = storage::Store::open(Path::new(store_path))?;
    let records = store.read_unexported_billing_records(10_000)?;

    if records.is_empty() {
        println!("No unexported billing records found.");
        return Ok(());
    }

    let mut body = String::new();
    for record in &records {
        let line = serde_json::to_string(record)?;
        body.push_str(&line);
        body.push('\n');
    }

    std::fs::write(output_path, body.as_bytes())?;

    let last_seq = records.last().map(|r| r.seq).unwrap_or(0);
    store.set_billing_export_watermark(last_seq)?;

    println!(
        "Exported {} billing records to {} (watermark: {})",
        records.len(),
        output_path,
        last_seq
    );

    Ok(())
}
