# Step 15 — Route Management

## Goal
Every Pingora node needs a mapping of `Host header → AppId`. This file covers:
- The `routes` table in redb (persistent across restarts)
- NATS sync so every node always has the same routes
- Bootstrap: a fresh node reads routes from redb, then subscribes to live updates
- The `HostRouter` integration (wiring into step 09)

---

## Context & Rationale

### The Problem This Solves

Pingora needs to answer one question for every incoming request: **which app should handle
this request?** The answer comes from the Host header (`api.myapp.com` → `api-users:v2`).

This mapping needs to be:
1. **Fast**: looked up on every request — must be an in-memory hash map, not a DB query
2. **Persistent**: survives node restarts — stored in redb
3. **Consistent**: all nodes must have the same routes — synced via NATS JetStream
4. **Instantly updated**: adding a route should take effect within milliseconds on all nodes

### Why Routes Are Separate from AppConfig

Routes are a distinct concept from app configuration. Separation allows:

- **Multiple routes to the same app**: `api.company.com` and `app.company.com` can both
  point to `api-users:v2`. The app doesn't need to know about its external hostnames.
- **Route migration without redeploy**: changing which domain points to which app (e.g.,
  during a cutover) does not require recompiling or restarting any Wasm instance.
- **Route versioning**: you can add `api-v2.company.com → api-users:v3` before removing
  the old route, enabling gradual traffic migration.

### Two-Phase Consistency: redb First, HostRouter Second

Route state must be consistent at two levels:
1. **Persistent** (redb): survives process restart
2. **Active** (HostRouter in memory): used by Pingora for routing

Both must be updated on every route change. The event handler updates both atomically:
```
Event received → save_route() [redb] → host_router.add_route() [memory]
```

If the node crashes between the two writes, on restart `load_from_store()` will re-populate
the HostRouter from redb — eventual consistency is guaranteed.

### Why Routes Use JetStream (Like Deployments)

Route changes must be durable for the same reason deploy events must be: a node that
was offline when a route was added must receive it when it reconnects. Without JetStream,
that node would have a stale routing table indefinitely.

The `DEPLOY` JetStream stream is extended to cover `routes.>` subjects. This means a
single stream handles both deployment and routing state — any node can replay it to
reconstruct the full operational state of the cluster.

### Route Conflict Resolution

If two operators simultaneously add conflicting routes (`api.com → app-a` and
`api.com → app-b`), the last write wins based on the `updated_at` timestamp. This is
a pragmatic choice: proper distributed locking would require a consensus protocol (Raft,
Paxos) which adds significant complexity. For a routing table that changes rarely, last
write wins is acceptable — operators should coordinate.

---

---

## 1. Route Record

```rust
// crates/common/src/types.rs (add)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    /// The Host header value to match. Supports exact match only for now.
    /// e.g. "api.myapp.com" or "myapp.com"
    pub host: String,

    /// The target app. Must exist in the [configs] table.
    pub app_id: AppId,

    /// Optional path prefix (default "/").
    pub path_prefix: String,

    /// If true, strip the path_prefix before forwarding.
    pub strip_prefix: bool,

    pub created_at: u64,
    pub updated_at: u64,
}
```

---

## 2. Routes Table in redb

```rust
// crates/storage/src/tables.rs (add)
/// Key   : host string (e.g. "api.myapp.com")
/// Value : JSON-serialized Route
pub const ROUTES: TableDefinition<&str, &str> = TableDefinition::new("routes");
```

```rust
// crates/storage/src/routes.rs
use crate::{Store, tables::ROUTES};
use common::{error::PlatformError, types::Route};

impl Store {
    pub fn save_route(&self, route: &Route) -> Result<(), PlatformError> {
        let json = serde_json::to_string(route)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(ROUTES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(route.host.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn delete_route(&self, host: &str) -> Result<(), PlatformError> {
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(ROUTES)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.remove(host)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn list_routes(&self) -> Result<Vec<Route>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(ROUTES)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut routes = Vec::new();
        for entry in table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (_, v) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            let route: Route = serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            routes.push(route);
        }
        Ok(routes)
    }
}
```

---

## 3. NATS Events for Routes

```rust
// crates/messaging/src/events.rs (add to Event enum)
RouteAdd {
    route: common::types::Route,
},
RouteRemove {
    host: String,
},
```

```rust
// Subject mapping (add to Event::subject())
Event::RouteAdd { .. }    => "routes.add".to_string(),
Event::RouteRemove { .. } => "routes.remove".to_string(),
```

Add `routes.>` to the JetStream `DEPLOY` stream so route changes are durable and replayed on node restart:

```rust
// crates/messaging/src/lib.rs — setup_jetstream()
js.create_stream(StreamConfig {
    name: "DEPLOY".to_string(),
    subjects: vec!["deploy.>".to_string(), "routes.>".to_string()],
    ..Default::default()
}).await?;
```

---

## 4. Event Handler Integration

```rust
// crates/messaging/src/handlers.rs (add to EventDispatcher::handle)
Event::RouteAdd { route } => {
    // 1. Persist to redb
    self.store.save_route(&route).ok();
    // 2. Update the in-memory HostRouter used by Pingora
    self.host_router.add_route(route.host.clone(), route.app_id.clone()).await;
    tracing::info!(host = %route.host, app = %route.app_id.0, "route added");
}
Event::RouteRemove { host } => {
    self.store.delete_route(&host).ok();
    self.host_router.remove_route(&host).await;
    tracing::info!(host, "route removed");
}
```

---

## 5. HostRouter: Add remove_route

```rust
// crates/proxy/src/router.rs (add)
impl HostRouter {
    pub async fn remove_route(&self, host: &str) {
        self.routes.write().await.remove(host);
    }

    /// Load all routes from redb into memory (called at startup).
    pub async fn load_from_store(&self, store: &storage::Store) {
        match store.list_routes() {
            Ok(routes) => {
                let mut map = self.routes.write().await;
                for r in routes {
                    map.insert(r.host, r.app_id);
                }
                tracing::info!(count = map.len(), "routes loaded from storage");
            }
            Err(e) => tracing::error!(error = %e, "failed to load routes"),
        }
    }
}
```

---

## 6. Bootstrap Sequence (Node Startup)

In `main.rs`, after opening storage and before starting Pingora:

```rust
// crates/node/src/main.rs (add after step 11 — host_router init)

// Load persisted routes into HostRouter
host_router.load_from_store(&store).await;
tracing::info!("routes loaded from local storage");

// Then subscribe to live route events via NATS (step 15 of event subscriptions)
{
    let d = dispatcher.clone();
    bus.subscribe_durable("DEPLOY", &format!("{}-routes", args.node_id), "routes.>",
        move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        }
    ).await?;
}
```

**Result**: On first boot, routes come from JetStream replay. On subsequent boots, they come from redb (fast, no NATS round-trip needed).

---

## 7. Route Conflict Resolution

When two apps try to claim the same host, the **last write wins** (latest `updated_at` timestamp). The operator CLI (step 18) should warn on conflict.

```rust
// crates/storage/src/routes.rs
impl Store {
    pub fn save_route_if_newer(&self, route: &Route) -> Result<bool, PlatformError> {
        if let Some(existing) = self.load_route(&route.host)? {
            if existing.updated_at >= route.updated_at {
                tracing::warn!(
                    host = %route.host,
                    "route update ignored (existing is newer)"
                );
                return Ok(false);
            }
        }
        self.save_route(route)?;
        Ok(true)
    }

    fn load_route(&self, host: &str) -> Result<Option<Route>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(ROUTES)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(host)
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?)),
            None => Ok(None),
        }
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Storage
- [x] `ROUTES` table is created in redb on `Store::open()`
- [x] `save_route()` persists a `Route` and `list_routes()` returns it
- [x] `delete_route(host)` removes the entry; subsequent `list_routes()` excludes it
- [x] `save_route_if_newer()` ignores an update with an older `updated_at` timestamp
- [x] Routes survive a `Store` drop and re-open (data is persistent)

### HostRouter
- [x] `add_route(host, app_id)` makes `resolve(host)` return the correct `AppId`
- [x] `remove_route(host)` causes `resolve(host)` to return `None`
- [x] `load_from_store(store)` populates all routes from redb on startup and logs the count
- [x] `add_route` and `resolve` are safe to call concurrently from multiple threads

### NATS Integration
- [x] Publishing `Event::RouteAdd` causes every node to call `save_route()` and `host_router.add_route()`
- [x] Publishing `Event::RouteRemove` causes every node to call `delete_route()` and `host_router.remove_route()`
- [x] `routes.>` subjects are covered by the JetStream `DEPLOY` stream (durable, replayed on restart)

### Bootstrap
- [x] On node restart, `host_router.load_from_store()` is called before Pingora starts accepting traffic
- [x] A fresh node that joins the cluster receives routes via JetStream replay within 10 seconds
- [x] After route loading, `resolve("api.myapp.com")` returns the correct app without any NATS message

### Tests
- [x] A test adds a route, resolves it, removes it, and verifies it is gone
- [x] A test verifies that after a Store re-open, all previously saved routes are still present
- [x] A test verifies that `RouteAdd` NATS events correctly populate the HostRouter
