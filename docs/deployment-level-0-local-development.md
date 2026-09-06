# Level 0 - Local Development

Use this guide when you want the shortest path to a working node on one machine. This level is for development only. It is not a production posture.

See also: [`deployment-levels.md`](deployment-levels.md)

## When To Choose This Level

- you are developing on one machine
- you do not need persistent production-grade secret material
- the admin plane stays on loopback
- local convenience matters more than operational hardening

## What To Install

1. Rust with the `wasm32-wasip2` target
2. NATS with JetStream enabled
3. optional PostgreSQL if you are testing database-backed apps

Use the installation steps in [`getting-started.md`](getting-started.md).

## Starting Config

Start from:

- `config/dev.toml`

Recommended choices:

- admin bind on loopback
- artifact bind on loopback
- local NATS
- `runtime.key_source = "generate"` or `passphrase-env:...`
- no external routable artifact advertisement

## Basic Setup Steps

1. build the platform:
   ```bash
   export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
   cargo build --release
   ```
2. start local NATS with JetStream
3. start the node:
   ```bash
   "$CARGO_TARGET_DIR/release/wasm-node" --config config/dev.toml
   ```
4. deploy a test app:
   ```bash
   wasm-ctl deploy --app hello-world --version v1 --wasm hello-world.wasm
   ```

## Settings To Keep

- keep the admin API on loopback
- do not treat generated keys as durable production state
- do not expose the artifact plane publicly

## Validation

- `wasm-ctl node health`
- `curl http://localhost:9090/metrics`
- deploy and undeploy one app successfully

## Move To Level 1 When

- you need restart persistence for secrets and node state
- you want admin authentication enabled
- you are running a private internal service instead of a developer laptop

Next guide:

- [`Level 1 - Single-Node Private Environment`](deployment-level-1-single-node-private.md)
