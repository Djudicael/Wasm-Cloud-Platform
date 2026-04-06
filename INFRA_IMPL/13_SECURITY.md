# Step 13 — Security Model & Multi-Tenancy Isolation

## Goal
Document and implement the security guarantees of the platform. Security must be enforced
at multiple layers: Wasm sandboxing, network isolation, secret handling, and node authentication.

---

## Context & Rationale

### The Problem This Solves

Multi-tenancy means running untrusted code from multiple tenants on the same hardware.
The security model must answer: **what can Tenant A's code do to Tenant B's data and
to the platform itself?**

This step defines the threat model and implements the defenses that make it safe to run
untrusted Wasm binaries on shared infrastructure.

### Why Wasm is a Better Isolation Boundary Than Containers

Container isolation relies on:
1. Linux kernel namespaces (process, network, mount)
2. cgroups (resource limits)
3. seccomp (syscall filtering)

These are all kernel-level mechanisms. A kernel exploit can bypass all of them. The
assumption "the kernel is trusted" breaks badly when the attacker controls the workload.

Wasm isolation relies on **Software Fault Isolation (SFI)**:
- The Wasm binary is compiled to native code by Wasmer with inserted memory boundary checks
- Every memory access is verified at the CPU instruction level: if address is outside
  the module's linear memory, it triggers a trap (not a segfault that could be exploited)
- No kernel involvement: the isolation is in the compiled machine code itself

An attacker inside a Wasm module would need to find a bug in Wasmer's SFI implementation
(a very narrow attack surface, written in Rust) rather than a kernel syscall that bypasses
namespaces.

### Defense in Depth: Why Multiple Layers?

No single security mechanism is perfect. The platform defends with multiple independent
layers so that a failure in one layer does not compromise the whole:

```
Layer 1: Wasm SFI       — prevents memory escape
Layer 2: Fuel metering  — prevents CPU exhaustion (infinite loops)
Layer 3: WASI network   — prevents unauthorized port binding and outbound connections
Layer 4: Secrets DEK    — ensures disk theft yields only ciphertext
Layer 5: NATS creds     — prevents unauthorized deploy commands
Layer 6: SHA-256 verify — prevents binary substitution between upload and compile
Layer 7: Audit log      — forensic trail if any layer is bypassed
```

An attacker who somehow bypasses Layer 1 (Wasm SFI) still cannot send unauthorized
commands to the cluster without valid NATS credentials (Layer 5).

### The Binary Substitution Attack

Without hash verification, the deployment flow is vulnerable:

```
1. Operator uploads api-users.wasm to Node-0's artifact server
2. NATS event: "deploy api-users with sha256=abc123"
3. [Attacker intercepts, replaces the binary on Node-0's disk]
4. Nodes compile the tampered binary
```

The SHA-256 check in `handle_deploy()` prevents step 4 from succeeding silently. If the
binary on disk doesn't match the hash in the NATS event (which the operator signed), the
node logs a `SECURITY` error and refuses to compile.

### Why Not Use Capability-Based Security for Wasm (WASI)?

WASI is already capability-based: a Wasm module can only access resources that are
explicitly given to it via the `WasiEnv`. It has no ambient authority.

The additional network policy (`NetworkPolicy` struct) extends this: even within what
WASI allows, the Supervisor further restricts which destinations can be contacted. This
catches misconfigured apps (e.g., an app that accidentally tries to connect to an
internal metadata service) and prevents certain exfiltration attacks.

### NATS ACLs: Why Nodes Cannot Forge Each Other

Each node's NATS credentials permit:
- **Publish**: only to `instance.ready.<own-node-id>.*`, `instance.dead.<own-node-id>.*`,
  `node.load.<own-node-id>`
- **Subscribe**: to `deploy.>`, `secrets.update.>`, `config.update.>`

A compromised node cannot:
- Publish `instance.ready` events on behalf of another node (node-id is in the subject)
- Publish deploy commands (no publish permission to `deploy.>`)
- Subscribe to admin-only subjects

This prevents a single compromised node from being used to push malicious binaries
to the rest of the cluster.

---

---

## 1. Threat Model

| Threat | Mitigation | Layer |
|--------|-----------|-------|
| Malicious Wasm code (infinite loop) | Fuel metering → process killed, not just throttled | Wasmer |
| Memory overflow | Linear memory limit via Tunables | Wasmer |
| App A reads App B's data | Separate Stores (no shared memory unless explicitly granted) | Wasmer |
| App A binds arbitrary ports | Pre-bound sockets via WASI; app only gets its assigned fd | WASI |
| App A calls external malicious host | Outbound connections restricted via WASI config | WASI |
| Secret leakage between apps | Per-app DEK encryption; secrets never written to disk unencrypted | Secrets layer |
| Secret leakage via logs | Supervisor never logs raw secret values | Application policy |
| Unauthorized deploy | NATS credentials + JetStream ACLs | NATS |
| Node impersonation | Node mutual TLS (mTLS) for NATS connections | NATS |
| Disk theft | All secrets at rest are AES-256-GCM encrypted | redb |
| Binary substitution attack | SHA-256 hash of `.wasm` verified before compilation | Deployment |

---

## 2. Wasm Software Fault Isolation (SFI)

Wasmer enforces SFI automatically. Each module gets:
- Its own linear memory (no access to host memory or other modules' memory)
- Its own call stack
- A restricted set of imported host functions (only what the Supervisor explicitly provides)

**What a malicious Wasm module CAN do:**
- Consume CPU (mitigated by Fuel)
- Allocate memory (mitigated by memory limits)
- Make network calls to addresses allowed by the WASI config

**What a malicious Wasm module CANNOT do:**
- Read host memory
- Access the filesystem unless explicitly preopen'ed
- Call any host function not in the import object
- Escape the sandbox

---

## 3. Secure Wasm Import Object

Only expose the minimum required host functions to the Wasm module.

```rust
// crates/runtime/src/executor.rs (security-hardened import object)
use wasmer::{imports, Function, Store};
use wasmer_wasix::WasiEnv;

pub fn build_minimal_imports(
    store: &mut Store,
    wasi_env: &WasiEnv,
    module: &wasmer::Module,
) -> Result<wasmer::Imports, common::error::PlatformError> {
    // Start with the WASI-provided imports (file I/O, clocks, random, network)
    let mut import_object = wasi_env.import_object(store, module)
        .map_err(|e| common::error::PlatformError::Runtime(e.to_string()))?;

    // Optionally expose a custom host function for metrics (read-only):
    // import_object.define("env", "report_metric", Function::new_typed(store, |fuel: u64| {
    //     tracing::debug!("wasm reported metric: fuel={}", fuel);
    // }));

    Ok(import_object)
}
```

---

## 4. Network Policy per App

```rust
// crates/runtime/src/wasi.rs (extended security policy)
use wasmer_wasix::WasiEnvBuilder;

pub struct NetworkPolicy {
    /// Allow the module to open outbound TCP connections.
    pub allow_outbound_tcp: bool,
    /// Allow the module to open outbound UDP.
    pub allow_outbound_udp: bool,
    /// Allowed outbound CIDRs (empty = all if allow_outbound_tcp = true).
    pub allowed_cidrs: Vec<std::net::IpNetwork>,
    /// Maximum number of concurrent outbound connections.
    pub max_connections: usize,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy {
            allow_outbound_tcp: true,  // needed for DB calls
            allow_outbound_udp: false,
            allowed_cidrs: vec![],     // empty = unrestricted
            max_connections: 100,
        }
    }
}

pub fn apply_network_policy(builder: WasiEnvBuilder, policy: &NetworkPolicy) -> WasiEnvBuilder {
    if policy.allow_outbound_tcp {
        builder.allow_connect(true)
    } else {
        builder.allow_connect(false)
    }
    // Future: wasmer-wasix may support CIDR filtering directly
}
```

---

## 5. Binary Integrity Verification

Before compiling a deployed binary, the Supervisor verifies its SHA-256 hash.

```rust
// crates/messaging/src/handlers.rs (deploy handler, with hash check)
use sha2::{Sha256, Digest};

async fn handle_deploy(
    app_id: AppId,
    config: AppConfig,
    wasm_bytes: Vec<u8>,
    expected_hash: Option<String>, // hex-encoded SHA-256 from deploy manifest
) {
    // Verify integrity
    if let Some(expected) = expected_hash {
        let mut hasher = Sha256::new();
        hasher.update(&wasm_bytes);
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected {
            tracing::error!(
                app = %app_id.0,
                expected,
                actual,
                "SECURITY: Wasm binary hash mismatch! Rejecting deploy."
            );
            return;
        }
    }
    // Proceed with normal deploy...
}
```

---

## 6. NATS Authentication & Authorization

All NATS connections use credentials. Deploy commands require admin-level creds.

```toml
# /etc/wasm-node/nats_credentials.conf
# Generated by nsc (NATS Security Credentials tool)
# Each node gets its own credential file with limited permissions:
# - Publish: instance.ready.*, instance.dead.*, node.load.*
# - Subscribe: deploy.>, secrets.update.>, config.update.>

[nats]
credentials_file = "/etc/wasm-node/node.creds"
tls_ca_file      = "/etc/wasm-node/nats-ca.pem"
```

```rust
// crates/messaging/src/lib.rs (authenticated connect)
use async_nats::ConnectOptions;

impl NatsBus {
    pub async fn connect_secure(url: &str, creds_path: &str) -> Result<Self, common::error::PlatformError> {
        let client = ConnectOptions::with_credentials_file(creds_path.into())
            .await
            .map_err(|e| common::error::PlatformError::Messaging(e.to_string()))?
            .connect(url)
            .await
            .map_err(|e| common::error::PlatformError::Messaging(format!("NATS connect: {e}")))?;
        Ok(NatsBus { client })
    }
}
```

---

## 7. Secrets Never Touch Disk Unencrypted

Security invariants enforced by the `secrets` crate:

```rust
// Invariant 1: SymmetricKey is zeroed on drop (zeroize crate)
// Invariant 2: Plaintext secrets only exist in process memory during spawn
// Invariant 3: redb only ever holds encrypted blobs
// Invariant 4: Env vars are in WasiEnv (process memory), not written to disk

// Audit checklist:
// ✓ No tracing::info!() calls that could log secret values
// ✓ No serde Serialize on SymmetricKey (no accidental JSON logging)
// ✓ AppSecretBundle.secrets values are always Vec<u8> (encrypted), never String
```

---

## 8. Resource Exhaustion Protections

```rust
// crates/supervisor/src/lib.rs
impl Supervisor {
    fn check_resource_limits(&self, config: &AppConfig) -> Result<(), common::error::PlatformError> {
        // Maximum fuel quota: 10 billion units (prevents absurdly long compute)
        if config.fuel_quota.0 > 10_000_000_000 {
            return Err(common::error::PlatformError::Runtime(
                "fuel_quota exceeds maximum allowed (10B units)".into()
            ));
        }

        // Maximum memory: 512 MB (8192 pages)
        if config.memory_limit.0 > 8192 {
            return Err(common::error::PlatformError::Runtime(
                "memory_limit exceeds maximum allowed (512 MB)".into()
            ));
        }

        // Maximum concurrent instances per app per node: 100
        if config.max_instances > 100 {
            return Err(common::error::PlatformError::Runtime(
                "max_instances exceeds node limit (100)".into()
            ));
        }

        Ok(())
    }
}
```

---

## 9. Audit Log

Security-relevant events should be written to a separate, append-only audit log.

```rust
// crates/supervisor/src/audit.rs
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: String,
    pub node_id: String,
    pub event_type: AuditEventType,
    pub actor: String,       // "nats-msg", "admin-api", "scheduler"
    pub app_id: String,
    pub details: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    AppDeployed,
    AppRemoved,
    InstanceSpawned,
    InstanceKilled,
    SecretRotated,
    TrapOccurred,
    BinaryHashMismatch,
    RateLimitExceeded,
}

pub fn write_audit_event(path: &str, event: &AuditEvent) {
    let line = serde_json::to_string(event).unwrap() + "\n";
    if let Ok(mut file) = OpenOptions::new().append(true).create(true).open(path) {
        file.write_all(line.as_bytes()).ok();
    }
}
```

---

## 10. Security Checklist (Pre-Production)

```
[ ] TLS enabled on Pingora (HTTPS) with valid certificate
[ ] NATS using credentials (not anonymous)
[ ] NATS TLS enabled (no plaintext messaging)
[ ] Node master key sourced from file with chmod 600, not environment variable
[ ] redb file has permissions 600 (only the node process can read)
[ ] Audit log enabled and shipped to SIEM
[ ] Wasm binaries verified by SHA-256 hash on deploy
[ ] Network policy: outbound connections restricted to known CIDR ranges
[ ] Rate limiting enabled on Pingora (per-app and global)
[ ] Admin API (port 9090) not exposed to the internet (firewall rule)
[ ] Fuel and memory limits set for all apps (no unlimited quotas)
[ ] Node process runs as a non-root user (systemd: User=wasm-node)
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Wasm Sandbox (SFI)
- [ ] A Wasm module that calls a host function not in the import object receives a link error — not a crash
- [ ] A Wasm module cannot read memory outside its own linear memory (verified by attempting out-of-bounds access)
- [ ] Two simultaneously running instances of different apps share no memory (verified by checking separate `Store` objects)
- [ ] A trap (out-of-fuel, OOM, illegal instruction) is caught and reported — it never kills the node process

### Network Policy
- [ ] An app with `allow_connect(false)` cannot open an outbound TCP connection
- [ ] An app cannot bind on any port other than the one the Supervisor pre-assigned
- [ ] The admin API (port 9090) and artifact server (port 9091) refuse connections from outside the configured bind address

### Binary Integrity
- [ ] `handle_deploy()` rejects a binary whose SHA-256 does not match `expected_hash`
- [ ] The rejection logs a `SECURITY` warning with both expected and actual hashes
- [ ] A corrupted artifact (bit flip) also fails the hash check

### Resource Limits
- [ ] `check_resource_limits()` rejects configs with `fuel_quota > 10_000_000_000`
- [ ] `check_resource_limits()` rejects configs with `memory_limit > 8192 pages`
- [ ] `check_resource_limits()` rejects configs with `max_instances > 100`
- [ ] All rejections return a descriptive `PlatformError` — not a panic

### Secrets at Rest
- [ ] Inspecting the raw redb file with a hex editor shows no plaintext secret values
- [ ] The node master key is never written to any log file or tracing span
- [ ] `SymmetricKey` values are not visible in stack traces or panic messages

### Audit Log
- [ ] Every `AppDeployed`, `InstanceKilled`, `TrapOccurred`, and `BinaryHashMismatch` event writes a line to the audit log
- [ ] The audit log is append-only (never overwritten or truncated on restart)
- [ ] Each audit entry contains `timestamp`, `node_id`, `app_id`, and `event_type`

### NATS Auth
- [ ] A connection attempt with invalid credentials is rejected by NATS
- [ ] A node can only publish to its allowed subjects (cannot forge another node's identity)

### Tests
- [ ] A test verifies that a Wasm module calling a forbidden host function receives a trap
- [ ] A test verifies that a binary with a wrong hash is rejected at deploy time
- [ ] A test verifies that resource limit violations return the correct error messages
