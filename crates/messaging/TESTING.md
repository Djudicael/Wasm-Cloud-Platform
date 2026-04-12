# Testing the Messaging Crate

This crate uses [testcontainers](https://github.com/testcontainers/testcontainers-rs) to spin up NATS containers for integration tests.

## Running Tests

### With Docker

If you have Docker installed and running:

```bash
cargo test -p messaging
```

### With Podman (WSL/Linux)

The tests automatically detect and use Podman if available. The test setup checks for:
- Podman socket at `/run/user/1000/podman/podman.sock`
- Automatically configures `DOCKER_HOST` environment variable
- Disables Ryuk (often needed for Podman)

Simply run:

```bash
cargo test -p messaging
```

### Manual Configuration

If auto-detection doesn't work, you can manually set environment variables:

```bash
DOCKER_HOST=unix:///run/user/1000/podman/podman.sock \
TESTCONTAINERS_RYUK_DISABLED=true \
cargo test -p messaging
```

Or uncomment the `DOCKER_HOST` line in `.cargo/config.toml`:

```toml
[env]
TESTCONTAINERS_RYUK_DISABLED = "true"
DOCKER_HOST = "unix:///run/user/1000/podman/podman.sock"
```

## Tests Included

- **test_pub_sub_deploy_app**: Tests basic pub/sub functionality with NATS
- **test_jetstream_durable_replay**: Tests JetStream durable consumer and message replay

## Troubleshooting

### Port Already in Use

If you get "port already in use" errors:

```bash
# Stop and remove existing NATS containers
podman stop $(podman ps -a | grep nats | awk '{print $1}')
podman rm $(podman ps -a | grep nats | awk '{print $1}')
```

### Podman Socket Not Found

Ensure Podman is running and the socket exists:

```bash
podman info | grep sock
ls -la /run/user/1000/podman/podman.sock
```

If the socket doesn't exist, start the Podman service:

```bash
systemctl --user enable --now podman.socket
```
