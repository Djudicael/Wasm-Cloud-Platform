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

Tests use [testcontainers](https://github.com/testcontainers/testcontainers-rs) to manage NATS containers.

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

### Run All E2E Tests

```bash
cargo test -p e2e -- --ignored --test-threads=1 --nocapture
```

**Important flags:**
- `--ignored`: E2E tests are marked with `#[ignore]` to prevent running in normal `cargo test`
- `--test-threads=1`: Run tests sequentially to avoid port conflicts
- `--nocapture`: Show test output (useful for debugging)

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
# Check Podman socket
ls -la /run/user/1000/podman/podman.sock

# If missing, start Podman service
systemctl --user enable --now podman.socket
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
  run: cargo test -p e2e -- --ignored --test-threads=1
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
4. Mark test with `#[ignore]` to exclude from normal runs
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
