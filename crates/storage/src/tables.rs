use redb::TableDefinition;

// ── ARTIFACT STORE ────────────────────────────────────────────────────────────
// Key   : app_id as &str  (e.g. "api-users:v1")
// Value : raw bytes of the AOT-compiled Wasmer artifact (can be several MB)
pub const ARTIFACTS: TableDefinition<&str, &[u8]> = TableDefinition::new("artifacts");

// ── APP CONFIG ────────────────────────────────────────────────────────────────
// Key   : app_id
// Value : JSON-serialized AppConfig struct
pub const CONFIGS: TableDefinition<&str, &str> = TableDefinition::new("configs");

// ── ENCRYPTED SECRETS ─────────────────────────────────────────────────────────
// Key   : app_id
// Value : EncryptedBlob struct serialized with bincode
pub const SECRETS: TableDefinition<&str, &[u8]> = TableDefinition::new("secrets");

// ── TELEMETRY (aggregated, 1-minute buckets) ──────────────────────────────────
// Key   : composite key "<app_id>:<timestamp_minute>" (e.g. "api-users:1735000000")
// Value : JSON-serialized MetricBucket
pub const METRICS: TableDefinition<&str, &str> = TableDefinition::new("metrics");
