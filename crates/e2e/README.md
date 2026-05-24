# End-to-End Tests

This crate contains integration/E2E tests for the Wasm Cloud Platform.

## Prerequisites

### 1. Build the Platform Binaries

```bash
# Build the node binary
cargo build --bin wasm-node

# Or for faster tests
cargo build --bin wasm-node --release
```

### 2. Build the Test WASM App

```bash
# IMPORTANT: Use tokio_unstable flag for WASI support
RUSTFLAGS='--cfg tokio_unstable' cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release
```

### 3. Container Runtime

Tests use the shared host container runtime helper to start NATS containers through Podman or Docker directly.

**Docker:**
```bash
# Ensure Docker is running
docker ps
```

**Podman (WSL/Linux):**
```bash
# Ensure Podman is running
podman ps

# The tests will automatically detect and use Podman if available
```

## Running Tests

### Run Default E2E Suite

```bash
cargo test -p e2e
```

### Run Live Cluster Regressions

```bash
cargo test -p e2e test_live_cluster_registry_drives_artifact_authorize_audience_set -- --ignored --test-threads=1 --nocapture
```

**Important flags for live cluster tests:**
- `--ignored`: live multi-process regressions stay opt-in for local runs
- `--test-threads=1`: run sequentially to avoid port conflicts
- `--nocapture`: show node/fixture output during failures

### Run Individual Tests

```bash
# Deploy and serve test
cargo test -p e2e test_deploy_and_serve_http -- --ignored --nocapture

# Route management test
cargo test -p e2e test_route_add_and_serve -- --ignored --nocapture

# Hot swap zero downtime test
cargo test -p e2e test_hot_swap_zero_downtime -- --ignored --nocapture

# Node restart test
cargo test -p e2e test_node_restart_restores_state -- --ignored --nocapture

# Fuel exhaustion test
cargo test -p e2e test_fuel_exhaustion_returns_4xx -- --ignored --nocapture

# Secret rotation test
cargo test -p e2e test_secret_rotation -- --ignored --nocapture

# Cluster registry -> audience-bound manifest regression
cargo test -p e2e test_live_cluster_registry_drives_artifact_authorize_audience_set -- --ignored --test-threads=1 --nocapture
```

## Tests Overview

### Integration Tests

| Test | Description | What It Verifies |
|------|-------------|------------------|
| `test_deploy_and_serve_http` | Deploy hello-axum.wasm and send HTTP requests | End-to-end deployment flow works |
| `test_route_add_and_serve` | Add/remove routes and verify routing | Route management works correctly |
| `test_hot_swap_zero_downtime` | Deploy new version while traffic flows | Zero failed requests during hot-swap |
| `test_node_restart_restores_state` | Restart node and verify state restored | State persists across restarts |
| `test_fuel_exhaustion_returns_4xx` | Request with tiny fuel limit | Returns 4xx, not 500 error |
| `test_secret_rotation` | Rotate a secret and verify app continues | Secret rotation works |
| `test_live_cluster_registry_drives_artifact_authorize_audience_set` | Start a live two-node cluster and authorize artifact transfer | Registry convergence and per-node manifest audience binding work |

## Troubleshooting

### "wasm-node binary not found"

Build the node binary:
```bash
cargo build --bin wasm-node
```

### "hello_axum.wasm not found"

Build the test app:
```bash
RUSTFLAGS='--cfg tokio_unstable' cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release
```

### "Failed to start NATS container"

**Docker users:**
```bash
# Check Docker is running
docker ps

# Try pulling NATS image manually
docker pull nats:2.10-alpine
```

**Podman users:**
```bash
# Check Podman is available
podman info
```

### Port conflicts

If you see "address already in use" errors:

```bash
# Stop any running NATS containers
podman stop $(podman ps -a | grep nats | awk '{print $1}')
podman rm $(podman ps -a | grep nats | awk '{print $1}')

# Or for Docker
docker stop $(docker ps -a | grep nats | awk '{print $1}')
docker rm $(docker ps -a | grep nats | awk '{print $1}')
```

### Tests timing out

- Increase timeouts in the test code if your machine is slow
- Ensure you have enough resources (CPU/memory) available
- Run tests sequentially with `--test-threads=1`

## CI Integration

To run these tests in CI:

```yaml
# .github/workflows/e2e.yml
- name: Build binaries
  run: |
    cargo build --bin wasm-node
    RUSTFLAGS='--cfg tokio_unstable' cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release

- name: Run E2E tests
  run: cargo test -p e2e

- name: Run cluster registry live regression
  run: cargo test -p e2e test_live_cluster_registry_drives_artifact_authorize_audience_set -- --ignored --test-threads=1 --nocapture
```

## Adding New Tests

1. Create a new test file in `crates/e2e/tests/`
2. Import the harness: `mod harness;`
3. Use harness utilities:
   - `NatsContainer::start(port)` - Start NATS
   - `NodeProcess::start(...)` - Start a node
   - `deploy_app(...)` - Deploy an app
   - `add_route(...)` - Add a route
   - `send_request(...)` - Send HTTP request
4. Mark only the heavier live multi-node regressions with `#[ignore]`
5. Document what the test verifies

Example:
```rust
mod harness;
use harness::*;

#[tokio::test]
#[ignore]
async fn test_my_feature() {
    let nats = NatsContainer::start(4230).await.unwrap();
    let bus = nats.connect().await.unwrap();
    // ... test logic
}
```
