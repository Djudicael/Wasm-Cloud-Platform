# Step 33 — WASI Policy Enforcement

## Goal
Implement per-app resource and network policy enforcement at the WASI host layer,
closing the security gap identified in Step 13 where `NetworkPolicy`, `ExtendedLimits`,
and `IoResourceTracker` are defined but never wired into the Wasmtime runtime. The
system must:
- Enforce per-app outbound connection limits (max connections, allowed destinations)
- Enforce per-app file descriptor limits (max open FDs)
- Enforce per-app filesystem write limits (max bytes written)
- Enforce per-app network egress limits (max bytes sent)
- Restrict which ports a Wasm module can bind to (only its pre-bound port)
- Block privileged syscalls from Wasm instances at the WASI layer
- Provide clear error messages to Wasm apps when a limit is hit (not silent drops)
- Expose policy violation metrics to Prometheus
- Integrate with the eBPF syscall monitor (Step 30) for kernel-level defense in depth
- Require no changes to Wasm app code — enforcement is transparent

---

## Context & Rationale

### The Problem This Solves

Step 13 (Security Model) defines a defense-in-depth strategy with 7 layers. Layer 1
(Wasm SFI) prevents memory escape. Layer 3 (WASI network) is supposed to prevent
unauthorized port binding and outbound connections. But the current implementation
gives every Wasm module unrestricted access:

```rust
// crates/runtime/src/executor.rs — current code
builder.inherit_network();
builder.allow_tcp(true);
builder.allow_udp(true);
builder.allow_ip_name_lookup(true);
```

This means a Wasm module can:
- Connect to any external service (database of another tenant, internal metadata service)
- Bind to any port (not just its assigned port)
- Open unlimited file descriptors
- Write unlimited data to the filesystem
- Send unlimited data over the network

The `IoResourceTracker` in `limits.rs` has methods for tracking all of these, but
nothing calls them. The `NetworkPolicy` struct from Step 13 is not implemented at all.
The `ExtendedLimits` in `AppConfig` are stored but never enforced.

### Why WASI Host-Level Enforcement (Not Just eBPF)

eBPF (Step 30) monitors syscalls at the kernel level — it can detect violations after
they happen and kill the offending instance. But eBPF cannot **prevent** a syscall
from succeeding. It is an observation layer, not an enforcement layer.

WASI host-level enforcement prevents the violation before it happens:
- When a Wasm module calls `socket()`, the WASI host function checks the policy
  **before** creating the socket
- If the policy denies the connection, the Wasm module receives an `EACCES` error
  (standard POSIX permission denied) — it can handle this gracefully
- No kernel-level resource is consumed

The two layers work together:
```
WASI Host (Layer 3)  → Prevents most violations (fast, per-request)
eBPF Monitor (Layer 8) → Catches anything that bypasses WASI (defense in depth)
```

### Why Custom WASI Host Functions (Not Wasmtime Config)

Wasmtime's `WasiCtxBuilder` provides some built-in restrictions:
- `allow_tcp(false)` — disables all TCP (too coarse)
- `allow_ip_name_lookup(false)` — disables DNS (breaks most apps)

These are all-or-nothing switches. They cannot express:
- "Allow TCP but only to these IP ranges"
- "Allow DNS but limit to 10 concurrent connections"
- "Allow file writes but cap at 50 MB"

Custom WASI host functions wrap the standard WASI implementations with policy checks.
When the policy allows the operation, the call passes through to the real WASI
implementation. When the policy denies it, the call returns an error.

### The WASI Preview 2 Challenge

WASI Preview 2 (used by this platform) is based on the Component Model. Resources
like sockets and files are managed through `ResourceTable` and capability handles.
The Wasm module cannot access a resource unless the host provides a handle for it.

This is actually an advantage for policy enforcement: the host controls **which**
handles are created. By intercepting handle creation (socket open, file open), we
can enforce limits before the Wasm module ever sees a handle.

### What About Performance?

Every network and file operation now goes through a policy check. The check is:
1. Look up the app's policy in a HashMap (O(1))
2. Compare counters against limits (integer comparison)
3. If allowed, call the real WASI function

This adds ~50ns per operation. For an app making 10,000 database queries per second,
that's 0.5ms of overhead — negligible compared to the 1–10ms typical database latency.

---

---

## 1. Policy Data Structures

### NetworkPolicy (Replaces Step 13 Stub)

```rust
// crates/common/src/policy.rs
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Network policy for a single Wasm app instance.
/// Enforced at the WASI host layer before any network operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Allow outbound TCP connections.
    pub allow_outbound_tcp: bool,

    /// Allow outbound UDP.
    pub allow_outbound_udp: bool,

    /// Allow DNS resolution (IP name lookup).
    pub allow_dns: bool,

    /// Allowed destination CIDRs for outbound connections.
    /// Empty = all destinations allowed (if allow_outbound_tcp/udp is true).
    /// Non-empty = only these CIDRs are allowed.
    pub allowed_cidrs: Vec<String>,

    /// Denied destination CIDRs (takes precedence over allowed_cidrs).
    /// Useful for blocking specific internal ranges (e.g., metadata service).
    pub denied_cidrs: Vec<String>,

    /// Maximum concurrent outbound connections.
    pub max_outbound_connections: u32,

    /// Maximum total egress bytes (0 = unlimited).
    pub max_egress_bytes: u64,

    /// Ports the app is allowed to bind to.
    /// Normally just one: the pre-bound port from the Supervisor.
    pub allowed_bind_ports: Vec<u16>,

    /// Allow inbound connections (for the app's HTTP server).
    /// This should always be true for apps that receive requests.
    pub allow_inbound: bool,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy {
            allow_outbound_tcp: true,
            allow_outbound_udp: false,
            allow_dns: true,
            allowed_cidrs: Vec::new(),
            denied_cidrs: Vec::new(),
            max_outbound_connections: 100,
            max_egress_bytes: 0, // unlimited by default
            allowed_bind_ports: Vec::new(), // populated at spawn time
            allow_inbound: true,
        }
    }
}

/// Filesystem and I/O policy for a single Wasm app instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemPolicy {
    /// Maximum number of simultaneously open file descriptors.
    pub max_open_fds: u32,

    /// Maximum total bytes written to the filesystem (0 = unlimited).
    pub max_fs_write_bytes: u64,

    /// Maximum total bytes read from the filesystem (0 = unlimited).
    pub max_fs_read_bytes: u64,

    /// Allow the app to create new files.
    pub allow_file_create: bool,

    /// Allow the app to delete files.
    pub allow_file_delete: bool,

    /// Allowed directories (preopen paths). Empty = no filesystem access.
    pub allowed_paths: Vec<String>,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        FilesystemPolicy {
            max_open_fds: 64,
            max_fs_write_bytes: 50 * 1024 * 1024, // 50 MB
            max_fs_read_bytes: 0,                  // unlimited
            allow_file_create: false,
            allow_file_delete: false,
            allowed_paths: Vec::new(), // no filesystem by default
        }
    }
}

/// Combined policy for a Wasm instance.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstancePolicy {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
}

/// Policy configuration stored in AppConfig (operator-facing).
/// Resolved into InstancePolicy at spawn time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyConfig {
    /// Network policy overrides. None = use defaults.
    #[serde(default)]
    pub network: Option<NetworkPolicyConfig>,

    /// Filesystem policy overrides. None = use defaults.
    #[serde(default)]
    pub filesystem: Option<FilesystemPolicyConfig>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            network: None,
            filesystem: None,
        }
    }
}

/// Operator-facing network policy config (in TOML / deploy manifest).
/// All fields are optional — None means "use the platform default".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicyConfig {
    pub allow_outbound_tcp: Option<bool>,
    pub allow_outbound_udp: Option<bool>,
    pub allow_dns: Option<bool>,
    pub allowed_cidrs: Option<Vec<String>>,
    pub denied_cidrs: Option<Vec<String>>,
    pub max_outbound_connections: Option<u32>,
    pub max_egress_bytes: Option<u64>,
}

/// Operator-facing filesystem policy config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemPolicyConfig {
    pub max_open_fds: Option<u32>,
    pub max_fs_write_bytes: Option<u64>,
    pub max_fs_read_bytes: Option<u64>,
    pub allow_file_create: Option<bool>,
    pub allow_file_delete: Option<bool>,
    pub allowed_paths: Option<Vec<String>>,
}

impl PolicyConfig {
    /// Resolve this config into a full InstancePolicy, applying defaults
    /// for any fields not explicitly set.
    pub fn resolve(&self, assigned_port: u16) -> InstancePolicy {
        let net_default = NetworkPolicy::default();
        let fs_default = FilesystemPolicy::default();

        let network = match &self.network {
            Some(cfg) => NetworkPolicy {
                allow_outbound_tcp: cfg.allow_outbound_tcp.unwrap_or(net_default.allow_outbound_tcp),
                allow_outbound_udp: cfg.allow_outbound_udp.unwrap_or(net_default.allow_outbound_udp),
                allow_dns: cfg.allow_dns.unwrap_or(net_default.allow_dns),
                allowed_cidrs: cfg.allowed_cidrs.clone().unwrap_or(net_default.allowed_cidrs),
                denied_cidrs: cfg.denied_cidrs.clone().unwrap_or(net_default.denied_cidrs),
                max_outbound_connections: cfg.max_outbound_connections
                    .unwrap_or(net_default.max_outbound_connections),
                max_egress_bytes: cfg.max_egress_bytes.unwrap_or(net_default.max_egress_bytes),
                allowed_bind_ports: vec![assigned_port],
                allow_inbound: true,
            },
            None => NetworkPolicy {
                allowed_bind_ports: vec![assigned_port],
                ..net_default
            },
        };

        let filesystem = match &self.filesystem {
            Some(cfg) => FilesystemPolicy {
                max_open_fds: cfg.max_open_fds.unwrap_or(fs_default.max_open_fds),
                max_fs_write_bytes: cfg.max_fs_write_bytes.unwrap_or(fs_default.max_fs_write_bytes),
                max_fs_read_bytes: cfg.max_fs_read_bytes.unwrap_or(fs_default.max_fs_read_bytes),
                allow_file_create: cfg.allow_file_create.unwrap_or(fs_default.allow_file_create),
                allow_file_delete: cfg.allow_file_delete.unwrap_or(fs_default.allow_file_delete),
                allowed_paths: cfg.allowed_paths.clone().unwrap_or(fs_default.allowed_paths),
            },
            None => fs_default,
        };

        InstancePolicy { network, filesystem }
    }
}
```

---

## 2. Policy State Tracker (Per-Instance)

The policy state tracker lives in the `StoreState` alongside `WasiCtx` and
`ResourceTable`. It maintains running counters that are checked on every
network and file operation.

```rust
// crates/runtime/src/policy_tracker.rs
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use common::policy::{InstancePolicy, NetworkPolicy, FilesystemPolicy};

/// Atomic counters for a single instance's policy enforcement.
/// Shared between the WASI host functions (which increment) and
/// the metrics exporter (which reads).
#[derive(Debug)]
pub struct PolicyCounters {
    // Network counters
    pub outbound_connections_active: AtomicU32,
    pub outbound_connections_total: AtomicU64,
    pub egress_bytes: AtomicU64,
    pub dns_lookups_total: AtomicU64,
    pub inbound_connections_active: AtomicU32,

    // Filesystem counters
    pub open_fds: AtomicU32,
    pub fd_open_total: AtomicU64,
    pub fs_write_bytes: AtomicU64,
    pub fs_read_bytes: AtomicU64,
    pub file_creates_total: AtomicU64,
    pub file_deletes_total: AtomicU64,

    // Violation counters
    pub connection_denied_total: AtomicU64,
    pub egress_denied_total: AtomicU64,
    pub fd_denied_total: AtomicU64,
    pub fs_write_denied_total: AtomicU64,
    pub bind_denied_total: AtomicU64,
    pub dns_denied_total: AtomicU64,
}

impl PolicyCounters {
    pub fn new() -> Self {
        PolicyCounters {
            outbound_connections_active: AtomicU32::new(0),
            outbound_connections_total: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            dns_lookups_total: AtomicU64::new(0),
            inbound_connections_active: AtomicU32::new(0),
            open_fds: AtomicU32::new(0),
            fd_open_total: AtomicU64::new(0),
            fs_write_bytes: AtomicU64::new(0),
            fs_read_bytes: AtomicU64::new(0),
            file_creates_total: AtomicU64::new(0),
            file_deletes_total: AtomicU64::new(0),
            connection_denied_total: AtomicU64::new(0),
            egress_denied_total: AtomicU64::new(0),
            fd_denied_total: AtomicU64::new(0),
            fs_write_denied_total: AtomicU64::new(0),
            bind_denied_total: AtomicU64::new(0),
            dns_denied_total: AtomicU64::new(0),
        }
    }
}

/// The policy enforcement engine. Lives in StoreState.
/// Called by custom WASI host functions before delegating to the real implementation.
pub struct PolicyEnforcer {
    pub policy: InstancePolicy,
    pub counters: Arc<PolicyCounters>,
}

impl PolicyEnforcer {
    pub fn new(policy: InstancePolicy) -> Self {
        PolicyEnforcer {
            policy,
            counters: Arc::new(PolicyCounters::new()),
        }
    }

    // ── Network Policy Checks ──────────────────────────────────────

    /// Check if an outbound TCP connection is allowed.
    /// Returns Ok(()) if allowed, Err with reason if denied.
    pub fn check_outbound_tcp_connect(&self, dest_ip: IpAddr, dest_port: u16) -> Result<(), PolicyDenied> {
        if !self.policy.network.allow_outbound_tcp {
            self.counters.connection_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::NetworkDisabled { protocol: "tcp" });
        }

        // Check denied CIDRs first (takes precedence)
        if self.ip_in_cidrs(dest_ip, &self.policy.network.denied_cidrs) {
            self.counters.connection_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination in denied_cidrs".to_string(),
            });
        }

        // Check allowed CIDRs (if non-empty, only these are allowed)
        if !self.policy.network.allowed_cidrs.is_empty()
            && !self.ip_in_cidrs(dest_ip, &self.policy.network.allowed_cidrs)
        {
            self.counters.connection_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination not in allowed_cidrs".to_string(),
            });
        }

        // Check connection count
        let current = self.counters.outbound_connections_active.load(Ordering::Relaxed);
        if current >= self.policy.network.max_outbound_connections {
            self.counters.connection_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::ConnectionLimitExceeded {
                current,
                limit: self.policy.network.max_outbound_connections,
            });
        }

        Ok(())
    }

    /// Record that an outbound connection was established.
    pub fn record_outbound_connect(&self) {
        self.counters.outbound_connections_active.fetch_add(1, Ordering::Relaxed);
        self.counters.outbound_connections_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an outbound connection was closed.
    pub fn record_outbound_disconnect(&self) {
        self.counters.outbound_connections_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Check if egress data is allowed (before sending).
    pub fn check_egress(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.network.max_egress_bytes == 0 {
            return Ok(()); // unlimited
        }

        let current = self.counters.egress_bytes.load(Ordering::Relaxed);
        if current + additional_bytes > self.policy.network.max_egress_bytes {
            self.counters.egress_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::EgressLimitExceeded {
                current,
                requested: additional_bytes,
                limit: self.policy.network.max_egress_bytes,
            });
        }

        Ok(())
    }

    /// Record egress bytes after a successful send.
    pub fn record_egress(&self, bytes: u64) {
        self.counters.egress_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Check if a DNS lookup is allowed.
    pub fn check_dns_lookup(&self) -> Result<(), PolicyDenied> {
        if !self.policy.network.allow_dns {
            self.counters.dns_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DnsDisabled);
        }
        self.counters.dns_lookups_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check if binding to a specific port is allowed.
    pub fn check_bind(&self, port: u16) -> Result<(), PolicyDenied> {
        if self.policy.network.allowed_bind_ports.contains(&port) {
            return Ok(());
        }
        self.counters.bind_denied_total.fetch_add(1, Ordering::Relaxed);
        Err(PolicyDenied::BindDenied {
            port,
            allowed: self.policy.network.allowed_bind_ports.clone(),
        })
    }

    // ── Filesystem Policy Checks ───────────────────────────────────

    /// Check if opening a file descriptor is allowed.
    pub fn check_fd_open(&self) -> Result<(), PolicyDenied> {
        let current = self.counters.open_fds.load(Ordering::Relaxed);
        if current >= self.policy.filesystem.max_open_fds {
            self.counters.fd_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::FdLimitExceeded {
                current,
                limit: self.policy.filesystem.max_open_fds,
            });
        }
        Ok(())
    }

    /// Record that an FD was opened.
    pub fn record_fd_open(&self) {
        self.counters.open_fds.fetch_add(1, Ordering::Relaxed);
        self.counters.fd_open_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an FD was closed.
    pub fn record_fd_close(&self) {
        self.counters.open_fds.fetch_sub(1, Ordering::Relaxed);
    }

    /// Check if a filesystem write is allowed.
    pub fn check_fs_write(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.filesystem.max_fs_write_bytes == 0 {
            return Ok(()); // unlimited
        }

        let current = self.counters.fs_write_bytes.load(Ordering::Relaxed);
        if current + additional_bytes > self.policy.filesystem.max_fs_write_bytes {
            self.counters.fs_write_denied_total.fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::FsWriteLimitExceeded {
                current,
                requested: additional_bytes,
                limit: self.policy.filesystem.max_fs_write_bytes,
            });
        }
        Ok(())
    }

    /// Record filesystem write bytes.
    pub fn record_fs_write(&self, bytes: u64) {
        self.counters.fs_write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Check if creating a file is allowed.
    pub fn check_file_create(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_create {
            return Err(PolicyDenied::FileCreateDenied);
        }
        self.counters.file_creates_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check if deleting a file is allowed.
    pub fn check_file_delete(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_delete {
            return Err(PolicyDenied::FileDeleteDenied);
        }
        self.counters.file_deletes_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────────────

    /// Check if an IP address falls within any of the given CIDR strings.
    fn ip_in_cidrs(&self, ip: IpAddr, cidrs: &[String]) -> bool {
        for cidr_str in cidrs {
            if let Ok(cidr) = cidr_str.parse::<ipnet::IpNet>() {
                if cidr.contains(&ip) {
                    return true;
                }
            }
        }
        false
    }
}

use std::net::IpAddr;

/// Reason a policy check denied an operation.
/// Returned as an error from WASI host functions.
#[derive(Debug, Clone)]
pub enum PolicyDenied {
    NetworkDisabled { protocol: &'static str },
    DestinationDenied { ip: String, reason: String },
    ConnectionLimitExceeded { current: u32, limit: u32 },
    EgressLimitExceeded { current: u64, requested: u64, limit: u64 },
    DnsDisabled,
    BindDenied { port: u16, allowed: Vec<u16> },
    FdLimitExceeded { current: u32, limit: u32 },
    FsWriteLimitExceeded { current: u64, requested: u64, limit: u64 },
    FileCreateDenied,
    FileDeleteDenied,
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyDenied::NetworkDisabled { protocol } => {
                write!(f, "outbound {} connections are disabled by policy", protocol)
            }
            PolicyDenied::DestinationDenied { ip, reason } => {
                write!(f, "connection to {} denied: {}", ip, reason)
            }
            PolicyDenied::ConnectionLimitExceeded { current, limit } => {
                write!(f, "outbound connection limit exceeded ({}/{})", current, limit)
            }
            PolicyDenied::EgressLimitExceeded { current, requested, limit } => {
                write!(f, "egress limit exceeded ({}+{} > {})", current, requested, limit)
            }
            PolicyDenied::DnsDisabled => {
                write!(f, "DNS lookups are disabled by policy")
            }
            PolicyDenied::BindDenied { port, allowed } => {
                write!(f, "binding to port {} denied (allowed: {:?})", port, allowed)
            }
            PolicyDenied::FdLimitExceeded { current, limit } => {
                write!(f, "FD limit exceeded ({}/{})", current, limit)
            }
            PolicyDenied::FsWriteLimitExceeded { current, requested, limit } => {
                write!(f, "filesystem write limit exceeded ({}+{} > {})", current, requested, limit)
            }
            PolicyDenied::FileCreateDenied => {
                write!(f, "file creation is disabled by policy")
            }
            PolicyDenied::FileDeleteDenied => {
                write!(f, "file deletion is disabled by policy")
            }
        }
    }
}

impl std::error::Error for PolicyDenied {}
```

---

## 3. Updated StoreState with PolicyEnforcer

The `StoreState` in `executor.rs` is extended with the `PolicyEnforcer`. All WASI
host functions that touch network or filesystem resources check the enforcer before
proceeding.

```rust
// crates/runtime/src/executor.rs — updated StoreState
use crate::policy_tracker::PolicyEnforcer;
use common::policy::InstancePolicy;

pub struct StoreState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
    pub limiter: MemoryLimiter,
    pub policy: PolicyEnforcer,  // Replaces the dead IoResourceTracker
}
```

### Updated spawn_instance

```rust
// crates/runtime/src/executor.rs — updated spawn_instance

impl PreparedModule {
    pub fn spawn_instance(
        &self,
        env_vars: Vec<(String, String)>,
        port: u16,
        policy_config: Option<&common::policy::PolicyConfig>,
    ) -> Result<(RunningInstance, ()), PlatformError> {
        let id = InstanceId::new();

        // Resolve the policy from config + assigned port
        let instance_policy = match policy_config {
            Some(cfg) => cfg.resolve(port),
            None => common::policy::PolicyConfig::default().resolve(port),
        };

        tracing::info!(
            app = %self.config.id.0,
            instance = %id.0,
            port,
            max_outbound_conns = instance_policy.network.max_outbound_connections,
            max_fds = instance_policy.filesystem.max_open_fds,
            "instance policy resolved"
        );

        // Build WASI environment with policy-aware configuration
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdout();
        builder.inherit_stderr();

        // Network: only enable what the policy allows
        if instance_policy.network.allow_outbound_tcp || instance_policy.network.allow_inbound {
            builder.inherit_network();
            builder.allow_tcp(true);
        }
        if instance_policy.network.allow_outbound_udp {
            builder.allow_udp(true);
        }
        if instance_policy.network.allow_dns {
            builder.allow_ip_name_lookup(true);
        }

        // Environment variables
        for (k, v) in env_vars {
            builder.env(&k, &v);
        }
        builder.env("PORT", &port.to_string());

        // Filesystem: only preopen allowed paths
        for path in &instance_policy.filesystem.allowed_paths {
            // Preopen the directory as read-only or read-write
            // depending on whether writes are allowed
            let preopen = wasmtime_wasi::DirPerms::READ;
            let file_perms = wasmtime_wasi::FilePerms::READ;
            // Note: write permissions would be added conditionally
            builder.preopened_dir(path, path, preopen, file_perms)
                .map_err(|e| PlatformError::Runtime(format!("preopen error: {e}")))?;
        }

        let state = StoreState {
            ctx: builder.build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(self.config.memory_limit),
            policy: PolicyEnforcer::new(instance_policy),
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limiter);
        configure_store(&mut store, self.config.fuel_quota)?;

        // Link WASI host functions (with policy-wrapped versions)
        let mut linker = Linker::new(&self.engine);
        add_to_linker_sync(&mut linker)
            .map_err(|e| PlatformError::Runtime(format!("linker error: {e}")))?;

        let instance = linker.instantiate(&mut store, &self.module).map_err(|e| {
            PlatformError::Runtime(format!("instantiation error: {e}"))
        })?;

        Ok((
            RunningInstance {
                id,
                instance,
                store,
                config: self.config.clone(),
                started_at: Instant::now(),
            },
            (),
        ))
    }
}
```

---

## 4. Policy-Aware WASI Host Function Wrappers

The standard WASI Preview 2 host functions are wrapped with policy checks. The
wrapper intercepts the call, checks the policy, and either delegates to the real
implementation or returns an error.

### Approach: Post-Instantiation Hook

Rather than replacing WASI host functions (which is complex with the Component Model),
we use a **post-instantiation hook** approach:

1. The Wasm module is instantiated with standard WASI host functions
2. The `PolicyEnforcer` in `StoreState` is the enforcement point
3. We wrap the WasiCtx's socket and file operations by intercepting at the
   `ResourceTable` level

For WASI Preview 2, the most practical approach is to use Wasmtime's
`subscribe` mechanism and custom `WasiView` implementation that checks policies
before delegating to the underlying WASI implementation.

```rust
// crates/runtime/src/policy_wasi.rs
use crate::executor::StoreState;
use crate::policy_tracker::PolicyDenied;
use wasmtime_wasi::WasiView;
use wasmtime_wasi::p2::bindings::sockets::{
    network::Network,
    tcp::TcpSocket,
    udp::UdpSocket,
};
use wasmtime::AsContextMut;

/// Policy-aware wrapper around WASI network operations.
///
/// This intercepts socket creation and data send operations to enforce
/// the per-instance NetworkPolicy. The interception happens at the
/// WasiView level by checking the PolicyEnforcer in StoreState.
///
/// Implementation strategy:
/// - Socket creation (tcp_connect, udp_connect): Check before creating
/// - Socket send (send_to, send): Check egress limits before sending
/// - Socket bind: Check allowed_bind_ports before binding
/// - DNS resolution: Check allow_dns before resolving

/// Check outbound connection policy before a TCP connect.
/// Called from the Supervisor's spawn_blocking wrapper around the Wasm module.
pub fn check_tcp_connect_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    dest_ip: std::net::IpAddr,
    dest_port: u16,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_outbound_tcp_connect(dest_ip, dest_port)
}

/// Record a successful TCP connection.
pub fn record_tcp_connect(store: &mut impl AsContextMut<Data = StoreState>) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_outbound_connect();
}

/// Record a TCP disconnection.
pub fn record_tcp_disconnect(store: &mut impl AsContextMut<Data = StoreState>) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_outbound_disconnect();
}

/// Check egress policy before sending data.
pub fn check_egress_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_egress(bytes)
}

/// Record egress bytes after a successful send.
pub fn record_egress(store: &mut impl AsContextMut<Data = StoreState>, bytes: u64) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_egress(bytes);
}

/// Check DNS policy before a name lookup.
pub fn check_dns_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_dns_lookup()
}

/// Check bind policy before binding to a port.
pub fn check_bind_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    port: u16,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_bind(port)
}

/// Check FD open policy before opening a file.
pub fn check_fd_open_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_fd_open()
}

/// Record an FD open.
pub fn record_fd_open(store: &mut impl AsContextMut<Data = StoreState>) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_fd_open();
}

/// Record an FD close.
pub fn record_fd_close(store: &mut impl AsContextMut<Data = StoreState>) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_fd_close();
}

/// Check filesystem write policy.
pub fn check_fs_write_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let state = store.as_context_mut().data_mut();
    state.policy.check_fs_write(bytes)
}

/// Record filesystem write bytes.
pub fn record_fs_write(store: &mut impl AsContextMut<Data = StoreState>, bytes: u64) {
    let state = store.as_context_mut().data_mut();
    state.policy.record_fs_write(bytes);
}
```

### Wasmtime Socket Interception via WasiCtxBuilder Hooks

Wasmtime 43.x supports custom `IoChannel` implementations. We use this to wrap
the standard TCP/UDP socket implementations with policy checks:

```rust
// crates/runtime/src/policy_socket.rs
use crate::policy_tracker::PolicyDenied;
use std::io;
use std::sync::Arc;

/// A wrapper around a TCP stream that enforces egress limits.
/// Every `write()` call checks the egress policy before delegating
/// to the underlying stream.
pub struct PolicyTcpStream {
    inner: tokio::net::TcpStream,
    enforcer: Arc<std::sync::Mutex<crate::policy_tracker::PolicyEnforcer>>,
}

impl PolicyTcpStream {
    pub fn new(
        inner: tokio::net::TcpStream,
        enforcer: Arc<std::sync::Mutex<crate::policy_tracker::PolicyEnforcer>>,
    ) -> Self {
        PolicyTcpStream { inner, enforcer }
    }

    pub async fn write_with_policy(&self, buf: &[u8]) -> io::Result<usize> {
        let enforcer = self.enforcer.lock().unwrap();
        enforcer.check_egress(buf.len() as u64)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
        drop(enforcer);

        // Delegate to the real stream
        use tokio::io::AsyncWriteExt;
        let mut stream = self.inner.clone();
        let n = stream.write(buf).await?;

        let enforcer = self.enforcer.lock().unwrap();
        enforcer.record_egress(n as u64);
        Ok(n)
    }
}
```

---

## 5. AppConfig Extension for Policy

The `AppConfig` struct gains a `policy` field that operators use to specify
per-app policy overrides.

```rust
// crates/common/src/types.rs — addition to AppConfig

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    // ... existing fields ...

    /// Security and resource policy for this app.
    /// None = use platform defaults.
    #[serde(default)]
    pub policy: Option<common::policy::PolicyConfig>,
}

impl AppConfig {
    pub fn default_for(app_id: AppId) -> Self {
        AppConfig {
            // ... existing defaults ...
            policy: None,
        }
    }
}
```

### Deploy Manifest Example

```toml
# Deploy manifest for a database-backed API
[app]
id = "api-users:v2"
fuel_quota = 500_000_000
memory_pages = 2048
max_instances = 10
idle_timeout_secs = 300
wasm_bind_port = 8080

[app.policy.network]
allow_outbound_tcp = true
allow_dns = true
max_outbound_connections = 20
allowed_cidrs = ["10.0.0.0/8"]       # Only internal network
denied_cidrs = ["169.254.169.254/32"] # Block cloud metadata service
max_egress_bytes = 1073741824         # 1 GB

[app.policy.filesystem]
max_open_fds = 32
max_fs_write_bytes = 10485760         # 10 MB
allow_file_create = false
allow_file_delete = false
```

```toml
# Deploy manifest for a static site generator (no network needed)
[app]
id = "static-gen:v1"
fuel_quota = 100_000_000
memory_pages = 512
max_instances = 2
wasm_bind_port = 8080

[app.policy.network]
allow_outbound_tcp = false
allow_dns = false
allow_inbound = true
max_outbound_connections = 0

[app.policy.filesystem]
max_open_fds = 16
max_fs_write_bytes = 52428800         # 50 MB
allow_file_create = true
allow_file_delete = false
allowed_paths = ["/tmp/output"]
```

---

## 6. Policy Violation Metrics

```rust
// crates/runtime/src/policy_metrics.rs
use prometheus::{IntCounter, IntGauge, Opts, Registry};

pub struct PolicyMetrics {
    /// Total outbound connections denied by policy.
    pub connection_denied_total: IntCounter,

    /// Total egress operations denied by policy.
    pub egress_denied_total: IntCounter,

    /// Total FD open operations denied by policy.
    pub fd_denied_total: IntCounter,

    /// Total filesystem write operations denied by policy.
    pub fs_write_denied_total: IntCounter,

    /// Total bind operations denied by policy.
    pub bind_denied_total: IntCounter,

    /// Total DNS lookups denied by policy.
    pub dns_denied_total: IntCounter,

    /// Current active outbound connections across all instances.
    pub active_outbound_connections: IntGauge,

    /// Current open FDs across all instances.
    pub open_fds: IntGauge,
}

impl PolicyMetrics {
    pub fn new(registry: &Registry) -> Self {
        let connection_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_connection_denied_total",
            "Outbound connections denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(connection_denied_total.clone())).unwrap();

        let egress_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_egress_denied_total",
            "Egress operations denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(egress_denied_total.clone())).unwrap();

        let fd_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_fd_denied_total",
            "FD open operations denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(fd_denied_total.clone())).unwrap();

        let fs_write_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_fs_write_denied_total",
            "Filesystem write operations denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(fs_write_denied_total.clone())).unwrap();

        let bind_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_bind_denied_total",
            "Bind operations denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(bind_denied_total.clone())).unwrap();

        let dns_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_dns_denied_total",
            "DNS lookups denied by WASI policy",
        )).unwrap();
        registry.register(Box::new(dns_denied_total.clone())).unwrap();

        let active_outbound_connections = IntGauge::with_opts(Opts::new(
            "wasm_policy_active_outbound_connections",
            "Current active outbound connections across all instances",
        )).unwrap();
        registry.register(Box::new(active_outbound_connections.clone())).unwrap();

        let open_fds = IntGauge::with_opts(Opts::new(
            "wasm_policy_open_fds",
            "Current open file descriptors across all instances",
        )).unwrap();
        registry.register(Box::new(open_fds.clone())).unwrap();

        PolicyMetrics {
            connection_denied_total,
            egress_denied_total,
            fd_denied_total,
            fs_write_denied_total,
            bind_denied_total,
            dns_denied_total,
            active_outbound_connections,
            open_fds,
        }
    }
}
```

### Prometheus Alerting Rules

```yaml
groups:
  - name: wasi_policy
    rules:
      - alert: WasiPolicyConnectionDenied
        expr: rate(wasm_policy_connection_denied_total[5m]) > 10
        for: 2m
        annotations:
          summary: "High rate of denied outbound connections on {{ $labels.node }}"
          description: "App is attempting connections that violate its network policy."

      - alert: WasiPolicyFdExhaustion
        expr: rate(wasm_policy_fd_denied_total[5m]) > 5
        for: 2m
        annotations:
          summary: "FD limit being hit frequently on {{ $labels.node }}"
          description: "An app is trying to open more FDs than its policy allows."

      - alert: WasiPolicyEgressDenied
        expr: rate(wasm_policy_egress_denied_total[5m]) > 5
        for: 2m
        annotations:
          summary: "Egress limit being hit on {{ $labels.node }}"
          description: "An app is trying to send more data than its egress policy allows."
```

---

## 7. Integration with eBPF Monitor (Step 30)

The WASI policy enforcer and the eBPF syscall monitor form a two-layer defense:

```
Layer 1: WASI Host Functions (this step)
  - Checks BEFORE the operation
  - Returns EACCES/EMFILE to the Wasm module
  - App can handle the error gracefully
  - Covers: socket(), connect(), bind(), write(), open()

Layer 2: eBPF Syscall Monitor (Step 30)
  - Observes AFTER the operation
  - Detects violations that bypassed WASI (hypothetical Wasmtime bug)
  - Kills the instance and logs SECURITY alert
  - Covers: raw_syscalls/sys_enter for privileged syscalls
```

### Coordination Protocol

The eBPF monitor's `MONITORED_PIDS` map is populated by the Supervisor when it
spawns instances. The WASI policy enforcer's counters are read by the eBPF
metrics exporter to provide a unified view:

```rust
// In the Supervisor's spawn method, after creating the instance:
pub async fn spawn(&self, app_id: &AppId) -> Result<SocketAddr, PlatformError> {
    // ... existing spawn logic ...

    // Register the instance PID with the eBPF syscall monitor
    let pid = std::process::id();
    let tid = instance.task.id(); // Tokio task ID (approximation)
    // The eBPF monitor uses cgroup ID for scoping, not PID directly.
    // But we can update the MONITORED_PIDS map for the syscall counter.

    // Share the PolicyEnforcer counters with the metrics exporter
    if let Some(ref policy_metrics) = self.policy_metrics {
        policy_metrics.active_outbound_connections.add(
            state.policy.counters.outbound_connections_active.load(Ordering::Relaxed) as i64
        );
    }

    // ... return the instance address ...
}
```

### What eBPF Catches That WASI Doesn't

| Scenario | WASI Detection | eBPF Detection |
|----------|---------------|----------------|
| App opens too many sockets | ✅ `check_outbound_tcp_connect()` | ✅ `fd_install` kprobe |
| App sends too much data | ✅ `check_egress()` | ✅ `tcp_monitor` byte count |
| App connects to denied IP | ✅ `check_outbound_tcp_connect()` | ✅ `tcp_monitor` dest IP check |
| App makes `ptrace` syscall | ❌ Not a WASI operation | ✅ `syscall_counter` |
| App makes `bpf` syscall | ❌ Not a WASI operation | ✅ `syscall_counter` |
| App forks a process | ❌ Not a WASI operation | ✅ `sched_process_exec` |
| Wasmtime bug allows raw syscall | ❌ Bypasses WASI entirely | ✅ `raw_syscalls/sys_enter` |

---

## 8. Default Policy Profiles

Instead of requiring operators to specify every policy field, the platform provides
named profiles that set sensible defaults for common app types.

```rust
// crates/common/src/policy_profiles.rs
use crate::policy::{PolicyConfig, NetworkPolicyConfig, FilesystemPolicyConfig};

/// Pre-defined policy profiles for common app types.
pub enum PolicyProfile {
    /// HTTP API server: needs inbound + outbound TCP, DNS, moderate limits.
    HttpApi,

    /// Background worker: needs outbound TCP, DNS, no inbound.
    BackgroundWorker,

    /// Static site: needs inbound only, no outbound, no filesystem.
    StaticSite,

    /// Database proxy: needs inbound + outbound TCP, high connection limit.
    DatabaseProxy,

    /// Unrestricted: all limits disabled (for trusted internal tools only).
    Unrestricted,
}

impl PolicyProfile {
    pub fn to_config(&self) -> PolicyConfig {
        match self {
            PolicyProfile::HttpApi => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: Some(vec!["169.254.169.254/32".to_string()]),
                    max_outbound_connections: Some(50),
                    max_egress_bytes: None,
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(64),
                    max_fs_write_bytes: Some(50 * 1024 * 1024),
                    max_fs_read_bytes: None,
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: None,
                }),
            },
            PolicyProfile::BackgroundWorker => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: Some(vec!["169.254.169.254/32".to_string()]),
                    max_outbound_connections: Some(20),
                    max_egress_bytes: Some(500 * 1024 * 1024), // 500 MB
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(32),
                    max_fs_write_bytes: Some(100 * 1024 * 1024), // 100 MB
                    max_fs_read_bytes: None,
                    allow_file_create: Some(true),
                    allow_file_delete: Some(false),
                    allowed_paths: Some(vec!["/tmp".to_string()]),
                }),
            },
            PolicyProfile::StaticSite => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(false),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(false),
                    allowed_cidrs: None,
                    denied_cidrs: None,
                    max_outbound_connections: Some(0),
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(16),
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: None,
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: None,
                }),
            },
            PolicyProfile::DatabaseProxy => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: Some(vec!["169.254.169.254/32".to_string()]),
                    max_outbound_connections: Some(200),
                    max_egress_bytes: None,
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(128),
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: None,
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: None,
                }),
            },
            PolicyProfile::Unrestricted => PolicyConfig {
                network: None,  // All defaults = mostly open
                filesystem: None,
            },
        }
    }
}
```

### Profile Selection in AppConfig

```toml
# Quick profile selection (overrides individual policy fields)
[app]
id = "api-users:v2"
policy_profile = "http_api"

# Or explicit policy (overrides profile)
[app]
id = "api-users:v2"

[app.policy.network]
max_outbound_connections = 20
denied_cidrs = ["169.254.169.254/32", "10.99.0.0/16"]
```

---

## 9. Policy Violation Audit Logging

Every policy denial is logged to the audit system (Step 07) for forensic analysis.

```rust
// crates/supervisor/src/audit.rs — addition to AuditEventType

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    // ... existing variants ...
    PolicyViolation,
}

// When a policy denial occurs in the WASI host function:
fn log_policy_violation(app_id: &str, instance_id: &str, denied: &PolicyDenied) {
    let event = AuditEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        node_id: std::env::var("NODE_ID").unwrap_or_default(),
        event_type: AuditEventType::PolicyViolation,
        actor: "wasi_policy_enforcer".to_string(),
        app_id: app_id.to_string(),
        details: serde_json::json!({
            "instance_id": instance_id,
            "denial_reason": denied.to_string(),
            "denial_type": std::mem::discriminant(denied).debug_fmt(),
        }),
    };
    write_audit_event("/var/log/wasm-node/audit.jsonl", &event);

    tracing::warn!(
        app = app_id,
        instance = instance_id,
        reason = %denied,
        "WASI policy violation"
    );
}
```

---

## 10. CLI Commands

```
# View the effective policy for an app
wasm-ctl app policy api-users:v2
# Output:
# Network Policy:
#   outbound_tcp: allowed
#   outbound_udp: denied
#   dns: allowed
#   allowed_cidrs: (all)
#   denied_cidrs: 169.254.169.254/32
#   max_outbound_connections: 50
#   max_egress_bytes: unlimited
#   allowed_bind_ports: [10001]
#
# Filesystem Policy:
#   max_open_fds: 64
#   max_fs_write_bytes: 50 MB
#   allow_file_create: denied
#   allow_file_delete: denied
#   allowed_paths: (none)

# View policy violation metrics for an app
wasm-ctl app policy-violations api-users:v2
# Output:
# connection_denied: 0
# egress_denied: 3
# fd_denied: 0
# fs_write_denied: 0
# bind_denied: 0
# dns_denied: 0

# Apply a policy profile to an app
wasm-ctl deploy api-users:v2 --policy-profile http_api

# Apply explicit policy overrides
wasm-ctl deploy api-users:v2 \
  --policy-network-max-outbound-connections 20 \
  --policy-network-denied-cidrs "169.254.169.254/32,10.99.0.0/16" \
  --policy-fs-max-open-fds 32

# List available policy profiles
wasm-ctl policy-profiles
# Output:
# http_api          - HTTP API server (inbound + outbound TCP, DNS)
# background_worker - Background worker (outbound TCP, DNS, no inbound)
# static_site       - Static site (inbound only, no outbound)
# database_proxy    - Database proxy (high connection limit)
# unrestricted      - No limits (trusted internal tools only)
```

---

## 11. Testing Strategy

### Unit Tests

```bash
cargo test -p runtime --lib  # Policy enforcer logic
cargo test -p common --lib   # Policy config resolution
```

Tests to implement:
- `test_policy_default_resolve`: Default PolicyConfig resolves to expected limits
- `test_policy_cidr_allowed`: IP in allowed CIDR passes check
- `test_policy_cidr_denied`: IP in denied CIDR is rejected
- `test_policy_denied_overrides_allowed`: IP in both lists is denied
- `test_policy_connection_limit`: Connection beyond limit is denied
- `test_policy_egress_limit`: Egress beyond limit is denied
- `test_policy_fd_limit`: FD open beyond limit is denied
- `test_policy_fs_write_limit`: Write beyond limit is denied
- `test_policy_bind_port_allowed`: Binding to assigned port succeeds
- `test_policy_bind_port_denied`: Binding to unassigned port is denied
- `test_policy_dns_disabled`: DNS lookup when disabled is denied
- `test_policy_file_create_denied`: File creation when disabled is denied
- `test_policy_counters_atomic`: Counters are accurate under concurrent updates
- `test_policy_profile_http_api`: HttpApi profile has expected settings
- `test_policy_profile_static_site`: StaticSite profile blocks outbound

### Integration Tests

```bash
cargo test -p runtime --tests  # With real Wasmtime
```

Tests to implement:
- `test_wasm_outbound_connection_allowed`: Wasm app connects to allowed host
- `test_wasm_outbound_connection_denied`: Wasm app denied connection to blocked host
- `test_wasm_fd_limit_enforced`: Wasm app gets EMFILE when FD limit hit
- `test_wasm_egress_limit_enforced`: Wasm app gets error when egress limit hit
- `test_wasm_bind_only_assigned_port`: Wasm app can only bind its assigned port
- `test_wasm_dns_disabled`: Wasm app gets error when DNS disabled
- `test_wasm_policy_violation_logged`: Denial creates audit log entry

### E2E Tests

```bash
cargo test -p e2e -- --ignored --test-threads=1
```

Tests to implement:
- `test_policy_blocks_metadata_service`: Deploy app with denied_cidrs containing
  169.254.169.254/32, verify connection attempt fails from Wasm
- `test_policy_connection_limit_enforced`: Deploy app with max_outbound_connections=2,
  verify 3rd concurrent connection is denied
- `test_policy_egress_limit_enforced`: Deploy app with max_egress_bytes=1024,
  verify large response is truncated/denied
- `test_policy_profile_http_api`: Deploy with profile, verify expected behavior

---

## 12. Migration Path

### Phase 1: Policy Structures (Non-Breaking)

Add `PolicyConfig`, `InstancePolicy`, `PolicyEnforcer`, and `PolicyCounters` to the
codebase. Add the `policy` field to `AppConfig` with `#[serde(default)]`. Existing
deployments continue to work with no policy enforcement (all limits at their current
"unrestricted" defaults).

### Phase 2: Enforcement in WasiCtxBuilder

Replace the unconditional `allow_tcp(true)` / `allow_udp(true)` with policy-aware
configuration. Add the `PolicyEnforcer` to `StoreState`. Wire the policy checks
into the WASI host function wrappers.

### Phase 3: Default Policy Activation

Change the default `NetworkPolicy` from "allow everything" to the `HttpApi` profile.
This is a **breaking change** for apps that rely on unrestricted access. Operators
must explicitly set `policy_profile = "unrestricted"` for such apps.

### Phase 4: eBPF Coordination

Wire the `PolicyEnforcer` counters into the eBPF metrics exporter. Register instance
PIDs with the eBPF `MONITORED_PIDS` map. This completes the two-layer defense.

---

## 13. Security Considerations

### Policy Enforcement Is Not a Sandbox

WASI policy enforcement limits what a well-behaved Wasm module can do through the
WASI API. It does NOT protect against:
- A Wasmtime SFI bypass (hypothetical bug in Wasmtime's bounds checking)
- A host-level vulnerability (kernel exploit, library bug)
- Side-channel attacks (timing, cache)

For these, the platform relies on Wasm SFI (Layer 1), eBPF monitoring (Layer 8),
and the principle of least privilege (this step).

### CIDR Validation

The `allowed_cidrs` and `denied_cidrs` fields accept strings that are parsed as
`ipnet::IpNet`. Invalid CIDR strings are logged as warnings and ignored (treated
as "not matching"). This means a typo in a CIDR string silently reduces security.

**Fix**: Validate CIDR strings at deploy time (in `config_validator.rs`) and reject
the deployment if any CIDR is invalid.

### DNS Exfiltration

Even with `allow_dns: true`, a malicious app could exfiltrate data via DNS queries
(e.g., encoding secrets in subdomain labels: `secret-data.attacker.com`). The
`max_egress_bytes` limit partially mitigates this (DNS queries are small), but
a dedicated attacker could leak data slowly.

**Mitigation**: For high-security deployments, set `allow_dns: false` and provide
IP addresses directly in environment variables.

### Policy Bypass via Wasmtime Bugs

If Wasmtime has a bug that allows a Wasm module to call host functions outside the
WASI API, the policy enforcer is bypassed. The eBPF syscall monitor (Step 30) is
the safety net for this scenario — it detects raw syscalls from Wasm instance PIDs
and kills the instance.

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Policy Data Structures
- [ ] `NetworkPolicy` struct with all fields from Step 13
- [ ] `FilesystemPolicy` struct with FD, write, and path limits
- [ ] `InstancePolicy` combining network + filesystem
- [ ] `PolicyConfig` with optional overrides for AppConfig
- [ ] `PolicyConfig::resolve()` merges config with defaults + assigned port
- [ ] `PolicyProfile` enum with 5 pre-defined profiles
- [ ] `PolicyConfig` added to `AppConfig` with `#[serde(default)]`
- [ ] Schema migration for new `policy` field in AppConfig

### Policy Enforcer
- [ ] `PolicyEnforcer` with all check/record methods
- [ ] `PolicyCounters` with atomic counters for all metrics
- [ ] `PolicyDenied` enum with descriptive error messages
- [ ] CIDR matching via `ipnet` crate
- [ ] Denied CIDRs take precedence over allowed CIDRs
- [ ] All counters are atomic (no locks on hot path)

### WASI Integration
- [ ] `PolicyEnforcer` added to `StoreState` (replaces dead `IoResourceTracker`)
- [ ] `WasiCtxBuilder` configured based on `InstancePolicy`
- [ ] TCP only enabled if `allow_outbound_tcp` or `allow_inbound`
- [ ] UDP only enabled if `allow_outbound_udp`
- [ ] DNS only enabled if `allow_dns`
- [ ] Filesystem preopens limited to `allowed_paths`
- [ ] `spawn_instance()` accepts `PolicyConfig` parameter
- [ ] Policy resolved at spawn time with assigned port

### Host Function Wrappers
- [ ] `check_tcp_connect_policy()` before outbound connections
- [ ] `check_egress_policy()` before data sends
- [ ] `check_dns_policy()` before DNS lookups
- [ ] `check_bind_policy()` before port binding
- [ ] `check_fd_open_policy()` before file opens
- [ ] `check_fs_write_policy()` before filesystem writes
- [ ] `check_file_create_policy()` before file creation
- [ ] `check_file_delete_policy()` before file deletion
- [ ] All wrappers delegate to real WASI on success
- [ ] All wrappers return `EACCES`/`EMFILE` on denial

### Metrics
- [ ] `wasm_policy_connection_denied_total` counter
- [ ] `wasm_policy_egress_denied_total` counter
- [ ] `wasm_policy_fd_denied_total` counter
- [ ] `wasm_policy_fs_write_denied_total` counter
- [ ] `wasm_policy_bind_denied_total` counter
- [ ] `wasm_policy_dns_denied_total` counter
- [ ] `wasm_policy_active_outbound_connections` gauge
- [ ] `wasm_policy_open_fds` gauge
- [ ] Prometheus alerting rules for policy violations

### Audit Logging
- [ ] `PolicyViolation` added to `AuditEventType`
- [ ] Every denial logged with app_id, instance_id, and reason
- [ ] Denials logged at `warn` level via tracing
- [ ] Audit events written to audit log file

### eBPF Coordination
- [ ] Instance PIDs registered in eBPF `MONITORED_PIDS` map
- [ ] Policy counters exported to eBPF metrics pipeline
- [ ] Two-layer defense documented: WASI (prevent) + eBPF (detect)

### CLI
- [ ] `wasm-ctl app policy <app_id>` shows effective policy
- [ ] `wasm-ctl app policy-violations <app_id>` shows violation counts
- [ ] `wasm-ctl deploy --policy-profile <profile>` applies profile
- [ ] `wasm-ctl deploy --policy-network-*` flags for overrides
- [ ] `wasm-ctl policy-profiles` lists available profiles

### Validation
- [ ] CIDR strings validated at deploy time
- [ ] Invalid CIDRs cause deployment rejection
- [ ] `max_outbound_connections > 0` enforced
- [ ] `max_open_fds > 0` enforced
- [ ] `allowed_bind_ports` populated with assigned port

### Testing
- [ ] Unit tests for all policy check methods (15+ tests)
- [ ] Unit tests for policy config resolution
- [ ] Unit tests for policy profiles
- [ ] Integration tests with real Wasmtime (7+ tests)
- [ ] E2E test: metadata service access blocked
- [ ] E2E test: connection limit enforced
- [ ] E2E test: egress limit enforced
- [ ] E2E test: policy profile applied correctly

### Documentation
- [ ] Deploy manifest format updated with policy fields
- [ ] Policy profile descriptions documented
- [ ] CIDR syntax examples provided
- [ ] Migration guide: Phase 1 → Phase 2 → Phase 3
- [ ] Security considerations documented (DNS exfiltration, CIDR validation)
