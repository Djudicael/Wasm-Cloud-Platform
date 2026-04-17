use redb::TableDefinition;

// ── ARTIFACT STORE ────────────────────────────────────────────────────────────
// Key   : app_id as &str  (e.g. "api-users:v1")
// Value : raw bytes of the AOT-compiled Wasmtime artifact (can be several MB)
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

// ── ROUTES ────────────────────────────────────────────────────────────────────
// Key   : host string (e.g. "api.myapp.com")
// Value : JSON-serialized Route
pub const ROUTES: TableDefinition<&str, &str> = TableDefinition::new("routes");

// ── RAW WASM ──────────────────────────────────────────────────────────────────
// Key   : app_id
// Value : raw uncompiled Wasm bytes
pub const RAW_WASM: TableDefinition<&str, &[u8]> = TableDefinition::new("raw_wasm");

// ── SCHEMA METADATA ───────────────────────────────────────────────────────────
// Key   : metadata key
// Value : metadata value
pub const SCHEMA_META: TableDefinition<&str, &str> = TableDefinition::new("schema_meta");

// ── ARTIFACT METADATA ─────────────────────────────────────────────────────────
// Key   : app_id
// Value : SHA-256 hex string of the raw .wasm bytes
pub const ARTIFACT_HASHES: TableDefinition<&str, &str> = TableDefinition::new("artifact_hashes");

// ── BILLING RECORDS ────────────────────────────────────────────────────────────
// Key   : "node_id:sequence_number" (e.g. "node-0:12345")
// Value : JSON-serialized BillingRecord
pub const BILLING: TableDefinition<&str, &str> = TableDefinition::new("billing");
