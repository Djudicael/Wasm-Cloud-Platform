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

## Agentic ecosystem development

The preferred agentic/harness feature-validation topology is a native wasm-node
process on Linux/WSL, running the actual WASI applications. Supporting NATS,
PostgreSQL and RustFS can run in checkout-scoped Podman containers. Preserve
explicit secrets, migrations, deployment, route, OIDC and network-policy checks;
changing the host topology does not bypass the platform execution path.

Keep application identities and node-local .internal routing unchanged. Run the
inference engine or compatible providers behind explicit policy-approved HTTP
endpoints. A native application-only or standalone Wasmtime rehearsal is useful
for narrower tests but is not the same end-to-end validation as wasm-node.

The sibling `wasmcloud_platform_harness` repository owns its Rust development
controller. Its `agentic-dev platform start|smoke|status|test|stop|logs|destroy`
command group implements the native-node lifecycle with real WASI components
and checkout-scoped NATS/PostgreSQL. Read that repository's
`crates/agentic-dev/README.md` for the supported flags and fixture-only E2E
contract. It is separate from the standalone native-app and Wasmtime modes.
On first use or node replacement, `--grant-ebpf-capabilities` explicitly grants
the private node copy the required monitoring capabilities. BTF, readable
tracefs and the pinned BPF build tools are prerequisites; disabling eBPF cannot
exercise authenticated internal mesh calls. See [eBPF](ebpf.md) and
[mesh attribution](internal-mesh.md).
Firecracker validation is reserved for later advanced integration rehearsals.
Native-host tests do not establish guest-kernel, VM networking or full isolation
behavior; retain a separate rehearsal gate for those concerns. Historical
microVM results remain valid for their recorded artifacts and scope.
