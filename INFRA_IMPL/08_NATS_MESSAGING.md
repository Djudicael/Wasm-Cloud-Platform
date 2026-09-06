# Step 08 — NATS Messaging Bus

## Goal
Implement the inter-node communication layer using NATS.

---

## Context & Rationale

### The Problem This Solves

A multi-node cluster needs a way for nodes to coordinate:
- When an operator deploys a new app, every node must receive the binary and compile it
- When a node spawns an instance, all other nodes' proxies must learn about it
- When a secret is rotated, all nodes must update their local cache

The naive approach — an operator CLI that iterates over known nodes and sends HTTP requests
to each — has critical problems:
1. The operator must know every node's address at all times
2. If a node is temporarily unreachable, it misses the update
3. Adding or removing nodes requires reconfiguring the operator's address book

NATS solves all three: nodes subscribe to subjects at startup. The operator publishes once.
NATS fans out to all subscribers. Nodes that are temporarily down receive missed events
when they reconnect (via JetStream replay).

### Pub/Sub vs Request/Reply

NATS supports two communication patterns:

**Pub/Sub**: Publisher sends to a subject; all subscribers receive a copy. Used for:
- `deploy.app.new` → fan out to all nodes simultaneously
- `instance.ready` → all proxies learn about a new upstream

**Request/Reply**: Publisher sends to a subject with a reply address; one subscriber responds.
Used for:
- Cluster bootstrap (step 19): new node asks for current state, one existing node responds
- Admin status queries: CLI asks for cluster health, nodes reply individually

This step implements pub/sub. Request/reply is built in step 19.

### Why JetStream for Deploy Events?

Plain NATS pub/sub is fire-and-forget. If a node is restarting at the moment a `deploy.app.new`
event is published, it misses the event. The app never gets deployed to that node.

JetStream adds **persistence and replay** to NATS subjects. The `DEPLOY` stream retains
every `deploy.>` event. When a node reconnects, it creates a **durable consumer** that
replays all missed events from where it left off.

This is critical for the shared-nothing design: nodes are independent but must eventually
converge to the same deployed state, even after restarts or temporary network partitions.

```
Time →

Node-0: [ONLINE ]  deploy v1 ──────────────────────────────
Node-1: [ONLINE ]  deploy v1 ──────────────────────────────
Node-2: [OFFLINE]             [restart]  replays deploy v1 ──►  now has v1
                  JetStream retains ────────────────────────►
```

### Why NOT JetStream for Secrets?

Secrets (database passwords, API keys) must never be stored in a message queue. NATS
JetStream persists messages to disk on the NATS server. A NATS server disk compromise
would expose all rotated secrets in plaintext.

Secrets use plain NATS pub/sub for rotation notifications. The actual secret value is
delivered via an encrypted payload, using the cluster key (which the NATS server never
sees in plaintext). For new nodes that join the cluster, secrets are transferred via a
direct encrypted NATS request/reply, not via JetStream replay (see step 19).

### The Event Enum Design

All NATS messages are typed via a single `Event` enum with `#[serde(tag = "type")]`.
This means:
- Every message has a `type` field in JSON: `{"type": "deploy_app", ...}`
- The deserializer can dispatch to the right handler based on the tag
- Adding a new event type is additive — old nodes that don't recognize the tag will
  log a warning and skip it, not crash

### Subject Hierarchy Design

Subjects follow `<domain>.<action>.<qualifier>`:
- `deploy.app.new` — the deploy domain, action: new app
- `instance.ready.<app_id>.<node_id>` — qualified with app and node for selective subscription
- `secrets.update.<app_id>` — qualified so nodes can subscribe to only their apps' secrets

This hierarchy enables **wildcard subscriptions**: a node interested in all instance events
subscribes to `instance.ready.>`. A node interested only in `api-users:v2` subscribes to
`instance.ready.api-users:v2.>`. This reduces unnecessary message processing.

### Remote Instance Registration

When Node-1 spawns a new instance for `api-users:v2`, it publishes `instance.ready`.
Node-0 and Node-2 receive this event and add Node-1's instance address to their upstream
registries. This means Pingora on Node-0 can route requests to an instance running on
Node-1 — enabling cross-node load balancing without a separate service registry.

The check `if node_id != self.our_node_id()` in the event handler prevents a node from
re-registering its own instances (which were already registered directly by the Supervisor).

--- Every node subscribes to a set of
subjects and reacts to events from other nodes. The bus handles:
- Deploy / undeploy commands
- Instance ready / dead notifications (service discovery)
- Secret rotation
- Config updates
- Load reports (for cross-node routing decisions)

---

## 1. NATS Subject Schema

All subjects follow this pattern: `<domain>.<action>.<target>`

| Subject | Direction | Payload | Purpose |
|---------|-----------|---------|---------|
| `deploy.app.new` | Control → All nodes | `DeployPayload` | Deploy a new Wasm binary |
| `deploy.app.remove` | Control → All nodes | `AppId` | Undeploy an app |
| `instance.ready` | Node → All proxies | `InstanceReadyPayload` | New instance available |
| `instance.dead` | Node → All proxies | `InstanceDeadPayload` | Instance gone |
| `secrets.update.<app_id>` | Control → All nodes | `SecretUpdatePayload` | Rotate a secret |
| `config.update.<app_id>` | Control → All nodes | `AppConfig` | Update app configuration |
| `node.load.<node_id>` | Node → All nodes | `NodeLoadPayload` | CPU/fuel usage report |
| `healthcheck.ping` | Any → Any | — | Cluster heartbeat |

---

## 2. Event Types

```rust
// crates/messaging/src/events.rs
use common::types::{AppConfig, AppId};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ── Deployment ─────────────────────────────────────────────────
    DeployApp {
        app_id: AppId,
        config: AppConfig,
        /// The raw .wasm bytes (base64-encoded for JSON transport).
        /// For large binaries, prefer a separate artifact fetch via `artifact_url`.
        wasm_bytes: Vec<u8>,
    },
    RemoveApp {
        app_id: AppId,
    },

    // ── Instance Lifecycle ─────────────────────────────────────────
    InstanceReady {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },
    InstanceDead {
        app_id: AppId,
        addr: SocketAddr,
        node_id: String,
    },

    // ── Configuration ──────────────────────────────────────────────
    SecretUpdate {
        app_id: AppId,
        key: String,
        /// Encrypted value (encrypted with the cluster key, not the node key).
        encrypted_value: Vec<u8>,
    },
    ConfigUpdate {
        app_id: AppId,
        config: AppConfig,
    },

    // ── Load Reporting ────────────────────────────────────────────
    NodeLoad {
        node_id: String,
        cpu_percent: f32,
        fuel_budget_used_percent: f32,
        active_instances: u32,
    },
}

impl Event {
    /// Convert to NATS subject string.
    pub fn subject(&self) -> String {
        match self {
            Event::DeployApp { .. }       => "deploy.app.new".to_string(),
            Event::RemoveApp { .. }       => "deploy.app.remove".to_string(),
            Event::InstanceReady { app_id, node_id, .. } =>
                format!("instance.ready.{}.{}", app_id.0, node_id),
            Event::InstanceDead { app_id, node_id, .. } =>
                format!("instance.dead.{}.{}", app_id.0, node_id),
            Event::SecretUpdate { app_id, .. } =>
                format!("secrets.update.{}", app_id.0),
            Event::ConfigUpdate { app_id, .. } =>
                format!("config.update.{}", app_id.0),
            Event::NodeLoad { node_id, .. } =>
                format!("node.load.{}", node_id),
        }
    }
}
```

---

## 3. NATS Client Wrapper

```rust
// crates/messaging/src/lib.rs
pub mod events;
pub mod handlers;
pub mod publisher;

use async_nats::Client;
use common::error::PlatformError;
use events::Event;

#[derive(Clone)]
pub struct NatsBus {
    client: Client,
}

impl NatsBus {
    /// Connect to the NATS server.
    pub async fn connect(url: &str) -> Result<Self, PlatformError> {
        let client = async_nats::connect(url).await
            .map_err(|e| PlatformError::Messaging(format!("NATS connect: {e}")))?;
        tracing::info!(url, "connected to NATS");
        Ok(NatsBus { client })
    }

    /// Publish an event to the appropriate subject.
    pub async fn publish(&self, event: &Event) -> Result<(), PlatformError> {
        let subject = event.subject();
        let payload = serde_json::to_vec(event)
            .map_err(|e| PlatformError::Messaging(e.to_string()))?;
        self.client.publish(subject.clone(), payload.into()).await
            .map_err(|e| PlatformError::Messaging(format!("publish to {subject}: {e}")))?;
        Ok(())
    }

    /// Subscribe to a subject pattern and return a stream of Events.
    pub async fn subscribe<F, Fut>(
        &self,
        subject: &str,
        handler: F,
    ) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut sub = self.client.subscribe(subject.to_string()).await
            .map_err(|e| PlatformError::Messaging(format!("subscribe to {subject}: {e}")))?;

        let subject = subject.to_string();
        tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                match serde_json::from_slice::<Event>(&msg.payload) {
                    Ok(event) => handler(event).await,
                    Err(e) => tracing::warn!(
                        subject = %subject,
                        error = %e,
                        "failed to deserialize NATS message"
                    ),
                }
            }
        });
        Ok(())
    }

    pub fn client(&self) -> &Client {
        &self.client
    }
}
```

---

## 4. Event Handlers (Subscriber Side)

```rust
// crates/messaging/src/handlers.rs
use crate::events::Event;
use common::types::AppId;
use proxy::upstream::UpstreamRegistry;
use supervisor::Supervisor;
use storage::Store;
use runtime::WasmRuntime;
use std::sync::Arc;
use tracing::{info, warn, error};

pub struct EventDispatcher {
    supervisor: Arc<Supervisor>,
    upstream: Arc<UpstreamRegistry>,
    store: Store,
    runtime: WasmRuntime,
}

impl EventDispatcher {
    pub async fn handle(&self, event: Event) {
        match event {
            Event::DeployApp { app_id, config, wasm_bytes } => {
                self.handle_deploy(app_id, config, wasm_bytes).await
            }
            Event::RemoveApp { app_id } => {
                self.handle_remove(app_id).await
            }
            Event::InstanceReady { app_id, addr, node_id } => {
                // Only register if it's from a DIFFERENT node
                // (our own instances are registered directly by the Supervisor)
                if node_id != self.our_node_id() {
                    self.upstream.add(&app_id, addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance registered");
                }
            }
            Event::InstanceDead { app_id, addr, node_id } => {
                if node_id != self.our_node_id() {
                    self.upstream.remove(&app_id, &addr).await;
                    info!(app = %app_id.0, %addr, from_node = %node_id, "remote instance deregistered");
                }
            }
            Event::SecretUpdate { app_id, key, encrypted_value } => {
                info!(app = %app_id.0, key, "received secret rotation");
                // Decrypt with cluster key and re-encrypt with node key
                // (see step 06 for details)
                // The next instance spawn for this app will pick up the new secret.
            }
            Event::ConfigUpdate { app_id, config } => {
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "config update failed");
                }
            }
            Event::NodeLoad { .. } => {
                // Collected by the metrics module for cross-node routing decisions
            }
        }
    }

    async fn handle_deploy(
        &self,
        app_id: AppId,
        config: common::types::AppConfig,
        wasm_bytes: Vec<u8>,
    ) {
        info!(app = %app_id.0, bytes = wasm_bytes.len(), "deploying app");

        // 1. Compile (CPU-intensive — spawn_blocking)
        let runtime = self.runtime.clone();
        let wasm_bytes_clone = wasm_bytes.clone();
        let artifact = tokio::task::spawn_blocking(move || {
            runtime.compile(&wasm_bytes_clone)
        }).await;

        match artifact {
            Ok(Ok(artifact_bytes)) => {
                // 2. Store artifact and config
                if let Err(e) = self.store.store_artifact(&app_id, &artifact_bytes) {
                    error!(app = %app_id.0, error = %e, "failed to store artifact");
                    return;
                }
                if let Err(e) = self.store.save_config(&config) {
                    error!(app = %app_id.0, error = %e, "failed to store config");
                    return;
                }
                info!(app = %app_id.0, "deploy complete, waiting for first request");
            }
            Ok(Err(e)) => error!(app = %app_id.0, error = %e, "compilation failed"),
            Err(e) => error!(app = %app_id.0, error = %e, "spawn_blocking panic"),
        }
    }

    async fn handle_remove(&self, app_id: AppId) {
        info!(app = %app_id.0, "removing app");
        // Stop all instances first
        // (supervisor.kill_all_for(&app_id) — not shown here)
        self.store.delete_artifact(&app_id).ok();
        // Remove config too (or mark as tombstone)
    }

    fn our_node_id(&self) -> String {
        std::env::var("NODE_ID").unwrap_or_else(|_| "node-0".to_string())
    }
}
```

---

## 5. Publisher (Supervisor → NATS)

```rust
// crates/messaging/src/publisher.rs
use crate::{events::Event, NatsBus};
use common::types::AppId;
use std::net::SocketAddr;
use tokio::sync::mpsc;

/// Background task: drains an mpsc channel and publishes events to NATS.
pub async fn run_publisher(bus: NatsBus, mut rx: mpsc::Receiver<Event>) {
    while let Some(event) = rx.recv().await {
        if let Err(e) = bus.publish(&event).await {
            tracing::error!(error = %e, "failed to publish event");
        }
    }
}
```

---

## 6. JetStream for Durable State

For events that must survive a NATS server restart (e.g. deployment orders), use JetStream:

```rust
// crates/messaging/src/lib.rs (JetStream setup)
use async_nats::jetstream::{self, stream::Config as StreamConfig, consumer::pull::Config as PullConfig};

impl NatsBus {
    /// Create durable JetStream subjects for deployment events.
    pub async fn setup_jetstream(&self) -> Result<(), PlatformError> {
        let js = jetstream::new(self.client.clone());

        // Create the "DEPLOY" stream that retains deploy events
        js.create_stream(StreamConfig {
            name: "DEPLOY".to_string(),
            subjects: vec!["deploy.>".to_string()],
            max_messages: 10_000,
            ..Default::default()
        }).await.map_err(|e| PlatformError::Messaging(e.to_string()))?;

        Ok(())
    }

    /// Subscribe as a durable consumer (will replay missed events on restart).
    pub async fn subscribe_durable<F, Fut>(
        &self,
        stream: &str,
        consumer_name: &str,
        filter_subject: &str,
        handler: F,
    ) -> Result<(), PlatformError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let js = jetstream::new(self.client.clone());
        let stream = js.get_stream(stream).await
            .map_err(|e| PlatformError::Messaging(e.to_string()))?;

        let consumer = stream.create_consumer(PullConfig {
            durable_name: Some(consumer_name.to_string()),
            filter_subject: filter_subject.to_string(),
            ..Default::default()
        }).await.map_err(|e| PlatformError::Messaging(e.to_string()))?;

        tokio::spawn(async move {
            let mut messages = consumer.messages().await.unwrap();
            while let Some(Ok(msg)) = messages.next().await {
                if let Ok(event) = serde_json::from_slice::<Event>(&msg.payload) {
                    handler(event).await;
                }
                msg.ack().await.ok();
            }
        });
        Ok(())
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Connection
- [ ] `NatsBus::connect(url)` connects to a running NATS server without error
- [ ] `NatsBus::connect_secure(url, creds)` connects with credentials and rejects invalid creds
- [x] `NatsBus::connect_with_tls(...)` authenticates a private CA and NATS
      mutual-TLS identity; incomplete client certificate/key input fails closed
- [ ] On NATS server restart, the client reconnects automatically within 5 seconds

### Publishing
- [ ] `bus.publish(event)` succeeds for every variant of `Event`
- [ ] Each `Event` variant maps to a unique, non-empty subject string
- [ ] Publishing with NATS unavailable returns a `PlatformError::Messaging` — not a panic

### Subscribing
- [ ] `bus.subscribe(subject, handler)` calls the handler for each matching message
- [ ] A malformed NATS message (invalid JSON) logs a warning and does not crash the subscriber
- [ ] Wildcard subjects (`deploy.>`, `instance.ready.>`) match all sub-subjects

### JetStream
- [ ] `setup_jetstream()` creates the `DEPLOY` stream covering `deploy.>` and `routes.>`
- [ ] Calling `setup_jetstream()` a second time on an existing stream does not error
- [ ] `subscribe_durable()` replays missed messages when a consumer reconnects after a restart
- [ ] Messages are acknowledged (`msg.ack()`) so they are not redelivered

### Event Handlers
- [ ] `DeployApp` event triggers artifact fetch + compile + store on every receiving node
- [ ] `InstanceReady` from a remote node adds the address to the local upstream registry
- [ ] `InstanceDead` from a remote node removes the address from the local upstream registry
- [ ] `SecretUpdate` triggers a redb secret cache update for the named app
- [ ] `ConfigUpdate` persists the new config to redb

### Tests
- [ ] A test publishes a `DeployApp` event and verifies the handler is called
- [ ] A test verifies durable consumer replays events after a subscriber restart
- [ ] All tests use an embedded or local NATS server, not a mock
