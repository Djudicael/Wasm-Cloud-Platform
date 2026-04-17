# Step 32 — Configuration Management & Hot-Reload

## Goal
Implement a unified configuration system for `wasm-node` that supports layered
configuration sources, runtime changes without restart, and cluster-wide
configuration propagation. The system must:
- Accept configuration from TOML files, environment variables, and CLI flags
- Define a clear merge priority: defaults < TOML file < environment variables < CLI flags
- Allow runtime changes to selected parameters (rate limits, log levels, thresholds)
  without restarting the node
- Propagate configuration changes to other nodes via NATS for cluster-wide consistency
- Validate configuration before applying (reject invalid values, never crash on bad config)
- Persist runtime overrides so they survive node restart
- Provide CLI commands for viewing and changing configuration
- Require no external configuration service — the node is self-sufficient

---

## Context & Rationale

### The Problem This Solves

Currently, all `wasm-node` configuration is via CLI flags (Step 14). This means:
- Every deployment requires a long command line or systemd unit with 20+ flags
- Changing a rate limit, log level, or memory threshold requires a full restart
- There is no way to view the effective configuration of a running node
- There is no way to propagate a configuration change across the cluster
- Environment-specific overrides (dev vs staging vs production) require different
  systemd unit files

A configuration file solves the deployment problem. Hot-reload solves the
operational problem. NATS propagation solves the cluster consistency problem.

### Why TOML (Not YAML, Not JSON, Not HOCON)

| Format   │ Comments │ Includes │ Rust Ecosystem │ Human Writable
|──────────┼──────────┼──────────┼────────────────┼───────────────
| TOML     │ Yes      │ No       │ `serde-toml`   │ Yes — minimal syntax
| YAML     │ Yes      │ No       │ `serde-yaml`   │ Error-prone (indentation, typing)
| JSON     │ No       │ No       │ `serde_json`   │ No — no comments, strict syntax
| HOCON    │ Yes      │ Yes      │ No mature crate │ Complex, JVM-originated

TOML is the Rust ecosystem's preferred configuration format. `cargo` uses it.
It supports comments (essential for operator documentation), has a simple syntax,
and `serde-toml` deserializes directly into the same structs that CLI parsing uses.

### Why Layered Configuration (Not Just a File)

A single configuration file cannot handle all environments:

```
base.toml          → All nodes share these defaults
production.toml    → Production-specific overrides (NATS URL, TLS paths)
node-0.override   → This specific node's overrides (NODE_ID, port range)
Environment vars   → Container orchestration injects these (Kubernetes, Nomad)
CLI flags          → One-off overrides for debugging (--log-level trace)
```

The merge priority ensures that more specific sources override more general ones.
An operator can deploy the same `base.toml` to all nodes and use environment
variables for per-node differences.

### Why Hot-Reload (Not Just Restart)

Restarting `wasm-node` means:
1. All Wasm instances are killed (even with graceful shutdown, this takes 30s)
2. The node is removed from the upstream pool for 30–60 seconds
3. Other nodes must absorb the traffic during the restart window
4. AOT compilation of all artifacts runs again on startup (~25s for 50 apps)

For a rate limit change from 1000 to 2000 req/s, this is unacceptable operational
overhead. The node should adjust the rate limit in <1 second without dropping
any traffic.

### What Can and Cannot Be Hot-Reloaded

```
Parameter                     │ Hot-Reloadable? │ Reason
──────────────────────────────┼─────────────────┼──────────────────────────────
Rate limits (per-app, per-IP) │ YES             │ In-memory token buckets
Memory pressure thresholds    │ YES             │ eBPF config map update
FD limits                     │ YES             │ eBPF config map update
Disk I/O latency threshold    │ YES             │ eBPF config map update
Log level (RUST_LOG equiv.)   │ YES             │ tracing-subscriber reload
TCP connection limits         │ YES             │ eBPF config map update
GC interval                   │ YES             │ Restart the timer
Billing export interval       │ YES             │ Restart the timer
──────────────────────────────┼─────────────────┼──────────────────────────────
NATS URL                      │ NO              │ Requires new connection
TLS cert/key paths            │ NO              │ Requires Pingora restart
Port range (start/end)        │ NO              │ Ports already allocated
Database path                 │ NO              │ redb already open
Node ID                       │ NO              │ Identity is established
Wasm runtime engine config    │ NO              │ Engine is shared, immutable
Proxy port                    │ NO              │ Pingora already bound
```

Hot-reloadable parameters are those backed by in-memory data structures or
periodic timers. Non-reloadable parameters require process restart because they
affect resources that are already allocated.

---

---

## 1. Configuration File Format

### Full TOML Schema

```toml
# /etc/wasm-node/config.toml
# Wasm Cloud Platform Node Configuration

[node]
# Unique node identifier within the cluster.
node_id = "node-0"

[storage]
# Path to the redb database file.
db_path = "/var/lib/wasm-node/state.redb"

[nats]
# NATS server URL.
url = "nats://127.0.0.1:4222"
# Optional credentials file for authenticated NATS connections.
# creds_file = "/etc/wasm-node/nats.creds"

[proxy]
# HTTP proxy port (North-South traffic).
http_port = 8080
# HTTPS proxy port (0 = disabled).
https_port = 8443
# TLS certificate path (required for HTTPS).
# tls_cert = "/etc/wasm-node/tls/server.crt"
# TLS private key path.
# tls_key = "/etc/wasm-node/tls/server.key"

[admin]
# Admin API port (metrics, health, config changes).
port = 9090
# Artifact server port (Wasm binary distribution).
artifact_port = 9091
# Bearer token for admin API authentication (required in production).
# auth_token = "secret-token-here"

[runtime]
# Port range for Wasm instance binding.
port_start = 10000
port_end = 19999
# Key source: "generate" (ephemeral), "file" (from key_file), "env:VAR_NAME"
key_source = "generate"
# key_file = "/etc/wasm-node/master.key"

[database]
# Default database URL injected into Wasm apps.
default_url = "postgres://127.0.0.1:5432"
# pgBouncer health check address.
pgbouncer_addr = "127.0.0.1:5432"
# Enable built-in TCP proxy (not PostgreSQL-aware).
enable_db_proxy = false
db_proxy_addr = "127.0.0.1:5433"
db_proxy_backend = "db.internal:5432"
db_proxy_max_connections = 20

[logging]
# Log level: trace, debug, info, warn, error
level = "info"
# Optional OpenTelemetry OTLP endpoint.
# otlp_endpoint = "http://collector:4317"

[billing]
# Directory for billing record exports (enables periodic export).
# export_dir = "/var/lib/wasm-node/billing"
# Export interval in seconds.
export_interval_secs = 3600

[gc]
# How many compiled artifact versions to retain per app.
artifact_keep_versions = 3
# How many days of metric buckets to retain.
metrics_retain_days = 7
# Grace period after undeploy before purging state (seconds).
undeploy_grace_secs = 3600
# GC loop interval (seconds).
gc_interval_secs = 600
# Disk usage warning threshold (0.0–1.0).
disk_warning_threshold = 0.80

[rate_limit]
# Default per-app requests per second.
default_requests_per_second = 1000
# Default per-app burst capacity.
default_burst_capacity = 200
# Default per-IP requests per second.
default_per_ip_limit = 200

[ebpf]
# Enable eBPF monitoring (requires Linux kernel >= 5.8).
enabled = true
# FD soft limit per Wasm instance (warning at 80%).
fd_soft_limit = 8192
# FD hard limit per Wasm instance (kill at 95%).
fd_hard_limit = 9728
# Memory pressure low threshold (pages).
mem_low_threshold_pages = 65536
# Memory pressure critical threshold (pages).
mem_critical_threshold_pages = 16384
# Disk I/O latency threshold for "slow" alert (nanoseconds).
disk_slow_threshold_ns = 50000000
# Maximum TCP connections per PID before alert.
tcp_conn_limit_per_pid = 10000
# Syscall rate limit per second for suspicious categories.
syscall_rate_limit = 100000
# Sampling period for periodic counters (seconds).
sampling_period_secs = 10

[dns]
# Platform domain for subdomain routing.
# platform_domain = "myplatform.com"
# Webhook URL for DNS automation.
# webhook_url = "https://dns-api.example.com/records"
# Webhook authentication token.
# webhook_token = "secret"

[health]
# Health check loop interval (seconds).
check_interval_secs = 5
# Instance idle timeout (seconds).
default_idle_timeout_secs = 300
# Maximum instances per app on this node.
default_max_instances = 10
# Default fuel quota per execution.
default_fuel_quota = 500000000
# Default memory limit in pages (1 page = 64 KiB).
default_memory_pages = 2048
```

### Minimal Configuration (Dev/Testing)

```toml
# dev.toml — minimal config for local development
[node]
node_id = "dev-node"

[nats]
url = "nats://127.0.0.1:4222"

[logging]
level = "debug"
```

All other fields use defaults defined in the Rust struct.

---

## 2. Configuration Structs

```rust
// crates/common/src/config.rs
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;

/// Top-level configuration for a wasm-node.
/// All fields have defaults — a completely empty TOML file is valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    #[serde(default)]
    pub node: NodeSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub nats: NatsSection,
    #[serde(default)]
    pub proxy: ProxySection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default)]
    pub runtime: RuntimeSection,
    #[serde(default)]
    pub database: DatabaseSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub billing: BillingSection,
    #[serde(default)]
    pub gc: GcSection,
    #[serde(default)]
    pub rate_limit: RateLimitSection,
    #[serde(default)]
    pub ebpf: EbpfSection,
    #[serde(default)]
    pub dns: DnsSection,
    #[serde(default)]
    pub health: HealthSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSection {
    #[serde(default = "default_node_id")]
    pub node_id: String,
}

fn default_node_id() -> String {
    std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
}

impl Default for NodeSection {
    fn default() -> Self {
        NodeSection {
            node_id: default_node_id(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
}

fn default_db_path() -> PathBuf {
    PathBuf::from("/tmp/wasm-node/state.redb")
}

impl Default for StorageSection {
    fn default() -> Self {
        StorageSection {
            db_path: default_db_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsSection {
    #[serde(default = "default_nats_url")]
    pub url: String,
    #[serde(default)]
    pub creds_file: Option<String>,
}

fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".to_string()
}

impl Default for NatsSection {
    fn default() -> Self {
        NatsSection {
            url: default_nats_url(),
            creds_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxySection {
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub tls_cert: Option<String>,
    #[serde(default)]
    pub tls_key: Option<String>,
}

fn default_http_port() -> u16 { 8080 }
fn default_https_port() -> u16 { 8443 }

impl Default for ProxySection {
    fn default() -> Self {
        ProxySection {
            http_port: default_http_port(),
            https_port: default_https_port(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSection {
    #[serde(default = "default_admin_port")]
    pub port: u16,
    #[serde(default = "default_artifact_port")]
    pub artifact_port: u16,
    #[serde(default)]
    pub auth_token: Option<String>,
}

fn default_admin_port() -> u16 { 9090 }
fn default_artifact_port() -> u16 { 9091 }

impl Default for AdminSection {
    fn default() -> Self {
        AdminSection {
            port: default_admin_port(),
            artifact_port: default_artifact_port(),
            auth_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSection {
    #[serde(default = "default_port_start")]
    pub port_start: u16,
    #[serde(default = "default_port_end")]
    pub port_end: u16,
    #[serde(default = "default_key_source")]
    pub key_source: String,
    #[serde(default)]
    pub key_file: Option<String>,
}

fn default_port_start() -> u16 { 10000 }
fn default_port_end() -> u16 { 19999 }
fn default_key_source() -> String { "generate".to_string() }

impl Default for RuntimeSection {
    fn default() -> Self {
        RuntimeSection {
            port_start: default_port_start(),
            port_end: default_port_end(),
            key_source: default_key_source(),
            key_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSection {
    #[serde(default = "default_db_url")]
    pub default_url: String,
    #[serde(default = "default_pgbouncer_addr")]
    pub pgbouncer_addr: String,
    #[serde(default)]
    pub enable_db_proxy: bool,
    #[serde(default = "default_db_proxy_addr")]
    pub db_proxy_addr: String,
    #[serde(default = "default_db_proxy_backend")]
    pub db_proxy_backend: String,
    #[serde(default = "default_db_proxy_max_conn")]
    pub db_proxy_max_connections: usize,
}

fn default_db_url() -> String { "postgres://127.0.0.1:5432".to_string() }
fn default_pgbouncer_addr() -> String { "127.0.0.1:5432".to_string() }
fn default_db_proxy_addr() -> String { "127.0.0.1:5433".to_string() }
fn default_db_proxy_backend() -> String { "db.internal:5432".to_string() }
fn default_db_proxy_max_conn() -> usize { 20 }

impl Default for DatabaseSection {
    fn default() -> Self {
        DatabaseSection {
            default_url: default_db_url(),
            pgbouncer_addr: default_pgbouncer_addr(),
            enable_db_proxy: false,
            db_proxy_addr: default_db_proxy_addr(),
            db_proxy_backend: default_db_proxy_backend(),
            db_proxy_max_connections: default_db_proxy_max_conn(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingSection {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

fn default_log_level() -> String { "info".to_string() }

impl Default for LoggingSection {
    fn default() -> Self {
        LoggingSection {
            level: default_log_level(),
            otlp_endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingSection {
    #[serde(default)]
    pub export_dir: Option<String>,
    #[serde(default = "default_billing_interval")]
    pub export_interval_secs: u64,
}

fn default_billing_interval() -> u64 { 3600 }

impl Default for BillingSection {
    fn default() -> Self {
        BillingSection {
            export_dir: None,
            export_interval_secs: default_billing_interval(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcSection {
    #[serde(default = "default_gc_keep_versions")]
    pub artifact_keep_versions: usize,
    #[serde(default = "default_gc_metrics_days")]
    pub metrics_retain_days: u32,
    #[serde(default = "default_gc_grace_secs")]
    pub undeploy_grace_secs: u64,
    #[serde(default = "default_gc_interval_secs")]
    pub gc_interval_secs: u64,
    #[serde(default = "default_gc_disk_threshold")]
    pub disk_warning_threshold: f64,
}

fn default_gc_keep_versions() -> usize { 3 }
fn default_gc_metrics_days() -> u32 { 7 }
fn default_gc_grace_secs() -> u64 { 3600 }
fn default_gc_interval_secs() -> u64 { 600 }
fn default_gc_disk_threshold() -> f64 { 0.80 }

impl Default for GcSection {
    fn default() -> Self {
        GcSection {
            artifact_keep_versions: default_gc_keep_versions(),
            metrics_retain_days: default_gc_metrics_days(),
            undeploy_grace_secs: default_gc_grace_secs(),
            gc_interval_secs: default_gc_interval_secs(),
            disk_warning_threshold: default_gc_disk_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSection {
    #[serde(default = "default_rps")]
    pub default_requests_per_second: u32,
    #[serde(default = "default_burst")]
    pub default_burst_capacity: u32,
    #[serde(default = "default_per_ip")]
    pub default_per_ip_limit: u32,
}

fn default_rps() -> u32 { 1000 }
fn default_burst() -> u32 { 200 }
fn default_per_ip() -> u32 { 200 }

impl Default for RateLimitSection {
    fn default() -> Self {
        RateLimitSection {
            default_requests_per_second: default_rps(),
            default_burst_capacity: default_burst(),
            default_per_ip_limit: default_per_ip(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EbpfSection {
    #[serde(default = "default_ebpf_enabled")]
    pub enabled: bool,
    #[serde(default = "default_fd_soft")]
    pub fd_soft_limit: u32,
    #[serde(default = "default_fd_hard")]
    pub fd_hard_limit: u32,
    #[serde(default = "default_mem_low")]
    pub mem_low_threshold_pages: u64,
    #[serde(default = "default_mem_critical")]
    pub mem_critical_threshold_pages: u64,
    #[serde(default = "default_disk_slow")]
    pub disk_slow_threshold_ns: u64,
    #[serde(default = "default_tcp_limit")]
    pub tcp_conn_limit_per_pid: u32,
    #[serde(default = "default_syscall_rate")]
    pub syscall_rate_limit: u64,
    #[serde(default = "default_sampling")]
    pub sampling_period_secs: u64,
}

fn default_ebpf_enabled() -> bool { true }
fn default_fd_soft() -> u32 { 8192 }
fn default_fd_hard() -> u32 { 9728 }
fn default_mem_low() -> u64 { 65536 }
fn default_mem_critical() -> u64 { 16384 }
fn default_disk_slow() -> u64 { 50_000_000 }
fn default_tcp_limit() -> u32 { 10000 }
fn default_syscall_rate() -> u64 { 100_000 }
fn default_sampling() -> u64 { 10 }

impl Default for EbpfSection {
    fn default() -> Self {
        EbpfSection {
            enabled: default_ebpf_enabled(),
            fd_soft_limit: default_fd_soft(),
            fd_hard_limit: default_fd_hard(),
            mem_low_threshold_pages: default_mem_low(),
            mem_critical_threshold_pages: default_mem_critical(),
            disk_slow_threshold_ns: default_disk_slow(),
            tcp_conn_limit_per_pid: default_tcp_limit(),
            syscall_rate_limit: default_syscall_rate(),
            sampling_period_secs: default_sampling(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsSection {
    #[serde(default)]
    pub platform_domain: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_token: Option<String>,
}

impl Default for DnsSection {
    fn default() -> Self {
        DnsSection {
            platform_domain: None,
            webhook_url: None,
            webhook_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSection {
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub default_idle_timeout_secs: u64,
    #[serde(default = "default_max_instances")]
    pub default_max_instances: u32,
    #[serde(default = "default_fuel_quota")]
    pub default_fuel_quota: u64,
    #[serde(default = "default_memory_pages")]
    pub default_memory_pages: u32,
}

fn default_check_interval() -> u64 { 5 }
fn default_idle_timeout() -> u64 { 300 }
fn default_max_instances() -> u32 { 10 }
fn default_fuel_quota() -> u64 { 500_000_000 }
fn default_memory_pages() -> u32 { 2048 }

impl Default for HealthSection {
    fn default() -> Self {
        HealthSection {
            check_interval_secs: default_check_interval(),
            default_idle_timeout_secs: default_idle_timeout(),
            default_max_instances: default_max_instances(),
            default_fuel_quota: default_fuel_quota(),
            default_memory_pages: default_memory_pages(),
        }
    }
}
```

---

## 3. Configuration Loader & Merge Priority

```rust
// crates/common/src/config_loader.rs
use crate::config::NodeConfig;
use crate::error::PlatformError;
use std::path::Path;

/// Load configuration with the merge priority:
///   1. Built-in defaults (struct Default impl)
///   2. TOML file (if provided)
///   3. Environment variables (WASM_NODE_ prefix)
///   4. CLI flags (highest priority)
pub fn load_config(
    config_path: Option<&Path>,
    cli_overrides: &CliOverrides,
) -> Result<NodeConfig, PlatformError> {
    // 1. Start with defaults
    let mut config = NodeConfig::default();

    // 2. Layer TOML file on top
    if let Some(path) = config_path {
        if path.exists() {
            let toml_str = std::fs::read_to_string(path).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to read config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            let file_config: NodeConfig = toml::from_str(&toml_str).map_err(|e| {
                PlatformError::ConfigValidation(format!(
                    "failed to parse config file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            config = merge_config(config, file_config);
            tracing::info!(path = %path.display(), "configuration file loaded");
        } else {
            tracing::warn!(path = %path.display(), "config file not found, using defaults");
        }
    }

    // 3. Layer environment variables on top
    config = apply_env_overrides(config);

    // 4. Layer CLI overrides on top (highest priority)
    config = apply_cli_overrides(config, cli_overrides);

    // 5. Validate the final configuration
    validate_config(&config)?;

    Ok(config)
}

/// CLI overrides extracted from clap args.
/// Only non-default values are set — None means "use the lower-priority value".
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub node_id: Option<String>,
    pub db_path: Option<String>,
    pub nats_url: Option<String>,
    pub nats_creds: Option<String>,
    pub http_port: Option<u16>,
    pub https_port: Option<u16>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub admin_port: Option<u16>,
    pub artifact_port: Option<u16>,
    pub port_start: Option<u16>,
    pub port_end: Option<u16>,
    pub key_source: Option<String>,
    pub key_file: Option<String>,
    pub database_url: Option<String>,
    pub pgbouncer_addr: Option<String>,
    pub enable_db_proxy: Option<bool>,
    pub db_proxy_addr: Option<String>,
    pub db_proxy_backend: Option<String>,
    pub db_proxy_max_connections: Option<usize>,
    pub log_level: Option<String>,
    pub otlp_endpoint: Option<String>,
    pub billing_export_dir: Option<String>,
    pub billing_export_interval_secs: Option<u64>,
    pub platform_domain: Option<String>,
    pub dns_webhook_url: Option<String>,
    pub dns_webhook_token: Option<String>,
    pub auth_token: Option<String>,
}

/// Merge two configs: `overlay` values take precedence over `base` values.
/// For Option fields: overlay.Some overrides base.Some; overlay.None keeps base.
/// For primitive fields: overlay's value always wins (even if it's the default).
/// This is why we only apply CLI overrides for explicitly-set flags.
fn merge_config(base: NodeConfig, overlay: NodeConfig) -> NodeConfig {
    NodeConfig {
        node: NodeSection {
            node_id: overlay.node.node_id,
        },
        storage: StorageSection {
            db_path: overlay.storage.db_path,
        },
        nats: NatsSection {
            url: overlay.nats.url,
            creds_file: overlay.nats.creds_file.or(base.nats.creds_file),
        },
        proxy: ProxySection {
            http_port: overlay.proxy.http_port,
            https_port: overlay.proxy.https_port,
            tls_cert: overlay.proxy.tls_cert.or(base.proxy.tls_cert),
            tls_key: overlay.proxy.tls_key.or(base.proxy.tls_key),
        },
        admin: AdminSection {
            port: overlay.admin.port,
            artifact_port: overlay.admin.artifact_port,
            auth_token: overlay.admin.auth_token.or(base.admin.auth_token),
        },
        runtime: RuntimeSection {
            port_start: overlay.runtime.port_start,
            port_end: overlay.runtime.port_end,
            key_source: overlay.runtime.key_source,
            key_file: overlay.runtime.key_file.or(base.runtime.key_file),
        },
        database: DatabaseSection {
            default_url: overlay.database.default_url,
            pgbouncer_addr: overlay.database.pgbouncer_addr,
            enable_db_proxy: overlay.database.enable_db_proxy,
            db_proxy_addr: overlay.database.db_proxy_addr,
            db_proxy_backend: overlay.database.db_proxy_backend,
            db_proxy_max_connections: overlay.database.db_proxy_max_connections,
        },
        logging: LoggingSection {
            level: overlay.logging.level,
            otlp_endpoint: overlay.logging.otlp_endpoint.or(base.logging.otlp_endpoint),
        },
        billing: BillingSection {
            export_dir: overlay.billing.export_dir.or(base.billing.export_dir),
            export_interval_secs: overlay.billing.export_interval_secs,
        },
        gc: GcSection {
            artifact_keep_versions: overlay.gc.artifact_keep_versions,
            metrics_retain_days: overlay.gc.metrics_retain_days,
            undeploy_grace_secs: overlay.gc.undeploy_grace_secs,
            gc_interval_secs: overlay.gc.gc_interval_secs,
            disk_warning_threshold: overlay.gc.disk_warning_threshold,
        },
        rate_limit: RateLimitSection {
            default_requests_per_second: overlay.rate_limit.default_requests_per_second,
            default_burst_capacity: overlay.rate_limit.default_burst_capacity,
            default_per_ip_limit: overlay.rate_limit.default_per_ip_limit,
        },
        ebpf: EbpfSection {
            enabled: overlay.ebpf.enabled,
            fd_soft_limit: overlay.ebpf.fd_soft_limit,
            fd_hard_limit: overlay.ebpf.fd_hard_limit,
            mem_low_threshold_pages: overlay.ebpf.mem_low_threshold_pages,
            mem_critical_threshold_pages: overlay.ebpf.mem_critical_threshold_pages,
            disk_slow_threshold_ns: overlay.ebpf.disk_slow_threshold_ns,
            tcp_conn_limit_per_pid: overlay.ebpf.tcp_conn_limit_per_pid,
            syscall_rate_limit: overlay.ebpf.syscall_rate_limit,
            sampling_period_secs: overlay.ebpf.sampling_period_secs,
        },
        dns: DnsSection {
            platform_domain: overlay.dns.platform_domain.or(base.dns.platform_domain),
            webhook_url: overlay.dns.webhook_url.or(base.dns.webhook_url),
            webhook_token: overlay.dns.webhook_token.or(base.dns.webhook_token),
        },
        health: HealthSection {
            check_interval_secs: overlay.health.check_interval_secs,
            default_idle_timeout_secs: overlay.health.default_idle_timeout_secs,
            default_max_instances: overlay.health.default_max_instances,
            default_fuel_quota: overlay.health.default_fuel_quota,
            default_memory_pages: overlay.health.default_memory_pages,
        },
    }
}

/// Apply environment variable overrides.
/// Convention: WASM_NODE_<SECTION>_<KEY> (uppercase, underscores for dots)
/// Examples:
///   WASM_NODE_NODE_ID=node-1
///   WASM_NODE_NATS_URL=nats://nats.prod:4222
///   WASM_NODE_LOGGING_LEVEL=debug
///   WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND=5000
fn apply_env_overrides(mut config: NodeConfig) -> NodeConfig {
    // Node
    if let Ok(v) = std::env::var("WASM_NODE_NODE_ID") {
        config.node.node_id = v;
    }
    // Storage
    if let Ok(v) = std::env::var("WASM_NODE_STORAGE_DB_PATH") {
        config.storage.db_path = PathBuf::from(v);
    }
    // NATS
    if let Ok(v) = std::env::var("WASM_NODE_NATS_URL") {
        config.nats.url = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_NATS_CREDS_FILE") {
        config.nats.creds_file = Some(v);
    }
    // Proxy
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTP_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.http_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_HTTPS_PORT") {
        if let Ok(port) = v.parse() {
            config.proxy.https_port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_CERT") {
        config.proxy.tls_cert = Some(v);
    }
    if let Ok(v) = std::env::var("WASM_NODE_PROXY_TLS_KEY") {
        config.proxy.tls_key = Some(v);
    }
    // Admin
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_PORT") {
        if let Ok(port) = v.parse() {
            config.admin.port = port;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_ADMIN_AUTH_TOKEN") {
        config.admin.auth_token = Some(v);
    }
    // Logging
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_LEVEL") {
        config.logging.level = v;
    }
    if let Ok(v) = std::env::var("WASM_NODE_LOGGING_OTLP_ENDPOINT") {
        config.logging.otlp_endpoint = Some(v);
    }
    // Rate limit
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_REQUESTS_PER_SECOND") {
        if let Ok(rps) = v.parse() {
            config.rate_limit.default_requests_per_second = rps;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_BURST_CAPACITY") {
        if let Ok(burst) = v.parse() {
            config.rate_limit.default_burst_capacity = burst;
        }
    }
    if let Ok(v) = std::env::var("WASM_NODE_RATE_LIMIT_DEFAULT_PER_IP_LIMIT") {
        if let Ok(limit) = v.parse() {
            config.rate_limit.default_per_ip_limit = limit;
        }
    }
    // eBPF
    if let Ok(v) = std::env::var("WASM_NODE_EBPF_ENABLED") {
        if let Ok(enabled) = v.parse() {
            config.ebpf.enabled = enabled;
        }
    }
    config
}

/// Apply CLI flag overrides. Only non-None values override.
fn apply_cli_overrides(mut config: NodeConfig, cli: &CliOverrides) -> NodeConfig {
    if let Some(v) = &cli.node_id { config.node.node_id = v.clone(); }
    if let Some(v) = &cli.db_path { config.storage.db_path = PathBuf::from(v); }
    if let Some(v) = &cli.nats_url { config.nats.url = v.clone(); }
    if let Some(v) = &cli.nats_creds { config.nats.creds_file = Some(v.clone()); }
    if let Some(v) = cli.http_port { config.proxy.http_port = v; }
    if let Some(v) = cli.https_port { config.proxy.https_port = v; }
    if let Some(v) = &cli.tls_cert { config.proxy.tls_cert = Some(v.clone()); }
    if let Some(v) = &cli.tls_key { config.proxy.tls_key = Some(v.clone()); }
    if let Some(v) = cli.admin_port { config.admin.port = v; }
    if let Some(v) = cli.artifact_port { config.admin.artifact_port = v; }
    if let Some(v) = cli.port_start { config.runtime.port_start = v; }
    if let Some(v) = cli.port_end { config.runtime.port_end = v; }
    if let Some(v) = &cli.key_source { config.runtime.key_source = v.clone(); }
    if let Some(v) = &cli.key_file { config.runtime.key_file = Some(v.clone()); }
    if let Some(v) = &cli.database_url { config.database.default_url = v.clone(); }
    if let Some(v) = &cli.pgbouncer_addr { config.database.pgbouncer_addr = v.clone(); }
    if let Some(v) = cli.enable_db_proxy { config.database.enable_db_proxy = v; }
    if let Some(v) = &cli.db_proxy_addr { config.database.db_proxy_addr = v.clone(); }
    if let Some(v) = &cli.db_proxy_backend { config.database.db_proxy_backend = v.clone(); }
    if let Some(v) = cli.db_proxy_max_connections { config.database.db_proxy_max_connections = v; }
    if let Some(v) = &cli.log_level { config.logging.level = v.clone(); }
    if let Some(v) = &cli.otlp_endpoint { config.logging.otlp_endpoint = Some(v.clone()); }
    if let Some(v) = &cli.billing_export_dir { config.billing.export_dir = Some(v.clone()); }
    if let Some(v) = cli.billing_export_interval_secs { config.billing.export_interval_secs = v; }
    if let Some(v) = &cli.platform_domain { config.dns.platform_domain = Some(v.clone()); }
    if let Some(v) = &cli.dns_webhook_url { config.dns.webhook_url = Some(v.clone()); }
    if let Some(v) = &cli.dns_webhook_token { config.dns.webhook_token = Some(v.clone()); }
    if let Some(v) = &cli.auth_token { config.admin.auth_token = Some(v.clone()); }
    config
}

/// Validate the final merged configuration.
fn validate_config(config: &NodeConfig) -> Result<(), PlatformError> {
    let mut errors = Vec::new();

    // Port range
    if config.runtime.port_start >= config.runtime.port_end {
        errors.push("port_start must be less than port_end".to_string());
    }
    if config.runtime.port_end - config.runtime.port_start < 100 {
        errors.push("port range must span at least 100 ports".to_string());
    }

    // Log level
    let valid_levels = ["trace", "debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        errors.push(format!(
            "invalid log level '{}', must be one of: {}",
            config.logging.level,
            valid_levels.join(", ")
        ));
    }

    // GC thresholds
    if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
        errors.push("disk_warning_threshold must be between 0.0 and 1.0".to_string());
    }
    if config.gc.artifact_keep_versions == 0 {
        errors.push("artifact_keep_versions must be > 0".to_string());
    }

    // eBPF
    if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
        errors.push("fd_soft_limit must be less than fd_hard_limit".to_string());
    }
    if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
        errors.push("mem_low_threshold_pages must be greater than mem_critical_threshold_pages".to_string());
    }

    // Health
    if config.health.check_interval_secs == 0 {
        errors.push("check_interval_secs must be > 0".to_string());
    }
    if config.health.default_fuel_quota == 0 {
        errors.push("default_fuel_quota must be > 0".to_string());
    }
    if config.health.default_memory_pages == 0 {
        errors.push("default_memory_pages must be > 0".to_string());
    }

    // Rate limits
    if config.rate_limit.default_requests_per_second == 0 {
        errors.push("default_requests_per_second must be > 0".to_string());
    }

    // TLS consistency
    if config.proxy.tls_cert.is_some() != config.proxy.tls_key.is_some() {
        errors.push("tls_cert and tls_key must both be set or both be unset".to_string());
    }

    // HTTPS port with no TLS
    if config.proxy.https_port > 0 && config.proxy.tls_cert.is_none() {
        errors.push("https_port requires tls_cert and tls_key".to_string());
    }

    if !errors.is_empty() {
        return Err(PlatformError::ConfigValidation(format!(
            "configuration validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}
```

---

## 4. Hot-Reloadable Configuration

Hot-reloadable parameters are stored in a shared `Arc<RwLock<HotConfig>>` that
all components reference. When a config change is applied, the lock is briefly
acquired for an atomic swap — no component ever sees a partially-updated config.

```rust
// crates/common/src/hot_config.rs
use crate::config::{EbpfSection, GcSection, HealthSection, RateLimitSection, LoggingSection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Configuration that can be changed at runtime without restart.
/// All fields in this struct have corresponding "cold" fields in NodeConfig
/// that require restart. HotConfig overrides those cold values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotConfig {
    pub rate_limit: RateLimitSection,
    pub ebpf: EbpfSection,
    pub gc: GcSection,
    pub health: HealthSection,
    pub logging: LoggingSection,
}

/// A shared handle to the hot-reloadable configuration.
/// Components read from this; the admin API writes to it.
#[derive(Clone)]
pub struct HotConfigHandle {
    inner: Arc<RwLock<HotConfig>>,
    /// Persistence layer for surviving restarts.
    store: Option<storage::Store>,
    node_id: String,
}

impl HotConfigHandle {
    pub fn new(initial: HotConfig, store: Option<storage::Store>, node_id: String) -> Self {
        // If persisted overrides exist, apply them on top of the initial config
        let effective = if let Some(ref s) = store {
            match s.load_hot_config_overrides() {
                Ok(Some(persisted)) => {
                    info!("applied persisted hot-config overrides from redb");
                    Self::merge_hot_config(initial, persisted)
                }
                Ok(None) => initial,
                Err(e) => {
                    warn!(error = %e, "failed to load persisted hot-config, using defaults");
                    initial
                }
            }
        } else {
            initial
        };

        HotConfigHandle {
            inner: Arc::new(RwLock::new(effective)),
            store,
            node_id,
        }
    }

    /// Read the current hot config (cheap — only acquires read lock).
    pub async fn read(&self) -> HotConfig {
        self.inner.read().await.clone()
    }

    /// Apply a partial update to the hot config.
    /// Only fields present in the update are changed; others remain unchanged.
    /// Returns the previous config for audit logging.
    pub async fn apply_update(
        &self,
        update: HotConfigUpdate,
    ) -> Result<HotConfig, String> {
        let mut config = self.inner.write().await;
        let previous = config.clone();

        if let Some(v) = update.rate_limit_default_rps {
            config.rate_limit.default_requests_per_second = v;
        }
        if let Some(v) = update.rate_limit_default_burst {
            config.rate_limit.default_burst_capacity = v;
        }
        if let Some(v) = update.rate_limit_default_per_ip {
            config.rate_limit.default_per_ip_limit = v;
        }
        if let Some(v) = update.ebpf_fd_soft_limit {
            config.ebpf.fd_soft_limit = v;
        }
        if let Some(v) = update.ebpf_fd_hard_limit {
            config.ebpf.fd_hard_limit = v;
        }
        if let Some(v) = update.ebpf_mem_low_threshold_pages {
            config.ebpf.mem_low_threshold_pages = v;
        }
        if let Some(v) = update.ebpf_mem_critical_threshold_pages {
            config.ebpf.mem_critical_threshold_pages = v;
        }
        if let Some(v) = update.ebpf_disk_slow_threshold_ns {
            config.ebpf.disk_slow_threshold_ns = v;
        }
        if let Some(v) = update.ebpf_tcp_conn_limit_per_pid {
            config.ebpf.tcp_conn_limit_per_pid = v;
        }
        if let Some(v) = update.ebpf_syscall_rate_limit {
            config.ebpf.syscall_rate_limit = v;
        }
        if let Some(v) = update.gc_interval_secs {
            config.gc.gc_interval_secs = v;
        }
        if let Some(v) = update.gc_disk_warning_threshold {
            config.gc.disk_warning_threshold = v;
        }
        if let Some(v) = update.health_check_interval_secs {
            config.health.check_interval_secs = v;
        }
        if let Some(v) = update.health_default_idle_timeout_secs {
            config.health.default_idle_timeout_secs = v;
        }
        if let Some(v) = update.logging_level {
            config.logging.level = v;
        }

        // Validate the updated config
        Self::validate_hot_config(&config)?;

        // Persist the overrides to redb so they survive restart
        if let Some(ref store) = self.store {
            if let Err(e) = store.save_hot_config_overrides(&config) {
                warn!(error = %e, "failed to persist hot-config overrides");
            }
        }

        info!(
            "hot config updated: {} field(s) changed",
            update.count_changes()
        );

        Ok(previous)
    }

    /// Reset hot config to the cold (startup) values.
    pub async fn reset(&self, base: HotConfig) -> Result<HotConfig, String> {
        let mut config = self.inner.write().await;
        let previous = config.clone();
        *config = base.clone();

        if let Some(ref store) = self.store {
            store.clear_hot_config_overrides().ok();
        }

        info!("hot config reset to startup defaults");
        Ok(previous)
    }

    fn validate_hot_config(config: &HotConfig) -> Result<(), String> {
        if config.ebpf.fd_soft_limit >= config.ebpf.fd_hard_limit {
            return Err("fd_soft_limit must be less than fd_hard_limit".to_string());
        }
        if config.ebpf.mem_low_threshold_pages <= config.ebpf.mem_critical_threshold_pages {
            return Err("mem_low_threshold_pages must be > mem_critical_threshold_pages".to_string());
        }
        if config.gc.disk_warning_threshold <= 0.0 || config.gc.disk_warning_threshold > 1.0 {
            return Err("disk_warning_threshold must be between 0.0 and 1.0".to_string());
        }
        let valid_levels = ["trace", "debug", "info", "warn", "error"];
        if !valid_levels.contains(&config.logging.level.as_str()) {
            return Err(format!("invalid log level: {}", config.logging.level));
        }
        Ok(())
    }

    fn merge_hot_config(base: HotConfig, overlay: HotConfig) -> HotConfig {
        // Overlay wins for all fields — it represents persisted runtime overrides
        overlay
    }
}

/// A partial update to hot config. Only Some fields are applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotConfigUpdate {
    pub rate_limit_default_rps: Option<u32>,
    pub rate_limit_default_burst: Option<u32>,
    pub rate_limit_default_per_ip: Option<u32>,
    pub ebpf_fd_soft_limit: Option<u32>,
    pub ebpf_fd_hard_limit: Option<u32>,
    pub ebpf_mem_low_threshold_pages: Option<u64>,
    pub ebpf_mem_critical_threshold_pages: Option<u64>,
    pub ebpf_disk_slow_threshold_ns: Option<u64>,
    pub ebpf_tcp_conn_limit_per_pid: Option<u32>,
    pub ebpf_syscall_rate_limit: Option<u64>,
    pub gc_interval_secs: Option<u64>,
    pub gc_disk_warning_threshold: Option<f64>,
    pub health_check_interval_secs: Option<u64>,
    pub health_default_idle_timeout_secs: Option<u64>,
    pub logging_level: Option<String>,
}

impl HotConfigUpdate {
    pub fn count_changes(&self) -> usize {
        let mut count = 0;
        if self.rate_limit_default_rps.is_some() { count += 1; }
        if self.rate_limit_default_burst.is_some() { count += 1; }
        if self.rate_limit_default_per_ip.is_some() { count += 1; }
        if self.ebpf_fd_soft_limit.is_some() { count += 1; }
        if self.ebpf_fd_hard_limit.is_some() { count += 1; }
        if self.ebpf_mem_low_threshold_pages.is_some() { count += 1; }
        if self.ebpf_mem_critical_threshold_pages.is_some() { count += 1; }
        if self.ebpf_disk_slow_threshold_ns.is_some() { count += 1; }
        if self.ebpf_tcp_conn_limit_per_pid.is_some() { count += 1; }
        if self.ebpf_syscall_rate_limit.is_some() { count += 1; }
        if self.gc_interval_secs.is_some() { count += 1; }
        if self.gc_disk_warning_threshold.is_some() { count += 1; }
        if self.health_check_interval_secs.is_some() { count += 1; }
        if self.health_default_idle_timeout_secs.is_some() { count += 1; }
        if self.logging_level.is_some() { count += 1; }
        count
    }
}
```

---

## 5. Hot Config Persistence in redb

Runtime overrides are persisted in the `SCHEMA_META` table so they survive restart.
This is separate from the cold config (TOML file + env + CLI) which is re-evaluated
on every startup.

```rust
// crates/storage/src/hot_config.rs
use crate::{tables::SCHEMA_META, Store};
use common::error::PlatformError;
use common::hot_config::HotConfig;
use redb::ReadableTable;

const HOT_CONFIG_KEY: &str = "hot_config_override";

impl Store {
    /// Save the current hot config overrides to redb.
    pub fn save_hot_config_overrides(&self, config: &HotConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(HOT_CONFIG_KEY, json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Load persisted hot config overrides from redb.
    /// Returns None if no overrides have been saved.
    pub fn load_hot_config_overrides(&self) -> Result<Option<HotConfig>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(SCHEMA_META)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(HOT_CONFIG_KEY)
            .map_err(|e| PlatformError::Storage(e.to_string()))?
        {
            Some(v) => {
                let config: HotConfig = serde_json::from_str(v.value())
                    .map_err(|e| PlatformError::Storage(e.to_string()))?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Clear persisted hot config overrides (reset to defaults).
    pub fn clear_hot_config_overrides(&self) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(SCHEMA_META)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(HOT_CONFIG_KEY)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        Ok(())
    }
}
```

---

## 6. Log Level Hot-Reload

Changing the log level at runtime requires updating the tracing subscriber.
The `tracing-subscriber` crate supports reload handles for this purpose.

```rust
// crates/node/src/log_reload.rs
use tracing_subscriber::{
    fmt::fmt,
    EnvFilter,
    reload::{self, Handle},
};

/// A handle that allows changing the log filter at runtime.
pub struct LogReloadHandle {
    handle: reload::Handle<EnvFilter, tracing_subscriber::fmt::Layer<tracing_subscriber::fmt::format::DefaultFields, tracing_subscriber::fmt::format::Format, EnvFilter>>,
}

impl LogReloadHandle {
    /// Create a new tracing subscriber with a reload handle.
    /// Returns the handle and the subscriber to pass to `tracing::subscriber::set_global_default`.
    pub fn init(default_level: &str) -> (Self, impl tracing::Subscriber) {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default_level));

        let (filter, reload_handle) = reload::Layer::new(filter);
        let subscriber = fmt()
            .with_env_filter(filter)
            .finish();

        (LogReloadHandle { handle: reload_handle }, subscriber)
    }

    /// Change the log level at runtime.
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(level);
        self.handle.reload(new_filter)
            .map_err(|e| format!("failed to reload log filter: {}", e))
    }
}
```

---

## 7. Admin API Endpoints for Configuration

```rust
// crates/proxy/src/admin.rs — additions for config management

use axum::{
    extract::State,
    Json,
    http::StatusCode,
};
use common::hot_config::{HotConfigHandle, HotConfigUpdate};
use common::config::NodeConfig;
use serde_json::{json, Value};

/// GET /admin/config — Show the effective configuration.
/// Returns both cold (startup) and hot (runtime) config.
pub async fn get_config(
    State(state): State<AdminState>,
) -> Json<Value> {
    let hot = state.hot_config.read().await;
    Json(json!({
        "cold": {
            "node_id": state.cold_config.node.node_id,
            "nats_url": state.cold_config.nats.url,
            "proxy_http_port": state.cold_config.proxy.http_port,
            "proxy_https_port": state.cold_config.proxy.https_port,
            "admin_port": state.cold_config.admin.port,
            "db_path": state.cold_config.storage.db_path.to_string_lossy(),
            "port_range": format!("{}-{}", state.cold_config.runtime.port_start, state.cold_config.runtime.port_end),
        },
        "hot": {
            "rate_limit": hot.rate_limit,
            "ebpf": hot.ebpf,
            "gc": hot.gc,
            "health": hot.health,
            "logging": hot.logging,
        },
        "hot_reloadable_fields": [
            "rate_limit.default_requests_per_second",
            "rate_limit.default_burst_capacity",
            "rate_limit.default_per_ip_limit",
            "ebpf.fd_soft_limit",
            "ebpf.fd_hard_limit",
            "ebpf.mem_low_threshold_pages",
            "ebpf.mem_critical_threshold_pages",
            "ebpf.disk_slow_threshold_ns",
            "ebpf.tcp_conn_limit_per_pid",
            "ebpf.syscall_rate_limit",
            "gc.gc_interval_secs",
            "gc.disk_warning_threshold",
            "health.check_interval_secs",
            "health.default_idle_timeout_secs",
            "logging.level",
        ],
    }))
}

/// PATCH /admin/config — Update hot-reloadable configuration.
/// Only fields present in the JSON body are changed.
pub async fn update_config(
    State(state): State<AdminState>,
    Json(update): Json<HotConfigUpdate>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let previous = state.hot_config.apply_update(update).await
        .map_err(|e| {
            (StatusCode::BAD_REQUEST, Json(json!({
                "error": "validation_failed",
                "message": e,
            })))
        })?;

    // If log level changed, apply it to the tracing subscriber
    if let Some(ref level) = update.logging_level {
        if let Some(ref handle) = state.log_reload_handle {
            if let Err(e) = handle.set_level(level) {
                tracing::warn!(error = %e, "failed to apply log level change");
            }
        }
    }

    // Publish config change event to NATS
    if let Some(ref bus) = state.bus {
        let event = messaging::events::Event::ConfigHotReload {
            node_id: state.node_id.clone(),
            changes: update.clone(),
        };
        bus.publish(&event).await.ok();
    }

    Ok((StatusCode::OK, Json(json!({
        "status": "updated",
        "previous": previous,
        "changes_applied": update.count_changes(),
    }))))
}

/// DELETE /admin/config — Reset hot config to startup defaults.
pub async fn reset_config(
    State(state): State<AdminState>,
) -> Json<Value> {
    let base = HotConfig::from_cold(&state.cold_config);
    let previous = state.hot_config.reset(base).await.ok();

    Json(json!({
        "status": "reset",
        "previous": previous,
        "message": "Hot config reset to startup defaults. Restart to re-read TOML file.",
    }))
}

/// State shared by admin API handlers.
pub struct AdminState {
    pub cold_config: NodeConfig,
    pub hot_config: HotConfigHandle,
    pub log_reload_handle: Option<LogReloadHandle>,
    pub bus: Option<messaging::NatsBus>,
    pub node_id: String,
}
```

---

## 8. NATS Config Change Propagation

When one node changes its hot config, other nodes may need to know about it
(e.g., to adjust their own rate limits consistently, or to log the change for audit).

```rust
// crates/messaging/src/events.rs — addition to the Event enum

pub enum Event {
    // ... existing events ...

    /// A node changed its hot-reloadable configuration.
    ConfigHotReload {
        node_id: String,
        changes: common::hot_config::HotConfigUpdate,
    },
}

// In the Event::subject() match arm:
Event::ConfigHotReload { node_id, .. } => {
    format!("config.hot_reload.{}", node_id)
}
```

### Peer Node Reaction

When a node receives `ConfigHotReload`:

```rust
// In the event dispatcher
Event::ConfigHotReload { node_id, changes } => {
    tracing::info!(
        node = %node_id,
        changes = changes.count_changes(),
        "peer node changed hot config"
    );
    // Log for audit but do NOT auto-apply — each node's operator controls its own config.
    // The event is informational: it tells operators that the cluster may now have
    // inconsistent rate limits or thresholds.
}
```

**Design decision: Config changes are NOT auto-propagated.** Each node's operator
is responsible for its own configuration. The `ConfigHotReload` event is informational
only — it alerts operators that the cluster's configuration may have diverged.

If an operator wants to apply the same change cluster-wide, they use:
```bash
# Apply to all nodes
for node in node-0 node-1 node-2; do
  wasm-ctl node config --target $node --set rate_limit.default_requests_per_second=5000
done
```

---

## 9. CLI Commands

```
# View effective configuration
wasm-ctl node config --target node-0
# Output:
# Cold (startup) config:
#   node_id: node-0
#   nats_url: nats://127.0.0.1:4222
#   proxy_http_port: 8080
#   db_path: /var/lib/wasm-node/state.redb
#   port_range: 10000-19999
#
# Hot (runtime) config:
#   rate_limit.default_requests_per_second: 1000
#   rate_limit.default_burst_capacity: 200
#   rate_limit.default_per_ip_limit: 200
#   ebpf.fd_soft_limit: 8192
#   ebpf.fd_hard_limit: 9728
#   ebpf.mem_low_threshold_pages: 65536
#   ebpf.mem_critical_threshold_pages: 16384
#   ebpf.disk_slow_threshold_ns: 50000000
#   ebpf.tcp_conn_limit_per_pid: 10000
#   ebpf.syscall_rate_limit: 100000
#   gc.gc_interval_secs: 600
#   gc.disk_warning_threshold: 0.80
#   health.check_interval_secs: 5
#   health.default_idle_timeout_secs: 300
#   logging.level: info

# Change a hot-reloadable parameter
wasm-ctl node config --target node-0 \
  --set rate_limit.default_requests_per_second=5000

# Change multiple parameters at once
wasm-ctl node config --target node-0 \
  --set rate_limit.default_requests_per_second=5000 \
  --set rate_limit.default_burst_capacity=1000 \
  --set logging.level=debug

# Reset hot config to startup defaults
wasm-ctl node config --target node-0 --reset

# Generate a default config file
wasm-node --generate-config > /etc/wasm-node/config.toml

# Validate a config file without starting the node
wasm-node --validate-config /etc/wasm-node/config.toml
```

---

## 10. Node main.rs Integration

```rust
// crates/node/src/main.rs — updated startup sequence

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Parse CLI args (now includes --config flag)
    let args = Args::parse();

    // 2. Load configuration from all sources
    let cli_overrides = args.to_cli_overrides();
    let config = common::config_loader::load_config(
        args.config.as_deref(),
        &cli_overrides,
    )?;

    // 3. Initialize tracing with reload handle
    let (log_handle, subscriber) = log_reload::LogReloadHandle::init(&config.logging.level);
    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to set tracing subscriber");

    info!(node_id = %config.node.node_id, "wasm-node starting");
    info!(config_merge = "defaults + TOML + env + CLI", "configuration loaded");

    // 4. Initialize hot config handle
    let store = storage::Store::open(&config.storage.db_path)?;
    let hot_config = HotConfigHandle::new(
        HotConfig::from_cold(&config),
        Some(store.clone()),
        config.node.node_id.clone(),
    );

    // ... rest of startup uses config.xxx instead of args.xxx ...
    // All components that need hot-reloadable params receive hot_config.clone()

    // Admin API now includes config endpoints
    let admin_app = axum::Router::new()
        .merge(health_router)
        .route("/admin/config", axum::routing::get(get_config))
        .route("/admin/config", axum::routing::patch(update_config))
        .route("/admin/config", axum::routing::delete(reset_config))
        .with_state(AdminState {
            cold_config: config.clone(),
            hot_config: hot_config.clone(),
            log_reload_handle: Some(log_handle),
            bus: Some(bus.clone()),
            node_id: config.node.node_id.clone(),
        });

    Ok(())
}
```

### Updated CLI Args

```rust
#[derive(Parser, Debug)]
#[command(name = "wasm-node", about = "Wasm Cloud Platform Node")]
struct Args {
    /// Path to TOML configuration file.
    /// If not set, only defaults + env vars + CLI flags are used.
    #[arg(long, short = 'c', env = "WASM_NODE_CONFIG")]
    config: Option<String>,

    /// Generate a default config file and print to stdout.
    #[arg(long)]
    generate_config: bool,

    /// Validate a config file and exit.
    #[arg(long)]
    validate_config: Option<String>,

    // ... all existing CLI flags remain for backward compatibility ...
    // They are converted to CliOverrides with Some() only when explicitly set.
}
```

---

## 11. Component Integration with HotConfig

Each component that uses hot-reloadable parameters reads from `HotConfigHandle`
instead of a static value.

### Rate Limiter Integration

```rust
// crates/proxy/src/rate_limiter.rs — updated to use HotConfigHandle

impl RateLimiter {
    /// Create a new rate limiter with initial config from HotConfigHandle.
    pub fn new(hot_config: HotConfigHandle) -> Self {
        let initial = tokio::task::block_in_place(|| {
            // Synchronous read for construction
            let rt = tokio::runtime::Handle::current();
            rt.block_on(hot_config.read())
        });

        RateLimiter {
            app_buckets: RwLock::new(HashMap::new()),
            ip_buckets: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            default_config: RateLimitConfig {
                requests_per_second: initial.rate_limit.default_requests_per_second,
                burst_capacity: initial.rate_limit.default_burst_capacity,
                per_ip_limit: initial.rate_limit.default_per_ip_limit,
            },
            hot_config,
        }
    }

    /// Check if the default config has been updated via hot-reload.
    /// Called periodically (every 10s) to sync with HotConfigHandle.
    pub async fn sync_hot_config(&self) {
        let hot = self.hot_config.read().await;
        let mut configs = self.configs.write().await;
        // Update the default config
        self.default_config = RateLimitConfig {
            requests_per_second: hot.rate_limit.default_requests_per_second,
            burst_capacity: hot.rate_limit.default_burst_capacity,
            per_ip_limit: hot.rate_limit.default_per_ip_limit,
        };
    }
}
```

### eBPF Monitor Integration

```rust
// crates/ebpf-monitor/src/loader.rs — updated to sync config map

/// Periodically sync the eBPF config map from HotConfigHandle.
pub async fn sync_ebpf_config(
    hot_config: HotConfigHandle,
    ebpf: Arc<Ebpf>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let hot = hot_config.read().await;
        if let Ok(config_map) = ebpf.map::<Array<_, MonitorConfigMap>>("CONFIG")
            .and_then(|m| m.try_into().ok())
        {
            let kernel_config = MonitorConfigMap {
                fd_soft_limit: hot.ebpf.fd_soft_limit,
                fd_hard_limit: hot.ebpf.fd_hard_limit,
                mem_low_threshold_pages: hot.ebpf.mem_low_threshold_pages,
                mem_critical_threshold_pages: hot.ebpf.mem_critical_threshold_pages,
                disk_slow_threshold_ns: hot.ebpf.disk_slow_threshold_ns,
                tcp_conn_limit_per_pid: hot.ebpf.tcp_conn_limit_per_pid,
                syscall_rate_limit: hot.ebpf.syscall_rate_limit,
                sampling_period_ns: hot.ebpf.sampling_period_secs * 1_000_000_000,
                ..Default::default()
            };
            config_map.set(0, kernel_config, 0).ok();
        }
    }
}
```

---

## 12. Testing Strategy

### Unit Tests

```bash
cargo test -p common --lib  # Config loading, validation, merge priority
```

Tests to implement:
- `test_default_config_valid`: Default `NodeConfig` passes validation
- `test_toml_parse_minimal`: Minimal TOML file parses correctly
- `test_toml_parse_full`: Full TOML file with all sections
- `test_merge_priority_env_over_toml`: Env var overrides TOML value
- `test_merge_priority_cli_over_env`: CLI flag overrides env var
- `test_validation_port_range_swapped`: `port_start > port_end` rejected
- `test_validation_invalid_log_level`: Bad log level rejected
- `test_validation_tls_consistency`: Only one of cert/key set rejected
- `test_hot_config_update_partial`: Only specified fields change
- `test_hot_config_persistence`: Override survives simulated restart
- `test_hot_config_reset`: Reset clears overrides
- `test_hot_config_validation`: Invalid update rejected, previous config preserved

### Integration Tests

```bash
cargo test -p storage --tests  # Hot config persistence in redb
```

Tests to implement:
- `test_hot_config_save_and_load`: Round-trip through redb
- `test_hot_config_clear`: Clear removes persisted overrides

### E2E Tests

```bash
cargo test -p e2e -- --ignored --test-threads=1
```

Tests to implement:
- `test_config_change_rate_limit`: Change rate limit via admin API, verify new limit enforced
- `test_config_change_log_level`: Change log level, verify tracing output changes
- `test_config_persistence_across_restart`: Change config, restart node, verify override applied
- `test_config_reset`: Reset config, verify defaults restored

---

## 13. Migration from CLI-Only to File-Based Configuration

### Backward Compatibility

All existing CLI flags continue to work. The `--config` flag is optional.
If no config file is provided, behavior is identical to the current system:
defaults + env vars + CLI flags.

### Migration Path

```
Phase 1: Add --config flag and NodeConfig struct. All existing flags still work.
         Internally, CLI flags are converted to CliOverrides and merged.

Phase 2: Add hot-reload endpoints to admin API. Components read from HotConfigHandle.

Phase 3: Add --generate-config flag. Operators generate a config file from their
         current CLI flags and switch to file-based configuration.

Phase 4: Document recommended config file layout for production deployments.
```

### Generating a Config File from Current Flags

```bash
# Generate a config file that matches the current CLI invocation
wasm-node --generate-config \
  --node-id node-0 \
  --nats-url nats://prod-nats:4222 \
  --proxy-port 8080 \
  --admin-port 9090 \
  > /etc/wasm-node/config.toml

# Then simplify the systemd unit:
# Before: wasm-node --node-id node-0 --nats-url nats://prod-nats:4222 ...
# After:  wasm-node --config /etc/wasm-node/config.toml
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Configuration File
- [ ] `NodeConfig` struct defined with all sections and `serde` defaults
- [ ] TOML file format documented with all fields and comments
- [ ] `load_config()` implements merge priority: defaults < TOML < env < CLI
- [ ] Environment variable convention documented (`WASM_NODE_<SECTION>_<KEY>`)
- [ ] `CliOverrides` struct captures only explicitly-set CLI flags
- [ ] `validate_config()` rejects all invalid configurations
- [ ] `--generate-config` flag outputs a complete TOML file
- [ ] `--validate-config` flag validates a file without starting

### Hot-Reload
- [ ] `HotConfig` struct contains only hot-reloadable fields
- [ ] `HotConfigHandle` provides `Arc<RwLock<>>` access to components
- [ ] `HotConfigUpdate` supports partial updates (only Some fields applied)
- [ ] Hot config validation runs before applying updates
- [ ] Failed validation preserves previous config (atomic swap)
- [ ] `LogReloadHandle` updates tracing subscriber log level
- [ ] Rate limiter syncs default config from `HotConfigHandle`
- [ ] eBPF monitor syncs config map from `HotConfigHandle`
- [ ] GC loop interval adjustable at runtime
- [ ] Health loop interval adjustable at runtime

### Persistence
- [ ] Hot config overrides saved to redb `SCHEMA_META` table
- [ ] Overrides loaded on startup and applied on top of cold config
- [ ] `--reset` clears persisted overrides
- [ ] Corrupted override JSON falls back to cold config with warning

### Admin API
- [ ] `GET /admin/config` returns cold + hot config
- [ ] `PATCH /admin/config` applies partial updates
- [ ] `DELETE /admin/config` resets to startup defaults
- [ ] All config changes logged with previous and new values
- [ ] Auth token checked on all admin endpoints (Step 34)

### NATS Propagation
- [ ] `Event::ConfigHotReload` published on config change
- [ ] Peer nodes log informational event (no auto-apply)
- [ ] Event includes `node_id` and `HotConfigUpdate`

### CLI
- [ ] `wasm-ctl node config --target <node>` shows effective config
- [ ] `wasm-ctl node config --set <key>=<value>` changes hot config
- [ ] `wasm-ctl node config --reset` resets to defaults
- [ ] `wasm-ctl node config --json` outputs in JSON format

### Node Integration
- [ ] `main.rs` uses `load_config()` instead of individual `Arg` reads
- [ ] All components receive `HotConfigHandle` instead of static values
- [ ] Backward compatibility: all existing CLI flags still work
- [ ] `--config` flag is optional (no file = current behavior)

### Testing
- [ ] Unit tests for config loading, merge priority, and validation
- [ ] Unit tests for hot config update, reset, and persistence
- [ ] Integration tests for redb persistence round-trip
- [ ] E2E test: rate limit change via admin API takes effect
- [ ] E2E test: log level change via admin API takes effect
- [ ] E2E test: config persistence across node restart

### Documentation
- [ ] `AGENTS.md` updated with `--config` flag and config file path
- [ ] Example `config.toml` files for dev, staging, production
- [ ] Environment variable reference table
- [ ] Hot-reloadable vs. restart-required parameter table
- [ ] systemd unit file example with `--config` flag
