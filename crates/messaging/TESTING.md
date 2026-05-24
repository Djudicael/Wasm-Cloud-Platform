# Testing the Messaging Crate

This crate uses the shared host container runtime helper to spin up NATS containers through Podman or Docker directly.

## Running Tests

### With Docker

If you have Docker installed and running:

```bash
cargo test -p messaging
```

### With Podman (WSL/Linux)

The tests automatically detect and use Podman if available.

Simply run:

```bash
cargo test -p messaging
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
