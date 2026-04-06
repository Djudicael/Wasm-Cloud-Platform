# Step 21 — Database Connection Management

## Goal
Each Wasm instance opens its own TCP connection to the database.

---

## Context & Rationale

### The Problem This Solves

In native Rust, you use `sqlx::Pool` or `deadpool-postgres` to share a connection pool
across all async tasks in a process. The pool has 5–20 connections; thousands of
concurrent requests share them efficiently.

In this platform, every Wasm instance has **its own isolated linear memory** — there is
no shared heap between instances. `sqlx::Pool` inside Instance A is completely invisible
to Instance B. Each instance creates its own pool of connections when it starts.

The math becomes alarming quickly:
- 50 concurrent instances of `api-users`
- Each instance's pool = 5 connections (minimum for `sqlx`)
- Total DB connections = 250

PostgreSQL's default `max_connections = 100`. The 51st instance to start will fail to
connect. Users see 500 errors. The platform becomes a DB connection bomb.

### Why pgBouncer Is the Right Solution

pgBouncer is a PostgreSQL connection pooler that sits between the application and the
database server. In **transaction pooling mode**, a real database connection is only
held during an active transaction — the moment the transaction commits or rolls back,
the connection returns to the pool and can be used by another client.

This is ideal for Wasm instances because:
- Wasm instances connect to `127.0.0.1:5432` (pgBouncer) — no code change needed
- pgBouncer holds 20 real connections to PostgreSQL
- 1000 Wasm instances connect to pgBouncer (within `max_client_conn`)
- Each of the 1000 clients gets a real connection only during active transactions
- Real PostgreSQL never sees more than 20 concurrent connections

### Why Not Fix This Inside the Wasm Runtime?

One might think: "make the Wasm runtime share a connection pool at the host level." But:

1. **WASI abstraction**: The Wasm module makes TCP connections via WASI `sock_connect`.
   The Wasm runtime sees this as a raw TCP connection request. It has no understanding
   of the PostgreSQL protocol or connection pooling semantics.

2. **Protocol complexity**: Building a PostgreSQL-aware connection multiplexer that
   handles prepared statements, session variables, and transaction state correctly is
   effectively building pgBouncer. pgBouncer already exists and is battle-tested.

3. **App transparency**: The Wasm app connects to `localhost:5432` and uses `sqlx` exactly
   as it would in any other environment. No platform SDK required.

### Transaction Mode vs Session Mode

pgBouncer's `pool_mode` setting determines when connections are returned to the pool:

- **Session mode**: connection held for the entire client session (=entire app instance lifetime)
  → not useful here: each Wasm instance lives for minutes
- **Transaction mode**: connection returned after each transaction → exactly what we want
- **Statement mode**: connection returned after each SQL statement → breaks multi-statement
  transactions, not safe for general use

**Transaction mode is the required setting** for this platform.

### The Supervisor Connection Proxy Alternative

For edge deployments where pgBouncer cannot be installed (embedded systems, single-binary
deployments), the Supervisor implements a minimal TCP multiplexer. This is a raw byte
forwarder — it does not understand PostgreSQL. Use only for databases that don't have
session state (e.g., Redis, simple TCP services).

--- With 50 instances of
the same app running on the node, that means 50 simultaneous DB connections — which
quickly exhausts PostgreSQL's `max_connections` limit.

This file covers:
1. Why connection pooling inside Wasm is impossible (and what to do instead)
2. Running `pgBouncer` as a sidecar (strongly recommended)
3. A Supervisor-managed connection proxy as an alternative
4. Configuration guidelines

---

## 1. Why Wasm Cannot Share a Connection Pool

In native Rust, `sqlx::Pool` or `bb8` hold a pool of live TCP sockets that are shared
across all async tasks. This works because:
- Multiple tasks share the same OS process and the same heap
- The pool is a static reference shared across all request handlers

In Wasm/WASI:
- Each Wasm instance has **its own isolated linear memory** (Shared-Nothing)
- There is no shared heap between two Wasm instances
- Each instance creates its own connections when it starts
- The pool inside instance A is invisible to instance B

**Consequence**: 100 concurrent instances of `api-users` × 5 connections per instance
= 500 PostgreSQL connections. PostgreSQL's default `max_connections = 100` will be
immediately exhausted.

---

## 2. Solution A — pgBouncer Sidecar (Recommended)

Run `pgBouncer` on every node. All Wasm instances connect to `localhost:5432`
(pgBouncer), which multiplexes them onto a smaller pool of real PostgreSQL connections.

```
Wasm Instance 1 ──► localhost:5432 (pgBouncer)
Wasm Instance 2 ──►               │
Wasm Instance 3 ──►               │  (pool: 10 real connections)
Wasm Instance N ──►               └──► PostgreSQL server:5432
```

### pgBouncer Configuration

```ini
# /etc/pgbouncer/pgbouncer.ini
[databases]
; "mydb" is the alias the Wasm apps connect to
mydb = host=db.internal port=5432 dbname=mydb

[pgbouncer]
; Transaction pooling = a connection is only held during an active transaction.
; This is the most efficient mode for Wasm (stateless, short transactions).
pool_mode = transaction

; How many real connections to keep to PostgreSQL
default_pool_size = 20
max_client_conn   = 1000    ; Maximum simultaneous Wasm connections

listen_port = 5432
listen_addr = 127.0.0.1

auth_type = md5
auth_file = /etc/pgbouncer/userlist.txt

logfile  = /var/log/pgbouncer/pgbouncer.log
pidfile  = /var/run/pgbouncer/pgbouncer.pid
```

### Wasm App: Connect to pgBouncer

No code change needed in the Wasm app. It connects to `localhost:5432` exactly as before.
The `DATABASE_URL` injected by the Supervisor points to pgBouncer, not PostgreSQL directly.

```bash
# Supervisor injects:
DATABASE_URL=postgres://user:pass@127.0.0.1:5432/mydb

# pgBouncer proxies to:
postgres://user:pass@db.internal:5432/mydb
```

### Supervisor Config Integration

```toml
# /etc/wasm-node/config.toml
[database]
# pgBouncer local address injected as DATABASE_URL for all apps
# (apps can override this via their own env vars)
default_pgbouncer_url = "postgres://127.0.0.1:5432"
```

---

## 3. Solution B — Supervisor Connection Proxy

If you cannot run pgBouncer (e.g. embedded/edge deployment), implement a minimal
TCP multiplexer inside the Supervisor.

The Supervisor listens on a local port (e.g. `127.0.0.1:5433`) and maintains a
pool of real connections to the database. Each Wasm instance connects to the Supervisor's
port; the Supervisor forwards queries to an available pooled connection.

```rust
// crates/supervisor/src/db_proxy.rs
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use std::sync::Arc;

/// A simple TCP connection pool proxy.
/// Does NOT understand the PostgreSQL protocol — it does raw byte forwarding.
/// Use pgBouncer for full PostgreSQL protocol support.
///
/// This is only appropriate for simple use cases or non-SQL protocols.
pub struct ConnectionProxy {
    /// Maximum simultaneous connections to the real backend.
    pool_semaphore: Arc<Semaphore>,
    backend_addr: String,
}

impl ConnectionProxy {
    pub fn new(max_connections: usize, backend_addr: String) -> Self {
        ConnectionProxy {
            pool_semaphore: Arc::new(Semaphore::new(max_connections)),
            backend_addr,
        }
    }

    pub async fn run(&self, listen_addr: &str) {
        let listener = TcpListener::bind(listen_addr).await
            .expect("db proxy bind failed");
        tracing::info!(addr = listen_addr, "DB proxy listening");

        loop {
            let (client, _) = listener.accept().await.unwrap();
            let sem = self.pool_semaphore.clone();
            let backend = self.backend_addr.clone();

            tokio::spawn(async move {
                // Acquire a slot in the pool
                let _permit = sem.acquire_owned().await.unwrap();

                // Connect to the real backend
                match TcpStream::connect(&backend).await {
                    Ok(server) => {
                        // Bidirectional copy
                        let (mut cr, mut cw) = client.into_split();
                        let (mut sr, mut sw) = server.into_split();
                        tokio::select! {
                            _ = tokio::io::copy(&mut cr, &mut sw) => {},
                            _ = tokio::io::copy(&mut sr, &mut cw) => {},
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "db proxy: backend connect failed"),
                }
                // _permit dropped here → slot returned to pool
            });
        }
    }
}
```

**Limitation**: This raw TCP proxy does not handle PostgreSQL session state (prepared
statements, `SET` commands, etc.). Use `pool_mode = transaction` in pgBouncer to avoid
these issues.

---

## 4. Connection Limit per App

Each app should be configured with a maximum connection count to prevent a single app
from monopolizing the pgBouncer pool.

```rust
// crates/common/src/types.rs (add to AppConfig)
/// Maximum simultaneous database connections this app is allowed to hold.
/// The Supervisor does NOT enforce this directly (pgBouncer does via max_client_conn
/// and per-user limits). This is stored for documentation/audit purposes.
pub db_max_connections: u32,
```

```ini
# pgBouncer: per-user limits
# userlist.txt
"api_users_svc" "hashed_password"

# pgbouncer.ini — per-database per-user pool
[databases]
mydb = host=db.internal dbname=mydb pool_size=5 max_db_connections=20
```

---

## 5. Other Databases

| Database | Recommended pooler | Notes |
|----------|--------------------|-------|
| PostgreSQL | pgBouncer (transaction mode) | Industry standard |
| MySQL / MariaDB | ProxySQL | Similar to pgBouncer for MySQL |
| Redis | None needed | Redis handles 10k+ connections natively |
| SQLite | N/A | Must be used only from a single Wasm instance (not suitable for multi-tenant) |
| CockroachDB | Built-in load balancer | Handles connection pooling internally |

---

## 6. Health Check: pgBouncer Status

Add a pgBouncer health check to the node's startup sequence and the admin API.

```rust
// crates/node/src/main.rs
async fn check_pgbouncer(url: &str) -> bool {
    // pgBouncer responds to a minimal PostgreSQL handshake on its admin database
    // Simplest check: try to open a TCP connection
    tokio::net::TcpStream::connect(url).await.is_ok()
}
```

```toml
# node.toml
[database]
health_check_url = "127.0.0.1:5432"
health_check_interval_secs = 30
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### pgBouncer Setup
- [ ] pgBouncer is installed and running on every node (`systemctl status pgbouncer` is active)
- [ ] `pgbouncer.ini` is configured with `pool_mode = transaction` and correct `default_pool_size`
- [ ] The Supervisor injects `DATABASE_URL=postgres://127.0.0.1:5432/...` for apps that need a database
- [ ] A Wasm app using `sqlx` or `tokio-postgres` can connect through pgBouncer without code changes
- [ ] `psql -h 127.0.0.1 -p 5432 -U user mydb` connects successfully via pgBouncer

### Connection Limits
- [ ] `max_client_conn` in pgBouncer is set high enough to accommodate `max_instances × connections_per_instance`
- [ ] `default_pool_size` limits real connections to PostgreSQL to a safe number (e.g. 20)
- [ ] A Wasm app that opens more connections than its quota is queued by pgBouncer — not rejected immediately

### Health Check
- [ ] `check_pgbouncer("127.0.0.1:5432")` returns `true` when pgBouncer is running
- [ ] Node startup logs a warning if pgBouncer is unreachable (not a fatal error — some apps may not need it)
- [ ] The admin API exposes pgBouncer status at `GET /status/pgbouncer`

### Connection Proxy (Alternative)
- [ ] If pgBouncer is not installed, `ConnectionProxy` starts on port 5433 as a fallback
- [ ] `ConnectionProxy` correctly limits to `max_connections` simultaneous backend connections
- [ ] Connections beyond the limit are queued (semaphore blocks), not rejected

### Tests
- [ ] A test starts 50 concurrent Wasm instances each holding 1 DB connection, verifies pgBouncer stays below `default_pool_size` real connections
- [ ] A test verifies that the node starts and logs a warning (not an error) when pgBouncer is not available
