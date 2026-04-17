use common::billing::BillingRecord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantBillingReport {
    pub tenant_id: String,
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    pub total_requests: u64,
    pub total_fuel_consumed: u64,
    pub total_wall_clock_ms: u64,
    pub peak_ram_bytes: u64,
    pub trap_count: u64,
    pub per_app: Vec<AppUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUsage {
    pub app_id: String,
    pub request_count: u64,
    pub fuel_consumed: u64,
    pub avg_fuel_per_request: u64,
    pub wall_clock_ms: u64,
    pub trap_count: u64,
}

pub fn generate_report(
    records: &[BillingRecord],
    tenant_id: &str,
    start_ms: u64,
    end_ms: u64,
) -> TenantBillingReport {
    let tenant_records: Vec<&BillingRecord> = records
        .iter()
        .filter(|r| {
            r.tenant_id == tenant_id && r.timestamp_ms >= start_ms && r.timestamp_ms < end_ms
        })
        .collect();

    let mut per_app: std::collections::HashMap<String, AppUsage> = std::collections::HashMap::new();

    let mut total_fuel = 0u64;
    let mut total_wall = 0u64;
    let mut peak_ram = 0u64;
    let mut trap_count = 0u64;

    for r in &tenant_records {
        total_fuel += r.fuel_consumed;
        total_wall += r.wall_clock_ms;
        peak_ram = peak_ram.max(r.ram_bytes);
        if r.is_trap {
            trap_count += 1;
        }

        let app = per_app.entry(r.app_id.clone()).or_insert(AppUsage {
            app_id: r.app_id.clone(),
            request_count: 0,
            fuel_consumed: 0,
            avg_fuel_per_request: 0,
            wall_clock_ms: 0,
            trap_count: 0,
        });
        app.request_count += 1;
        app.fuel_consumed += r.fuel_consumed;
        app.wall_clock_ms += r.wall_clock_ms;
        if r.is_trap {
            app.trap_count += 1;
        }
    }

    let mut apps: Vec<AppUsage> = per_app.into_values().collect();
    for app in &mut apps {
        app.avg_fuel_per_request = if app.request_count > 0 {
            app.fuel_consumed / app.request_count
        } else {
            0
        };
    }
    apps.sort_by(|a, b| b.fuel_consumed.cmp(&a.fuel_consumed));

    TenantBillingReport {
        tenant_id: tenant_id.to_string(),
        period_start_ms: start_ms,
        period_end_ms: end_ms,
        total_requests: tenant_records.len() as u64,
        total_fuel_consumed: total_fuel,
        total_wall_clock_ms: total_wall,
        peak_ram_bytes: peak_ram,
        trap_count,
        per_app: apps,
    }
}

impl TenantBillingReport {
    pub fn format(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!("Tenant: {}\n", self.tenant_id));

        let start = chrono::DateTime::from_timestamp_millis(self.period_start_ms as i64)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| self.period_start_ms.to_string());
        let end = chrono::DateTime::from_timestamp_millis(self.period_end_ms as i64)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| self.period_end_ms.to_string());

        output.push_str(&format!("Period: {} to {}\n", start, end));
        output.push_str(&format!("Total requests: {}\n", self.total_requests));
        output.push_str(&format!(
            "Total fuel consumed: {}\n",
            self.total_fuel_consumed
        ));
        output.push_str(&format!(
            "Total wall clock: {} ms\n",
            self.total_wall_clock_ms
        ));
        output.push_str(&format!("Peak RAM: {} bytes\n", self.peak_ram_bytes));
        output.push_str(&format!("Trap count: {}\n", self.trap_count));
        output.push_str("\nPer-app breakdown:\n");

        for app in &self.per_app {
            output.push_str(&format!(
                "  {}  {:>10} req  {:>15} fuel  avg {:>10} fuel/req\n",
                app.app_id, app.request_count, app.fuel_consumed, app.avg_fuel_per_request
            ));
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use common::billing::BillingRecord;

    fn create_record(app_id: &str, fuel: u64, is_trap: bool) -> BillingRecord {
        BillingRecord {
            seq: 1,
            prev_hash: String::new(),
            tenant_id: "test-tenant".to_string(),
            app_id: app_id.to_string(),
            instance_id: "inst-1".to_string(),
            node_id: "node-0".to_string(),
            timestamp_ms: 1712400000000,
            fuel_consumed: fuel,
            fuel_quota: 100_000_000,
            ram_bytes: 1024,
            wall_clock_ms: 10,
            status_code: 200,
            is_trap,
            record_hash: "test".to_string(),
        }
    }

    #[test]
    fn test_report_generation() {
        let records = vec![
            create_record("app1:v1", 1000, false),
            create_record("app1:v1", 2000, false),
            create_record("app2:v1", 3000, true),
        ];

        let report = super::generate_report(&records, "test-tenant", 0, u64::MAX);

        assert_eq!(report.tenant_id, "test-tenant");
        assert_eq!(report.total_requests, 3);
        assert_eq!(report.total_fuel_consumed, 6000);
        assert_eq!(report.trap_count, 1);
        assert_eq!(report.per_app.len(), 2);
    }
}
