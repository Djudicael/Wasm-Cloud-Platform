#[cfg(test)]
mod test_helpers {
    use crate::{collector::MetricsCollector, exporter::Metrics, ExecutionSample};
    use std::sync::Arc;
    use storage::Store;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_prometheus_exporter_format() {
        let metrics = Arc::new(Metrics::new());

        let sample = ExecutionSample {
            app_id: "test-app:v1".to_string(),
            instance_id: "inst-123".to_string(),
            timestamp_ms: 1600000000000,
            fuel_consumed: 1500,
            fuel_limit: 5000000,
            ram_bytes: 2048,
            wall_clock_ms: 45,
            status_code: 200,
            is_trap: false,
            trap_reason: None,
            trace_id: None,
        };

        // Record a successful execution
        metrics.record_execution(&sample);

        // Record a trap execution
        let trap_sample = ExecutionSample {
            app_id: "test-app:v1".to_string(),
            instance_id: "inst-123".to_string(),
            timestamp_ms: 1600000005000,
            fuel_consumed: 5000000,
            fuel_limit: 5000000,
            ram_bytes: 4096,
            wall_clock_ms: 120,
            status_code: 500,
            is_trap: true,
            trap_reason: Some("out_of_fuel".to_string()),
            trace_id: None,
        };
        metrics.record_execution(&trap_sample);

        // Simulate active instances gauge
        metrics
            .active_instances
            .with_label_values(&["test-app:v1"])
            .inc();

        // Gather and encode metrics
        use prometheus::Encoder;
        let mut buf = Vec::new();
        let encoder = prometheus::TextEncoder::new();
        encoder
            .encode(&metrics.registry.gather(), &mut buf)
            .unwrap();
        let output = String::from_utf8(buf).unwrap();

        // Assert expected Prometheus text format output
        assert!(output.contains("wasm_requests_total"));
        assert!(output.contains("wasm_fuel_consumed_total"));
        assert!(output.contains("wasm_ram_usage_bytes"));
        assert!(output.contains("wasm_request_duration_seconds"));
        assert!(output.contains("wasm_active_instances"));
        assert!(output.contains("wasm_trap_total"));
        #[cfg(target_os = "linux")]
        {
            assert!(output.contains("process_open_fds"));
            assert!(output.contains("process_max_fds"));
        }

        // Assert labels
        assert!(output.contains("app=\"test-app:v1\""));
        assert!(output.contains("status=\"200\""));
        assert!(output.contains("status=\"500\""));
        assert!(output.contains("reason=\"out_of_fuel\""));
    }

    #[tokio::test]
    async fn test_collector_non_blocking_drop() {
        let f = NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();

        let collector = MetricsCollector::start(store);

        // Send 10,050 samples (capacity is 10,000)
        // It must not block or panic when the channel is full.
        for i in 0..10050 {
            collector.record(ExecutionSample {
                app_id: "stress-app".to_string(),
                instance_id: format!("inst-{i}"),
                timestamp_ms: 1600000000000,
                fuel_consumed: 500,
                fuel_limit: 1000,
                ram_bytes: 1024,
                wall_clock_ms: 50,
                status_code: 200,
                is_trap: false,
                trap_reason: None,
                trace_id: None,
            });
        }

        // If we reach this point, the collector successfully dropped excess samples
        // without blocking the thread.
    }
}
