# Step 27 — Disaster Recovery & Node Rebuild

## Goal
Define the procedures for recovering from node failures, data corruption, and cluster
partitions. The system must:
- Recover a single node from total disk loss using only the NATS cluster and peer nodes
- Detect and repair corrupted `redb` data
- Handle split-brain scenarios in NATS network partitions
- Provide operator playbooks for every failure mode
- Require no external backup infrastructure (recovery is built into the architecture)

---

## Context & Rationale

### The Problem This Solves

Step 19 (Cluster Bootstrap) defined how a **new** node joins the cluster. But a new node
and a **failed** node are different situations:

- **New node**: Empty redb, no history, no ongoing connections. Clean start.
- **Failed node**: May have been serving traffic when it died. NATS consumers may have
  pending acknowledgments. Routes pointed to it. Other nodes may have been steering
  requests to it. The cluster needs to converge around its absence.

This step defines what happens when things go wrong and how to get back to a healthy state.

### Why Shared-Nothing Makes Recovery Simpler

In a traditional architecture with a central database, node failure means:
- Checking if the central DB has the latest state
- Worrying about split-brain writes to the central DB
- Restoring database backups from S3 or a standby replica

In this platform, each node's `redb` is a **cache of the cluster state**, not the source
of truth. The source of truth is the combination of:
1. NATS JetStream durable streams (all deploy and route events ever published)
2. The artifact registry (all Wasm binaries)
3. Peer nodes (current secrets, via bootstrap protocol)

A node with a completely empty disk can rebuild its full state from these three sources.
This is the fundamental advantage of shared-nothing: **no backup infrastructure is needed
because the cluster IS the backup**.

### Failure Classification

```
Severity │ Description                           │ Recovery Strategy
─────────┼───────────────────────────────────────┼──────────────────────
   L1    │ Single instance crash (OOM, trap)      │ Automatic respawn (step 07)
   L2    │ Node process restart (OOM-killed,      │ Automatic restore from redb (step 07)
         │   SIGKILL, binary upgrade)             │
   L3    │ Node disk corruption (partial redb     │ Redb integrity check + re-bootstrap
         │   failure, bad sectors)                │
   L4    │ Node total loss (disk dead, VM         │ Full re-bootstrap from cluster (step 19)
         │   terminated, hardware failure)        │
   L5    │ Network partition (node alive but      │ NATS reconnect + state catch-up
         │   disconnected from NATS)              │
   L6    │ Multi-node failure (> N/2 nodes lost)  │ Manual intervention required
```

L1 and L2 are handled by existing steps (Supervisor health loop and restore_from_storage).
This document covers L3–L6.

### Split-Brain: Why This Platform Is Tolerant

A network partition splits the cluster into two groups. In a central-database architecture,
both groups might write conflicting data. In this platform:

- Each partition continues serving the apps it has locally compiled. No data conflict.
- Deploy events published in one partition are not seen by the other — but no state is
  corrupted. When the partition heals, NATS JetStream replays missed events.
- The only risk: routes may diverge (one partition adds a route the other doesn't know
  about). JetStream replay after reconnection resolves this automatically.

**No split-brain resolution protocol is needed** because there is no shared mutable state.

---

---

## 1. L3 Recovery: Redb Corruption Detection and Repair

### Detection

Redb uses checksums on its B-tree pages. A corrupted page will cause a `ReadError` or
`CommitError` when accessed. The node detects this during normal operation (health tick,
deploy handling, config read) or during a startup integrity check.

```rust
// crates/storage/src/integrity.rs
use crate::{Store, tables::*};
use common::error::PlatformError;
use tracing::{info, error, warn};

/// Integrity check result.
pub struct IntegrityReport {
    pub tables_checked: u32,
    pub tables_ok: u32,
    pub tables_corrupted: Vec<String>,
    pub recommendation: RecoveryAction,
}

pub enum RecoveryAction {
    /// All tables are readable. No action needed.
    Healthy,
    /// Some tables are corrupted but the critical path (artifacts + configs) is OK.
    /// Non-critical tables can be rebuilt from JetStream replay.
    PartialRebuild { tables: Vec<String> },
    /// Critical tables are corrupted. Full re-bootstrap required.
    FullRebootstrap,
}

impl Store {
    /// Check every table in redb for read errors.
    /// Called at startup and on-demand via admin API.
    pub fn integrity_check(&self) -> IntegrityReport {
        let table_names = vec![
            "artifacts", "configs", "secrets", "metrics",
            "routes", "raw_wasm", "schema_meta",
        ];

        let mut report = IntegrityReport {
            tables_checked: table_names.len() as u32,
            tables_ok: 0,
            tables_corrupted: Vec::new(),
            recommendation: RecoveryAction::Healthy,
        };

        for name in &table_names {
            match self.check_table_readable(name) {
                Ok(count) => {
                    info!(table = name, entries = count, "integrity check passed");
                    report.tables_ok += 1;
                }
                Err(e) => {
                    error!(table = name, error = %e, "integrity check FAILED — table corrupted");
                    report.tables_corrupted.push(name.to_string());
                }
            }
        }

        // Determine recovery action
        if report.tables_corrupted.is_empty() {
            report.recommendation = RecoveryAction::Healthy;
        } else if report.tables_corrupted.iter().any(|t| t == "artifacts" || t == "configs") {
            report.recommendation = RecoveryAction::FullRebootstrap;
        } else {
            report.recommendation = RecoveryAction::PartialRebuild {
                tables: report.tables_corrupted.clone(),
            };
        }

        report
    }

    fn check_table_readable(&self, _table_name: &str) -> Result<u64, PlatformError> {
        // Open a read transaction and iterate the table.
        // Any error during iteration indicates corruption.
        // Returns the number of entries read successfully.
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        // Table dispatch would be done based on table_name using the defined table constants.
        // For brevity, showing the pattern:
        let table = tx.open_table(ARTIFACTS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;

        let mut count = 0u64;
        for entry in table.iter().map_err(|e| PlatformError::Storage(e.to_string()))? {
            entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            count += 1;
        }
        Ok(count)
    }
}
```

### Repair: Partial Rebuild

If only non-critical tables are corrupted (metrics, routes), the node can rebuild them
without a full re-bootstrap:

```rust
// crates/storage/src/integrity.rs (continued)
impl Store {
    /// Rebuild corrupted tables by replaying data from NATS JetStream.
    pub async fn partial_rebuild(
        &self,
        corrupted_tables: &[String],
        nats_client: &async_nats::Client,
    ) -> Result<(), PlatformError> {
        for table_name in corrupted_tables {
            match table_name.as_str() {
                "routes" => {
                    // Drop the corrupted routes table and recreate it
                    self.recreate_table_routes()?;
                    // Replay all route events from JetStream
                    replay_jetstream_subject(nats_client, "routes.>", |event| {
                        // Re-apply each route event to the fresh table
                        self.apply_route_event(&event)
                    }).await?;
                    info!("routes table rebuilt from JetStream replay");
                }
                "metrics" => {
                    // Metrics are expendable — just recreate an empty table.
                    // Historical metrics are lost, but current metrics will re-accumulate.
                    self.recreate_table_metrics()?;
                    warn!("metrics table rebuilt (historical data lost)");
                }
                other => {
                    warn!(table = other, "no rebuild strategy for this table — manual intervention needed");
                }
            }
        }
        Ok(())
    }
}
```

---

## 2. L4 Recovery: Full Node Rebuild

A node with a completely destroyed disk (or a brand-new replacement VM) follows the
cluster bootstrap protocol (step 19) with one addition: it must first be recognized
by the cluster as a **returning node**, not a new one.

```
L4 Recovery Flow:

1. Replace the hardware / provision a new VM
2. Install the wasm-node binary (same version as the cluster)
3. Start the node with the same NODE_ID as the failed node
   (NODE_ID is stored in the systemd unit file or environment, not in redb)
4. Node boots with empty redb
5. Node publishes Event::NodeJoined to NATS (same as step 19)
6. Existing nodes recognize the NODE_ID and respond with StateSnapshot
7. Node receives: all deploy events (JetStream replay), routes, secrets (X25519 transfer)
8. Node compiles all Wasm artifacts (this is the slow step — minutes for many apps)
9. Node publishes Event::NodeReady
10. Pingora on other nodes starts routing traffic to the rebuilt node
```

### Time to Recover

The bottleneck is step 8: AOT compilation. A node with 50 deployed apps, each taking
500ms to compile, recovers in ~25 seconds. A node with 500 apps takes ~4 minutes. During
this time, the other nodes in the cluster handle all traffic.

```rust
// crates/node/src/recovery.rs
use storage::Store;
use tracing::{info, warn};

/// Check if this node is recovering from a failure (empty redb but known NODE_ID).
pub fn detect_recovery_mode(store: &Store, node_id: &str) -> RecoveryMode {
    match store.count_artifacts() {
        Ok(0) => {
            info!(node = node_id, "empty redb detected — entering recovery mode");
            RecoveryMode::FullRebuild
        }
        Ok(n) => {
            info!(node = node_id, artifacts = n, "existing state found — normal startup");
            RecoveryMode::Normal
        }
        Err(e) => {
            warn!(node = node_id, error = %e, "redb read failed — corruption likely");
            RecoveryMode::CorruptionDetected
        }
    }
}

pub enum RecoveryMode {
    /// Normal startup: redb has data, proceed with restore_from_storage()
    Normal,
    /// Full rebuild: empty redb, re-bootstrap from cluster
    FullRebuild,
    /// Corruption: run integrity check, then partial or full rebuild
    CorruptionDetected,
}
```

---

## 3. L5 Recovery: Network Partition Handling

When a node loses its NATS connection, it enters a degraded mode where it can still
serve requests for apps it has locally, but it cannot:
- Receive new deploy events
- Report load to the cluster (other nodes stop steering traffic to it)
- Receive secret updates

```rust
// crates/messaging/src/reconnect.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Tracks NATS connection health.
pub struct NatsHealth {
    connected: Arc<AtomicBool>,
    last_connected_secs: Arc<std::sync::atomic::AtomicU64>,
}

impl NatsHealth {
    pub fn new() -> Self {
        NatsHealth {
            connected: Arc::new(AtomicBool::new(true)),
            last_connected_secs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
        tracing::warn!("NATS connection lost — entering degraded mode");
    }

    pub fn mark_reconnected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_connected_secs.store(now, Ordering::Relaxed);
        tracing::info!("NATS connection restored — catching up");
    }
}
```

### Catch-Up After Reconnection

When the NATS connection is restored, JetStream automatically replays missed messages
from the consumer's last acknowledged position. The node processes these events in order:

1. Deploy events → compile new apps, update configs
2. Route events → update the local route table
3. Secret updates → decrypt and store new secret values
4. NodeLoad events → rebuild the load table for steering decisions

No special catch-up logic is needed — JetStream's durable consumer handles this natively.
This is why step 08 uses durable consumers with explicit acknowledgment.

### Degraded Mode Behavior

While disconnected from NATS:

```
Feature                  │ Available?  │ Reason
─────────────────────────┼─────────────┼────────────────────────────────
Serve existing apps      │ YES         │ Artifacts are in local redb
Cold-start existing apps │ YES         │ No NATS needed for local spawn
Receive new deploys      │ NO          │ Deploy events come via NATS
Receive secret updates   │ NO          │ Secret rotation comes via NATS
Report load to cluster   │ NO          │ NodeLoad published via NATS
Cross-node steering      │ NO          │ NodeLoad table goes stale
Health checks            │ YES         │ TCP probes are local
Metrics collection       │ YES         │ Prometheus scrape is HTTP, not NATS
GC                       │ YES         │ Local redb operation only
Admin API                │ PARTIAL     │ Local operations work; cluster operations fail
```

---

## 4. L6 Recovery: Multi-Node Failure

If more than half the nodes fail simultaneously, the platform may lose the ability to
respond to NATS events (not enough consumers). This is an exceptional scenario that
requires manual intervention.

### Recovery Playbook

```
Scenario: 3-node cluster, 2 nodes lost simultaneously.

1. Remaining node (Node-2) continues serving local traffic (degraded mode)
2. Operator provisions 2 replacement VMs
3. Ensure NATS cluster quorum is restored:
   - If NATS itself was running on the failed nodes, restore NATS first
   - Use NATS's built-in cluster recovery (raft-based consensus)
4. Start wasm-node on replacement VMs with NODE_ID of the failed nodes
5. New nodes re-bootstrap from Node-2 (the survivor) via step 19
6. Verify: `wasm-ctl platform status` shows all 3 nodes healthy
```

### NATS Quorum Recovery

NATS JetStream uses Raft for stream replication. Losing N/2+1 NATS servers means
JetStream cannot commit writes. NATS recovery depends on the deployment topology:

```
NATS Topology       │ Tolerance    │ Recovery
────────────────────┼──────────────┼──────────────────────────
Embedded (1 per     │ Lose N/2-1   │ Restore NATS data dir on new nodes,
  wasm-node)        │   nodes      │ or start fresh and replay from
                    │              │   surviving nodes
External cluster    │ Independent  │ NATS cluster recovers independently;
  (3+ dedicated     │   of app     │ wasm-nodes just reconnect
  NATS servers)     │   nodes      │
```

For production, **external NATS clusters** are recommended. This decouples NATS
availability from wasm-node availability.

---

## 5. Preventive: Startup Integrity Check

Every node boot runs an integrity check before accepting traffic. This catches corruption
early — before a bad read causes a runtime error during request handling.

```rust
// crates/node/src/main.rs (added to startup sequence from step 14)
use storage::integrity::RecoveryAction;

async fn startup_integrity_check(store: &Store, nats: &async_nats::Client) {
    let report = store.integrity_check();

    match report.recommendation {
        RecoveryAction::Healthy => {
            tracing::info!(
                tables = report.tables_ok,
                "startup integrity check passed"
            );
        }
        RecoveryAction::PartialRebuild { tables } => {
            tracing::warn!(
                corrupted = ?tables,
                "startup integrity check found corrupt tables — rebuilding"
            );
            store.partial_rebuild(&tables, nats).await
                .expect("partial rebuild failed — manual intervention required");
        }
        RecoveryAction::FullRebootstrap => {
            tracing::error!(
                "critical tables corrupted — triggering full re-bootstrap"
            );
            // Delete the corrupted redb file and restart.
            // The node will detect an empty redb and enter recovery mode.
            std::fs::remove_file(store.db_path())
                .expect("failed to delete corrupted redb");
            tracing::info!("corrupted redb deleted — restarting for clean bootstrap");
            std::process::exit(1); // systemd restarts the process
        }
    }
}
```

---

## 6. CLI Commands

```
# Run an integrity check on a specific node
wasm-ctl node health --node node-0
# Output:
# Tables checked: 7
# Tables OK: 7
# Tables corrupted: 0
# Status: HEALTHY

# Force a full re-bootstrap (nuclear option)
wasm-ctl node rebuild --node node-0
# This will:
#   1. Gracefully drain all traffic from node-0
#   2. Delete node-0's redb file
#   3. Restart the node, triggering a full re-bootstrap from the cluster
# Are you sure? [y/N]

# Check NATS connectivity for all nodes
wasm-ctl cluster health
# Output:
# node-0: NATS=connected, redb=healthy, uptime=24h
# node-1: NATS=connected, redb=healthy, uptime=12h
# node-2: NATS=DISCONNECTED (last seen 45s ago), redb=unknown
```

---

## 7. Prometheus Metrics for Recovery

```rust
// Additional metrics for disaster recovery monitoring
use prometheus::{IntGauge, Opts, Registry};

pub struct RecoveryMetrics {
    /// 1 if the node is in degraded mode (NATS disconnected), 0 otherwise.
    pub nats_disconnected: IntGauge,

    /// Number of corrupted tables detected in the last integrity check.
    pub corrupted_tables: IntGauge,

    /// Seconds since the last successful NATS message was received.
    pub nats_last_message_age_secs: IntGauge,
}

// Alerting rules (Prometheus alertmanager):
//
// alert: NatsDisconnected
//   expr: nats_disconnected == 1
//   for: 30s
//   annotations: "Node {{ $labels.node }} lost NATS connection"
//
// alert: RedbCorruption
//   expr: corrupted_tables > 0
//   annotations: "Node {{ $labels.node }} has corrupted redb tables"
//
// alert: NatsStale
//   expr: nats_last_message_age_secs > 60
//   annotations: "Node {{ $labels.node }} hasn't received NATS messages in 60s"
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### L3 — Corruption
- [ ] `integrity_check()` detects a corrupted table and returns `PartialRebuild`
- [ ] `partial_rebuild()` rebuilds the routes table from JetStream replay
- [ ] After partial rebuild, the node serves traffic normally
- [ ] A corrupted artifacts table triggers `FullRebootstrap` (process exit + restart)

### L4 — Total Loss
- [ ] A node with empty redb and a known NODE_ID enters recovery mode
- [ ] The node receives a StateSnapshot from a peer and compiles all artifacts
- [ ] After rebuild, `wasm-ctl platform status` shows the node as healthy
- [ ] Time to recover 50 apps is under 60 seconds

### L5 — Partition
- [ ] A node disconnected from NATS continues serving existing apps
- [ ] `nats_disconnected` metric flips to 1 within 5 seconds of disconnection
- [ ] After reconnection, JetStream replays all missed deploy and route events
- [ ] Post-reconnection, the node's state matches all other nodes

### L6 — Multi-Node Failure
- [ ] Documentation describes the manual recovery playbook for N/2+ failure
- [ ] The surviving node continues serving traffic in degraded mode
- [ ] Replacement nodes can re-bootstrap from the surviving node

### Startup
- [ ] Every node boot runs an integrity check before accepting traffic
- [ ] A failed integrity check prevents the node from joining the upstream pool
- [ ] Admin can force a rebuild via `wasm-ctl node rebuild`
