# Full Stack Example: E-Commerce Platform

This guide walks through deploying a complete full-stack application on the Wasm Cloud Platform. We'll build:

1. **Order Service** — Public-facing API with JWT auth
2. **Payment Service** — Internal microservice with API key auth
3. **PostgreSQL** — Shared database (via pgBouncer)
4. **NATS** — Control plane messaging
5. **Keycloak** — OIDC provider for JWT authentication

## Table of Contents

1. [Architecture](#architecture)
2. [Prerequisites](#prerequisites)
3. [Step 1: Infrastructure](#step-1-infrastructure)
4. [Step 2: Payment Service](#step-2-payment-service)
5. [Step 3: Order Service](#step-3-order-service)
6. [Step 4: Deploy Everything](#step-4-deploy-everything)
7. [Step 5: Test End-to-End](#step-5-test-end-to-end)
8. [Step 6: Verify Internal Mesh](#step-6-verify-internal-mesh)
9. [Troubleshooting](#troubleshooting)

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              External Traffic                                 │
│                                                                              │
│   curl https://shop.example.com/api/orders                                  │
│         │                                                                    │
│         ▼                                                                    │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                         Pingora Proxy                                │   │
│  │  - TLS termination                                                   │   │
│  │  - JWT validation (Keycloak)                                         │   │
│  │  - Rate limiting: 500 req/s                                          │   │
│  │  - Route: shop.example.com → order-service:v1                       │   │
│  └────────────────────────────┬──────────────────────────────────────────┘   │
│                               │                                              │
│  ┌────────────────────────────▼──────────────────────────────────────────┐   │
│  │                      Namespace: production                             │   │
│  │                                                                         │   │
│  │  ┌──────────────────┐        ┌──────────────────┐                     │   │
│  │  │  Order Service   │───────►│ Payment Service  │                     │   │
│  │  │  (public API)    │        │  (internal)      │                     │   │
│  │  │                  │        │                  │                     │   │
│  │  │  /health  → none │        │  /process →      │                     │   │
│  │  │  /api/orders      │        │    api_key       │                     │   │
│  │  │    → roles[user] │        │                  │                     │   │
│  │  └──────────────────┘        └──────────────────┘                     │   │
│  │          │                            │                               │   │
│  │          │                            │                               │   │
│  │          ▼                            ▼                               │   │
│  │  ┌──────────────────────────────────────────────┐                   │   │
│  │  │  pgBouncer → PostgreSQL                      │                   │   │
│  │  │  (shared database)                            │                   │   │
│  │  └──────────────────────────────────────────────┘                   │   │
│  │                                                                         │   │
│  └─────────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│  Control Plane: NATS + JetStream                                             │
│  Auth: Keycloak OIDC                                                         │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Prerequisites

```bash
# 1. Rust toolchain
rustup target add wasm32-wasip2

# 2. NATS server
nats-server --version

# 3. PostgreSQL + pgBouncer
psql --version
pgbouncer --version

# 4. Platform binaries built
cargo build --release
wasm-node --version
wasm-ctl --version
```

---

## Step 1: Infrastructure

### 1.1 Start PostgreSQL

```bash
# Start PostgreSQL
sudo systemctl start postgresql

# Create database and user
sudo -u postgres psql << 'EOF'
CREATE DATABASE ecommerce;
CREATE USER ecommerce_user WITH PASSWORD 'ecommerce_pass';
GRANT ALL PRIVILEGES ON DATABASE ecommerce TO ecommerce_user;
EOF

# Create tables
psql -h 127.0.0.1 -U ecommerce_user -d ecommerce << 'EOF'
CREATE TABLE orders (
    id SERIAL PRIMARY KEY,
    user_id TEXT NOT NULL,
    amount DECIMAL(10,2) NOT NULL,
    currency TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    payment_id TEXT,
    created_at TIMESTAMP DEFAULT NOW()
);

CREATE TABLE payments (
    id SERIAL PRIMARY KEY,
    order_id INTEGER REFERENCES orders(id),
    amount DECIMAL(10,2) NOT NULL,
    status TEXT NOT NULL DEFAULT 'processing',
    created_at TIMESTAMP DEFAULT NOW()
);
EOF
```

### 1.2 Start pgBouncer

```bash
cat > /tmp/pgbouncer.ini << 'EOF'
[databases]
ecommerce = host=127.0.0.1 port=5432 dbname=ecommerce

[pgbouncer]
listen_port = 5432
listen_addr = 127.0.0.1
auth_type = md5
auth_file = /tmp/pgbouncer_users.txt
pool_mode = transaction
max_client_conn = 1000
default_pool_size = 20
EOF

echo '"ecommerce_user" "ecommerce_pass"' > /tmp/pgbouncer_users.txt

pgbouncer /tmp/pgbouncer.ini
```

### 1.3 Start NATS

```bash
cat > /tmp/nats-prod.conf << 'EOF'
port: 4222
http_port: 8222

jetstream {
    store_dir: "/tmp/nats-jetstream-prod"
    max_memory_store: 1GB
    max_file_store: 10GB
}

authorization {
    users: [
        { user: "platform", password: "platform-secret" }
    ]
}
EOF

nats-server -c /tmp/nats-prod.conf
```

### 1.4 Start Keycloak (for OIDC)

```bash
# Using Docker for quick setup
docker run -d \
  --name keycloak \
  -p 8080:8080 \
  -e KEYCLOAK_ADMIN=admin \
  -e KEYCLOAK_ADMIN_PASSWORD=admin \
  quay.io/keycloak/keycloak:24.0 \
  start-dev

# Wait for Keycloak to start
sleep 30

# Create realm
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh config credentials \
  --server http://localhost:8080 \
  --realm master \
  --user admin \
  --password admin

# Create realm "production"
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh create realms \
  -s realm=production \
  -s enabled=true

# Create client "ecommerce"
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh create clients \
  -r production \
  -s clientId=ecommerce \
  -s enabled=true \
  -s publicClient=false \
  -s redirectUris='["*"]' \
  -s serviceAccountsEnabled=true

# Create roles
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh create roles \
  -r production \
  -s name=user

docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh create roles \
  -r production \
  -s name=admin

# Create a test user
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh create users \
  -r production \
  -s username=testuser \
  -s enabled=true \
  -s email=test@example.com \
  -s credentials='[{"type":"password","value":"testpass","temporary":false}]'

# Assign role to user
docker exec keycloak \
  /opt/keycloak/bin/kcadm.sh add-roles \
  -r production \
  --uusername testuser \
  --rolename user
```

Get the Keycloak public key:

```bash
# The JWKS endpoint will be:
# http://localhost:8080/realms/production/protocol/openid-connect/certs
```

### 1.5 Start the Wasm Node

```bash
cat > /tmp/wasm-node-prod.toml << 'EOF'
[node]
node_id = "node-1"

[storage]
db_path = "/tmp/wasm-node-prod.redb"

[nats]
url = "nats://platform:platform-secret@127.0.0.1:4222"

[proxy]
http_port = 8081
https_port = 0

[admin]
port = 9090
artifact_port = 9091

[auth]
enabled = false

[runtime]
port_start = 10000
port_end = 19999

[database]
default_url = "postgres://ecommerce_user:ecommerce_pass@127.0.0.1:5432/ecommerce"
pgbouncer_addr = "127.0.0.1:5432"

[logging]
level = "info"
format = "text"

[gateway]
[gateway.oidc]
issuer_url = "http://127.0.0.1:8080/realms/production"
audience = "ecommerce"
EOF

wasm-node --config /tmp/wasm-node-prod.toml
```

---

## Step 2: Payment Service

### 2.1 Code

```rust
// payment-service/src/main.rs
use axum::{
    routing::post,
    Router, Json,
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Deserialize)]
struct PaymentRequest {
    order_id: i32,
    amount: f64,
    currency: String,
}

#[derive(Serialize)]
struct PaymentResponse {
    payment_id: String,
    status: String,
}

async fn process_payment(
    Json(req): Json<PaymentRequest>,
) -> Result<Json<PaymentResponse>, StatusCode> {
    // In production, this would call a real payment processor
    let payment_id = format!("pay_{}", uuid::Uuid::new_v4());

    tracing::info!(
        order_id = req.order_id,
        amount = req.amount,
        currency = %req.currency,
        "Payment processed"
    );

    Ok(Json(PaymentResponse {
        payment_id,
        status: "completed".to_string(),
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/process", post(process_payment));

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .unwrap();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

```toml
# payment-service/Cargo.toml
[package]
name = "payment-service"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
uuid = { version = "1", features = ["v4"] }
```

### 2.2 Build

```bash
cd payment-service
cargo build --release --target wasm32-wasip2
```

### 2.3 Manifest

```toml
# payment-service.toml
[app]
name = "payment-service"
version = "v1"
namespace = "production"
wasm_artifact = "./payment-service/target/wasm32-wasip2/release/payment-service.wasm"
wasm_bind_port = 8080

[fuel]
quota = 500_000_000
memory_pages = 1024
max_instances = 5
idle_timeout_secs = 300

[policy]
profile = "http_api"

# No external host — internal-only service
# Gateway config only for endpoint-level policies
[[gateway.endpoints]]
path = "/process"
methods = ["POST"]
auth = "api_key"
rate_limit = { requests_per_second = 100, burst_capacity = 20 }

[env]
LOG_LEVEL = "info"
```

---

## Step 3: Order Service

### 3.1 Code

```rust
// order-service/src/main.rs
use axum::{
    routing::{get, post},
    Router, Json, extract::State,
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;

#[derive(Clone)]
struct AppState {
    http: reqwest::Client,
    db_url: String,
    payment_api_key: String,
}

#[derive(Deserialize)]
struct CreateOrderRequest {
    user_id: String,
    amount: f64,
    currency: String,
    items: Vec<String>,
}

#[derive(Serialize)]
struct OrderResponse {
    order_id: i32,
    status: String,
    payment_id: Option<String>,
}

async fn health_check() -> &'static str {
    "OK"
}

async fn create_order(
    State(state): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<Json<OrderResponse>, StatusCode> {
    // 1. Call payment service internally
    let payment_resp = state.http
        .post("http://payment-service.internal/process")
        .header("X-Api-Key", &state.payment_api_key)
        .json(&json!({
            "order_id": 0,  // Will be updated after DB insert
            "amount": req.amount,
            "currency": req.currency,
        }))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Payment service call failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    if !payment_resp.status().is_success() {
        tracing::error!("Payment service returned: {}", payment_resp.status());
        return Err(StatusCode::BAD_GATEWAY);
    }

    let payment_result: serde_json::Value = payment_resp.json().await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    // 2. In production, insert into database here
    // For this example, we return a mock order
    tracing::info!(
        user_id = %req.user_id,
        amount = req.amount,
        payment_id = %payment_result["payment_id"].as_str().unwrap_or("unknown"),
        "Order created"
    );

    Ok(Json(OrderResponse {
        order_id: 42,
        status: "confirmed".to_string(),
        payment_id: payment_result["payment_id"].as_str().map(|s| s.to_string()),
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = AppState {
        http: reqwest::Client::new(),
        db_url: std::env::var("DATABASE_URL").unwrap(),
        payment_api_key: std::env::var("PAYMENT_API_KEY").unwrap_or_else(|_| "dev-key".to_string()),
    };

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/api/orders", post(create_order))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .unwrap();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

```toml
# order-service/Cargo.toml
[package]
name = "order-service"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

### 3.2 Build

```bash
cd order-service
cargo build --release --target wasm32-wasip2
```

### 3.3 Manifest

```toml
# order-service.toml
[app]
name = "order-service"
version = "v1"
namespace = "production"
wasm_artifact = "./order-service/target/wasm32-wasip2/release/order-service.wasm"
wasm_bind_port = 8080

[fuel]
quota = 1_000_000_000
memory_pages = 2048
max_instances = 10
idle_timeout_secs = 300

[policy]
profile = "http_api"

[gateway]
host = "shop.example.com"

[gateway.auth]
policy = "roles"
allowed_roles = ["user", "admin"]
client_id = "ecommerce"

[gateway.cors]
allowed_origins = ["https://shop.example.com"]
allow_credentials = true
max_age_secs = 3600

[gateway.rate_limit]
requests_per_second = 500
burst_capacity = 100
distributed = true

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/api/orders"
methods = ["POST"]
auth = "roles"
allowed_roles = ["user", "admin"]
rate_limit = { requests_per_second = 100, burst_capacity = 20 }

[env]
LOG_LEVEL = "info"
DATABASE_URL = "postgres://ecommerce_user:ecommerce_pass@127.0.0.1:5432/ecommerce"
PAYMENT_API_KEY = "ak_payment_internal_123"
```

---

## Step 4: Deploy Everything

### 4.1 Set up the CLI

```bash
# Point wasm-ctl to the node
export WASM_CTL_NODE_API=http://127.0.0.1:9090
export WASM_CTL_NATS_URL=nats://platform:platform-secret@127.0.0.1:4222
```

### 4.2 Deploy payment-service

```bash
wasm-ctl deploy --manifest ./payment-service.toml

# Verify
wasm-ctl app list --namespace production
# → Apps in namespace 'production':
#   payment-service:v1 (0 instances — waiting for first request)
```

### 4.3 Deploy order-service

```bash
wasm-ctl deploy --manifest ./order-service.toml

# Verify
wasm-ctl app list --namespace production
# → Apps in namespace 'production':
#   payment-service:v1 (0 instances)
#   order-service:v1 (0 instances)
```

### 4.4 Add routes

```bash
# Route for order-service (public)
wasm-ctl routes add \
  --host shop.example.com \
  --app order-service:v1 \
  --path-prefix /

# No route needed for payment-service — it's internal-only
```

### 4.5 Store the payment API key

```bash
# The payment service needs an API key to accept internal requests
wasm-ctl gateway api-key add payment-service:v1 \
  --namespace production \
  --name "order-service" \
  --scopes "/process" \
  --key "ak_payment_internal_123"
```

---

## Step 5: Test End-to-End

### 5.1 Get a JWT token from Keycloak

```bash
# Request a token
curl -X POST http://127.0.0.1:8080/realms/production/protocol/openid-connect/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "grant_type=password" \
  -d "client_id=ecommerce" \
  -d "username=testuser" \
  -d "password=testpass" \
  | jq -r '.access_token' > /tmp/jwt_token.txt

JWT_TOKEN=$(cat /tmp/jwt_token.txt)
echo "Token: $JWT_TOKEN"
```

### 5.2 Test public health endpoint (no auth)

```bash
curl -v http://127.0.0.1:8081/health \
  -H "Host: shop.example.com"

# Expected:
# HTTP/1.1 200 OK
# OK
```

### 5.3 Test protected endpoint (JWT + role)

```bash
curl -v http://127.0.0.1:8081/api/orders \
  -H "Host: shop.example.com" \
  -H "Authorization: Bearer $JWT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "user_id": "user-123",
    "amount": 99.99,
    "currency": "USD",
    "items": ["widget", "gadget"]
  }'

# Expected:
# HTTP/1.1 200 OK
# {"order_id":42,"status":"confirmed","payment_id":"pay_xxxx-xxxx"}
```

### 5.4 Test without JWT (should fail)

```bash
curl -v http://127.0.0.1:8081/api/orders \
  -H "Host: shop.example.com" \
  -H "Content-Type: application/json" \
  -d '{"user_id":"user-123","amount":99.99,"currency":"USD","items":["widget"]}'

# Expected:
# HTTP/1.1 401 Unauthorized
```

### 5.5 Test CORS preflight

```bash
curl -v -X OPTIONS http://127.0.0.1:8081/api/orders \
  -H "Host: shop.example.com" \
  -H "Origin: https://shop.example.com" \
  -H "Access-Control-Request-Method: POST"

# Expected:
# HTTP/1.1 204 No Content
# access-control-allow-origin: https://shop.example.com
# access-control-allow-methods: GET, POST, PUT, DELETE, PATCH, OPTIONS
```

---

## Step 6: Verify Internal Mesh

### 6.1 Check that order-service calls payment-service internally

The `order-service` makes an internal call to `http://payment-service.internal/process`. This should happen transparently.

### 6.2 Verify namespace isolation

Deploy an app in a different namespace and try to reach payment-service:

```bash
# Deploy a "hacker" app in the "staging" namespace
wasm-ctl deploy \
  --app hacker \
  --version v1 \
  --namespace staging \
  --wasm ./some-app.wasm

# From inside this app, trying to connect to payment-service.internal
# → Connection refused (cross-namespace block)
```

### 6.3 Verify API key enforcement on payment-service

```bash
# Call payment-service without API key (should fail)
curl -v -X POST http://127.0.0.1:8081/process \
  -H "Host: payment-service.internal"

# Expected: 401 Unauthorized
```

### 6.4 Check platform logs

```bash
# Node logs show the internal routing
# Look for:
# "Virtual DNS resolved payment-service.internal → 127.0.0.1"
# "socket_addr_check: same namespace, allowing connection"
# "internal proxy: API key validated for /process"
```

---

## Troubleshooting

### "No route found for host"

```bash
# Check the route exists
wasm-ctl routes list

# Re-add if missing
wasm-ctl routes add --host shop.example.com --app order-service:v1
```

### "401 Unauthorized" on /health

```bash
# Check gateway config
wasm-ctl gateway show order-service:v1

# The /health endpoint should have auth = "none"
# If not, reset and reconfigure:
wasm-ctl gateway reset order-service:v1
wasm-ctl gateway set-auth order-service:v1 --policy none
```

### "502 Bad Gateway" on /api/orders

```bash
# Check if order-service instances are running
wasm-ctl instances

# Check node health
wasm-ctl node health

# Check supervisor logs for spawn errors
tail -f /var/log/wasm-node/node.log | grep order-service
```

### "Connection refused" on internal calls

```bash
# Check if both apps are in the same namespace
wasm-ctl app list --namespace production

# Check if payment-service is registered in the namespace registry
# (This is automatic, but verify the app is deployed)
```

### JWT validation fails

```bash
# Check JWKS is reachable from the node
curl http://127.0.0.1:8080/realms/production/protocol/openid-connect/certs

# Verify the token
curl -X POST http://127.0.0.1:8080/realms/production/protocol/openid-connect/token/introspect \
  -d "token=$JWT_TOKEN" \
  -d "client_id=ecommerce"

# Check node gateway config
wasm-ctl node config --json | jq '.gateway.oidc'
```

### Rate limiting kicks in too early

```bash
# Check current rate limit config
wasm-ctl gateway show order-service:v1

# Update if needed
wasm-ctl gateway set-rate-limit order-service:v1 \
  --rps 1000 \
  --burst 200 \
  --distributed
```

---

## Cleanup

```bash
# Remove apps
wasm-ctl remove order-service:v1
wasm-ctl remove payment-service:v1

# Stop node
pkill wasm-node

# Stop NATS
pkill nats-server

# Stop PostgreSQL
sudo systemctl stop postgresql

# Remove data
rm -f /tmp/wasm-node-prod.redb
rm -rf /tmp/nats-jetstream-prod
```

---

## What You Built

| Component | Technology | Purpose |
|-----------|-----------|---------|
| **Order Service** | Rust + Axum (Wasm) | Public API, JWT auth, CORS |
| **Payment Service** | Rust + Axum (Wasm) | Internal microservice, API key auth |
| **Proxy / Gateway** | Pingora (built-in) | TLS, routing, rate limit, circuit breaker |
| **Database** | PostgreSQL + pgBouncer | Shared transactional database |
| **Control Plane** | NATS + JetStream | Deploy events, config sync, KV store |
| **Auth** | Keycloak OIDC | JWT issuance and validation |
| **Internal Mesh** | Virtual DNS + Network Interceptor | Transparent East-West communication |

This is a production-ready pattern that scales horizontally by adding more platform nodes behind a load balancer, all sharing the same NATS cluster and database.
