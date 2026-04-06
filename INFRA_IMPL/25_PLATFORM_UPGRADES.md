# Step 25 — Platform Binary Upgrades & Protocol Versioning

## Goal
Define the strategy for upgrading the platform binary (`wasm-node`) across a running cluster.
The system must:
- Perform rolling upgrades without dropping traffic
- Maintain protocol compatibility between nodes running different versions
- Version NATS message schemas so old and new nodes can coexist during the upgrade window
- Provide a rollback path if the new binary is broken
- Require no external orchestrator (no Kubernetes, no Ansible)

---

## Context & Rationale

### The Problem This Solves

Step 22 covers `redb` schema versioning — how the storage layer evolves. But the platform
binary itself also evolves: new features, bug fixes, performance improvements. In a cluster
with 10 nodes, upgrading all 10 simultaneously means 100% downtime. Upgrading one at a time
(rolling upgrade) requires that the upgraded node can coexist with the old nodes.

This coexistence has three dimensions:
1. **NATS messages**: Node-0 (v2) publishes a `DeployApp` event with a new field. Node-1
   (v1) receives it and must not crash on the unknown field.
2. **HTTP admin API**: If the admin API adds a new endpoint in v2, the CLI must still work
   against v1 nodes.
3. **redb format**: Covered by step 22 (schema migrations). Not repeated here.

Without protocol versioning, a rolling upgrade is a gamble: one incompatible NATS message
crashes all old nodes in the cluster.

### Why Semantic Versioning for the Protocol (Not the Binary)

The binary version (`wasm-node 0.4.0`) follows semver for release management. But the
**protocol version** is a separate integer that tracks wire-format compatibility:

```
Binary version:   0.3.0 → 0.4.0 → 0.5.0 → 1.0.0
Protocol version:    1  →    1   →    2   →    2
```

Protocol version increments only when the NATS message format or HTTP admin API introduces
a **breaking change**. Most binary versions don't change the protocol — they add features,
fix bugs, or optimize internals.

The rule: **a node must be able to communicate with any other node that shares its protocol
version ± 1**. This gives operators a one-version upgrade window: upgrade from protocol 1
to protocol 2, but not from 1 to 3 in one step.

### The Rolling Upgrade Sequence

```
Cluster: [Node-0:v1, Node-1:v1, Node-2:v1]
All nodes on protocol version 1.

Step 1: Upgrade Node-0
  - `wasm-ctl node upgrade node-0 --binary ./wasm-node-v2`
  - Node-0 receives a NATS event: Event::NodeUpgrade { binary_url, sha256 }
  - Node-0 downloads the new binary to /opt/wasm-node/wasm-node-v2
  - Node-0 verifies SHA-256
  - Node-0 begins graceful shutdown (step 20):
    a. NATS: publish Event::NodeDraining { node_id: "node-0" }
    b. Pingora stops accepting new connections
    c. In-flight requests drain (30s timeout)
    d. Process exits
  - systemd restarts the service with the new binary
  - Node-0 boots as v2 (protocol version 2)
  - Node-0 uses backward-compatible NATS messages (see section 2)

Cluster: [Node-0:v2, Node-1:v1, Node-2:v1]
Mixed protocol versions. All nodes communicate successfully.

Step 2: Upgrade Node-1 (same process)

Cluster: [Node-0:v2, Node-1:v2, Node-2:v1]

Step 3: Upgrade Node-2 (same process)

Cluster: [Node-0:v2, Node-1:v2, Node-2:v2]
All nodes on protocol version 2. Upgrade complete.
```

### Why Not Blue-Green at the Cluster Level?

Blue-green for Wasm instances (step 10) works because instances are lightweight (< 10ms
spawn). Blue-green at the **cluster level** would mean standing up an entirely separate
3-node cluster, migrating DNS, then tearing down the old cluster. This requires:
- 2× the hardware during the transition
- DNS propagation delays (TTL-dependent, can take minutes)
- State migration between clusters (redb data, NATS streams)

Rolling upgrades avoid all of this by upgrading in-place, one node at a time.

### Rollback: The Previous Binary Stays on Disk

The upgrade process downloads the new binary alongside the old one — it does not replace it:

```
/opt/wasm-node/
├── wasm-node-v1    ← previous binary (kept)
├── wasm-node-v2    ← new binary (active)
└── wasm-node       ← symlink → wasm-node-v2
```

Rollback is: `ln -sf wasm-node-v1 wasm-node && systemctl restart wasm-node`. The node
restarts with the old binary. Since `redb` schema migrations are forward-only (step 22),
the old binary must be able to read any new tables it doesn't understand (it ignores them).
This is why step 22 uses additive-only migrations: new tables are added, old tables are
never modified in incompatible ways.

---

---

## 1. Protocol Version Tracking

Every NATS message includes a protocol version field. Receivers use it to decide how to
parse the payload.

```rust
// crates/common/src/protocol.rs
use serde::{Deserialize, Serialize};

/// Current protocol version of this binary.
/// Increment when NATS message format introduces a breaking change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Minimum protocol version this binary can communicate with.
/// Nodes running a version below this are incompatible and should be upgraded first.
pub const MIN_COMPATIBLE_PROTOCOL: u32 = 1;

/// Every NATS message is wrapped in this envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope<T> {
    /// Protocol version of the sender.
    pub protocol_version: u32,

    /// Node ID of the sender.
    pub sender: String,

    /// Timestamp (milliseconds since UNIX epoch).
    pub timestamp_ms: u64,

    /// The actual event payload.
    pub payload: T,
}

impl<T: Serialize> MessageEnvelope<T> {
    pub fn new(sender: &str, payload: T) -> Self {
        MessageEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sender: sender.to_string(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            payload,
        }
    }
}
```

---

## 2. Backward-Compatible NATS Message Parsing

When a v2 node adds a new field to `DeployApp`, v1 nodes must not crash. The solution:
**always use `#[serde(default)]` on new fields** and deserialize with `serde_json::from_slice`
which ignores unknown fields by default.

```rust
// crates/messaging/src/events.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    DeployApp {
        app_id: String,
        wasm_sha256: String,
        config_json: String,

        /// Added in protocol version 2.
        /// v1 nodes deserialize this as None (serde default).
        #[serde(default)]
        rate_limit: Option<RateLimitConfig>,
    },

    // ... other events
}

// Rule for adding new fields:
//   1. Always use #[serde(default)] or Option<T>
//   2. Old code ignores the field (serde skips unknowns)
//   3. New code checks for the field and applies it if present
//   4. Never rename or remove existing fields in the same protocol version
```

### Breaking changes that require a protocol version bump:

```
Protocol version 1 → 2 (breaking change examples):
  - Renaming Event::DeployApp → Event::DeployApplication
  - Changing app_id from String to a struct AppId { name, version }
  - Removing a required field from an event

NOT breaking (no version bump needed):
  - Adding an Optional field with #[serde(default)]
  - Adding a new Event variant (unknown variants are logged and skipped)
  - Adding a new NATS subject (old nodes are simply not subscribed)
```

---

## 3. Version Compatibility Check on NodeJoined

When a new node joins (step 19), it includes its protocol version. Existing nodes
check compatibility before responding with a `StateSnapshot`.

```rust
// crates/messaging/src/events.rs (extended NodeJoined)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeJoined {
    pub node_id: String,
    pub public_key: Vec<u8>,
    pub protocol_version: u32,
    pub binary_version: String,  // e.g. "0.4.0"
}

// crates/messaging/src/handlers.rs
use common::protocol::{PROTOCOL_VERSION, MIN_COMPATIBLE_PROTOCOL};

pub fn handle_node_joined(event: &NodeJoined) -> Result<(), String> {
    // Check protocol compatibility
    if event.protocol_version < MIN_COMPATIBLE_PROTOCOL {
        return Err(format!(
            "node {} runs protocol v{}, minimum supported is v{}. \
             Upgrade the joining node first.",
            event.node_id, event.protocol_version, MIN_COMPATIBLE_PROTOCOL
        ));
    }

    if event.protocol_version > PROTOCOL_VERSION + 1 {
        return Err(format!(
            "node {} runs protocol v{}, this node runs v{}. \
             Upgrade this node first (max gap is 1).",
            event.node_id, event.protocol_version, PROTOCOL_VERSION
        ));
    }

    tracing::info!(
        node = %event.node_id,
        protocol = event.protocol_version,
        binary = %event.binary_version,
        "node joined with compatible protocol version"
    );
    Ok(())
}
```

---

## 4. Binary Distribution via Artifact Registry

The same HTTP artifact server (step 16) used for Wasm binaries serves platform binary
upgrades. The operator uploads the new `wasm-node` binary:

```
# Upload the new binary to the artifact registry
wasm-ctl platform upload --binary ./wasm-node-v2

# The artifact registry stores it at:
# GET /artifacts/platform/wasm-node-v2?sha256=<hash>
```

Nodes download from the artifact registry, not from NATS (binary is too large for
JetStream's ~1MB message limit).

```rust
// crates/node/src/upgrade.rs
use common::error::PlatformError;
use sha2::{Sha256, Digest};
use std::path::{Path, PathBuf};
use tokio::fs;

/// Download the new binary, verify its hash, and write it to disk.
pub async fn download_and_verify(
    artifact_url: &str,
    expected_sha256: &str,
    install_dir: &Path,
    binary_name: &str,
) -> Result<PathBuf, PlatformError> {
    // 1. Download the binary
    let response = reqwest::get(artifact_url).await
        .map_err(|e| PlatformError::Network(format!("download failed: {e}")))?;

    let bytes = response.bytes().await
        .map_err(|e| PlatformError::Network(format!("read body failed: {e}")))?;

    // 2. Verify SHA-256
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash != expected_sha256 {
        return Err(PlatformError::Security(format!(
            "SHA-256 mismatch: expected {}, got {}. Aborting upgrade.",
            expected_sha256, actual_hash
        )));
    }

    // 3. Write to install directory
    let dest = install_dir.join(binary_name);
    fs::write(&dest, &bytes).await
        .map_err(|e| PlatformError::Storage(format!("write binary failed: {e}")))?;

    // 4. Set executable permission (Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&dest, perms)
            .map_err(|e| PlatformError::Storage(format!("chmod failed: {e}")))?;
    }

    tracing::info!(path = %dest.display(), sha256 = %actual_hash, "binary verified and installed");
    Ok(dest)
}
```

---

## 5. Upgrade NATS Event

```rust
// crates/messaging/src/events.rs (new event type)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpgrade {
    /// Which node should upgrade. Use "*" for all nodes (rolling).
    pub target_node: String,

    /// URL to download the new binary from the artifact registry.
    pub binary_url: String,

    /// Expected SHA-256 hash of the new binary.
    pub binary_sha256: String,

    /// The new binary's protocol version. Used for compatibility checks.
    pub new_protocol_version: u32,

    /// The new binary version string (e.g. "0.5.0").
    pub new_binary_version: String,
}
```

---

## 6. Upgrade Orchestration (Rolling Strategy)

When `target_node = "*"`, nodes upgrade **sequentially** to avoid cluster outage.
The order is determined by sorted node IDs — the lexicographically first node upgrades
first, waits for confirmation, then the next node begins.

```rust
// crates/node/src/upgrade.rs (continued)
use messaging::events::Event;
use tokio::sync::mpsc;

/// Handle an incoming NodeUpgrade event.
pub async fn handle_upgrade_event(
    event: &NodeUpgrade,
    own_node_id: &str,
    cluster_node_ids: &[String],
    event_tx: &mpsc::Sender<Event>,
) -> Result<UpgradeAction, PlatformError> {
    // Check if this event targets us
    if event.target_node != "*" && event.target_node != own_node_id {
        return Ok(UpgradeAction::NotTargeted);
    }

    // For rolling upgrades (target = "*"), check if it's our turn
    if event.target_node == "*" {
        let mut sorted_nodes = cluster_node_ids.to_vec();
        sorted_nodes.sort();

        let my_position = sorted_nodes.iter().position(|id| id == own_node_id)
            .ok_or_else(|| PlatformError::Internal("own node not in cluster list".into()))?;

        if my_position > 0 {
            // Wait for the previous node to confirm its upgrade
            let previous = &sorted_nodes[my_position - 1];
            tracing::info!(
                waiting_for = %previous,
                position = my_position,
                "waiting for previous node to complete upgrade"
            );
            return Ok(UpgradeAction::WaitForPredecessor {
                predecessor: previous.clone(),
            });
        }
    }

    // Protocol compatibility check
    if event.new_protocol_version > common::protocol::PROTOCOL_VERSION + 1 {
        tracing::error!(
            current = common::protocol::PROTOCOL_VERSION,
            new = event.new_protocol_version,
            "protocol version gap too large — upgrade intermediate version first"
        );
        return Ok(UpgradeAction::IncompatibleVersion);
    }

    Ok(UpgradeAction::ProceedWithUpgrade)
}

pub enum UpgradeAction {
    NotTargeted,
    WaitForPredecessor { predecessor: String },
    IncompatibleVersion,
    ProceedWithUpgrade,
}
```

---

## 7. Upgrade Confirmation Event

After a node restarts with the new binary, it publishes a confirmation event so the
next node in the rolling sequence can begin its upgrade.

```rust
// crates/messaging/src/events.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpgradeComplete {
    pub node_id: String,
    pub new_binary_version: String,
    pub new_protocol_version: u32,
}
```

---

## 8. CLI Commands

```
# Upgrade a specific node
wasm-ctl platform upgrade \
  --node node-0 \
  --binary ./wasm-node-v2 \
  --sha256 abc123...

# Rolling upgrade: all nodes sequentially
wasm-ctl platform upgrade \
  --all \
  --binary ./wasm-node-v2 \
  --sha256 abc123...

# Check cluster version status
wasm-ctl platform status
# Output:
# node-0: binary=0.5.0 protocol=2 uptime=3h
# node-1: binary=0.4.0 protocol=1 uptime=12h  ← needs upgrade
# node-2: binary=0.5.0 protocol=2 uptime=1h

# Rollback a node to previous binary
wasm-ctl platform rollback --node node-0
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Binary Distribution
- [ ] `wasm-ctl platform upload` uploads a binary to the artifact registry
- [ ] Nodes download the binary via HTTP and verify SHA-256 before installing
- [ ] The old binary is preserved on disk (not overwritten) for rollback

### Rolling Upgrade
- [ ] Upgrading a single node causes zero traffic loss for apps running on other nodes
- [ ] During a rolling upgrade, NATS messages between v1 and v2 nodes are parsed without errors
- [ ] Nodes upgrade in sorted order (lexicographic by node_id)
- [ ] Each node waits for `NodeUpgradeComplete` from the previous node before starting its own upgrade

### Protocol Compatibility
- [ ] A v2 node can parse all v1 NATS messages without error
- [ ] A v1 node can parse all v2 NATS messages without error (unknown fields are ignored via serde default)
- [ ] A node refuses to join a cluster where the protocol version gap is > 1
- [ ] `PROTOCOL_VERSION` and `MIN_COMPATIBLE_PROTOCOL` are defined in `common/src/protocol.rs`

### Rollback
- [ ] `wasm-ctl platform rollback --node X` restarts the node with the previous binary
- [ ] The rolled-back node (protocol v1) continues to communicate with v2 nodes in the cluster
- [ ] redb data written by v2 is readable by v1 (additive-only schema changes)

### Observability
- [ ] `wasm-ctl platform status` shows binary version, protocol version, and uptime for every node
- [ ] A Prometheus metric `platform_binary_version{node, version}` is exposed for alerting on version drift
