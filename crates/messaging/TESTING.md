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

The focused `cargo test -p messaging url_auth --lib` tests do not start a
container. They cover percent-decoding, credential-free SDK diagnostics,
credentials-file conflicts, token/user-password URL forms, bounded server lists
and refusal of mixed/divergent authentication. The config crate additionally
checks TOML/CLI list preservation and all-server production TLS enforcement.

- **test_pub_sub_deploy_app**: Tests basic pub/sub functionality with NATS
- **test_jetstream_durable_replay**: Tests JetStream durable consumer and message replay

## Troubleshooting

### Port Already in Use

If you get "port already in use" errors:

Use an available ephemeral port or the owning fixture's exact recorded cleanup
path. Never stop/remove brokers by matching `nats` in a global container list:
other development stacks can be using them. Preserve unrelated containers and
volumes; verify cleanup against the fixture's actual recorded identities.

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
