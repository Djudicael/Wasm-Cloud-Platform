use super::*;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[test]
fn test_logging_config_default() {
    let config = LoggingConfig::default();
    assert_eq!(config.format, LogFormat::Json);
    assert_eq!(config.default_level, "info");
    assert!(!config.sampling_enabled);
}

#[test]
fn test_log_rotation_config_default() {
    let config = LogRotationConfig::default();
    assert_eq!(config.max_file_size_bytes, 100 * 1024 * 1024);
    assert_eq!(config.max_files, 10);
    assert!(config.compress);
}

#[test]
fn test_sampling_layer_rates() {
    let layer = SamplingLayer::new(2, 10, 100);
    // WARN/ERROR are always enabled - counters should not affect them.
    assert!(layer.info_counter.load(Ordering::Relaxed) == 0);
    layer.set_rates(5, 20, 200);
    assert_eq!(layer.info_rate.load(Ordering::Relaxed), 5);
    assert_eq!(layer.debug_rate.load(Ordering::Relaxed), 20);
    assert_eq!(layer.trace_rate.load(Ordering::Relaxed), 200);
}

#[test]
fn test_node_log_record_serialize() {
    let record = NodeLogRecord {
        timestamp: "2026-04-05T12:00:00Z".to_string(),
        level: "INFO".to_string(),
        target: "test".to_string(),
        span: None,
        message: "hello".to_string(),
        node_id: "node-0".to_string(),
        app_id: Some("app:v1".to_string()),
        instance_id: None,
        trace_id: None,
        span_id: None,
        fields: serde_json::Map::new(),
        source_file: None,
        source_line: None,
    };
    let json = serde_json::to_string(&record).unwrap();
    assert!(json.contains("\"node_id\":\"node-0\""));
    assert!(json.contains("\"app_id\":\"app:v1\""));
}

#[test]
fn test_field_collector() {
    let mut collector = FieldCollector::default();
    collector.fields.insert(
        "app_id".to_string(),
        serde_json::Value::String("my-app".to_string()),
    );
    collector.fields.insert(
        "status".to_string(),
        serde_json::Value::Number(200u64.into()),
    );
    assert_eq!(
        collector.fields.get("app_id"),
        Some(&serde_json::Value::String("my-app".to_string()))
    );
    assert_eq!(
        collector.fields.get("status"),
        Some(&serde_json::Value::Number(200u64.into()))
    );
}

#[test]
fn test_build_log_writer_opens_file_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.log");

    let writer = build_log_writer(&LogOutput::File { path: path.clone() });
    assert!(writer.is_ok());
    assert!(path.exists());
}

#[test]
fn test_build_log_writer_reports_file_open_failure() {
    let path = PathBuf::from("/definitely-missing-parent-dir/child/app.log");

    let err = match build_log_writer(&LogOutput::File { path: path.clone() }) {
        Ok(_) => panic!("expected file-open failure"),
        Err(err) => err,
    };
    assert!(err.contains("failed to open log file"));
    assert!(err.contains(&path.display().to_string()));
}
