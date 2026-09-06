# wasm-ctl

CLI tool for managing the Wasm Cloud Platform.

## Overview

`wasm-ctl` is the command-line interface for interacting with the Wasm Cloud Platform. It communicates with running nodes via NATS events and the HTTP admin API, providing a convenient interface for operators to deploy, manage, and inspect Wasm applications across the cluster.

## Architecture

The CLI follows a standard command-and-handler pattern:

1. **Command Parsing** — Clap-based CLI with subcommands mapped to `*Cmd` structs.
2. **Client Layer** — Builds HTTP clients for the Admin API and NATS connections for event-based commands.
3. **Handler Execution** — Each command handler interacts with the platform through the appropriate transport (HTTP for synchronous operations, NATS for event-driven operations).
4. **Output Formatting** — Results are formatted and printed to stdout/stderr.

For a private NATS PKI, configure `--nats-ca-cert`,
`--nats-client-cert`, and `--nats-client-key` (or the corresponding
`WASM_CTL_NATS_*` environment variables). The client certificate and key must
be supplied together. `--nats-creds` remains available for NATS user/account
credentials and can be combined with mutual TLS.

### Commands

| Command | Description |
|---------|-------------|
| `deploy` | Deploy a Wasm application to the cluster |
| `remove` | Remove a deployed application |
| `list` | List deployed applications |
| `instances` | Show running instances across the cluster |
| `routes` | Add or remove host routes; `list` currently redirects operators to `status` |
| `secrets` | Manage application secrets |
| `app` | Application-level operations |
| `logs` | Stream application logs |
| `logging` | Configure logging levels |
| `status` | Platform status overview |
| `platform` | Platform information |
| `gc` | Trigger garbage collection |
| `node` | Node management operations |
| `cluster` | Cluster-wide operations |
| `billing` | Billing and usage information |
| `policy` | Policy management |
| `gateway` | Gateway configuration |

### Remote deploys

`wasm-ctl deploy` supports three artifact sources:

- local file via `--wasm`
- remote URL via `--artifact-url` plus `--sha256`
- OCI reference via `--artifact-ref`

Remote artifact deploys use the deploy ingress endpoint. Configure it with:

- `--deploy-api`
- or `WASM_CTL_DEPLOY_API`

This can be separate from `--node-api` / `WASM_CTL_NODE_API`, which remain relevant for local artifact upload and other node/admin operations.

Examples:

```bash
wasm-ctl deploy --app hello --version v1 --wasm ./hello.wasm

wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello:v1 \
  --artifact-credential ghcr-reader
```

Manifest-driven deploys can now publish public route bindings as part of the
deploy itself. Both of these shapes are supported:

```toml
[gateway]
host = "www.example.com"

[[gateway.routes]]
host = "api.example.com"
path_prefix = "/v1"
strip_prefix = false
```

`gateway.host` is kept as the compatibility shorthand for the default `/`
route. `[[gateway.routes]]` supports additional host and path bindings for the
same app.

If deploy ingress is running with `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS=true`,
mutable OCI tag refs like `oci://...:v1` are rejected and callers must use
digest-pinned refs.

Optional signed-artifact metadata can be attached on remote deploys:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm ed25519 \
  --artifact-issuer https://token.actions.githubusercontent.com \
  --artifact-repository example-org/hello \
  --artifact-namespace production
```

For Cosign-style signed payload verification with a public key:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm cosign-ed25519 \
  --artifact-signature-payload '{"critical":{"identity":{"docker-reference":"ghcr.io/example-org/hello"},"image":{"docker-manifest-digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},"type":"cosign container image signature"},"optional":{"issuer":"https://token.actions.githubusercontent.com","repository":"example-org/hello","namespace":"production"}}'
```

`cosign-ed25519` verifies a Cosign-style signed payload with the supplied public
key and then applies the normal deploy-ingress issuer/repository/namespace
policy checks. It does not implement Fulcio/Rekor verification by itself.

For Sigstore bundle verification:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm sigstore-bundle \
  --artifact-signature-payload "$(cat artifact.sigstore.json)" \
  --artifact-identity user@example.com \
  --artifact-issuer https://github.com/login/oauth
```

This mode verifies a Sigstore bundle against Sigstore’s trust root. The current
policy binding for it is issuer + identity.

Artifact fetch credentials are managed separately from runtime app secrets:

```bash
export WASM_CTL_DEPLOY_API=https://deploy.example.com
wasm-ctl secrets set-artifact-credential --key ghcr-reader
```

Those credentials are stored in the deploy-ingress credential store under `_platform/artifact-credentials:v1` and are used only for deploy-time artifact fetch.

For OCI refs, the node resolves tags or digests during deploy ingress, fetches the registry manifest if needed, then downloads the final blob and verifies its content hash before publishing the normal deploy event.

## Public API

This crate produces the `wasm-ctl` binary. It is not intended to be used as a library.

### Key Types

- `*Cmd` structs — Parsed representations of each CLI subcommand
- `build_http_client()` — Constructs the HTTP client for Admin API communication
- `update_config()` — Fetches and updates CLI configuration from the platform

## Known Issues & Improvements

### Reliability and error reporting

- `build_http_client()` returns construction failures as `Result`. If a supplied bearer token contains invalid header characters, however, the CLI warns and builds an unauthenticated client. Commands should fail before making a request in that case.
- GC configuration update fetches fall back to `GcConfig::default()` on transport or decode failure. A partial update can therefore overwrite live values with defaults.
- The log command parses each response chunk independently. SSE events split across chunks or using multiple `data:` lines can be lost or malformed, and connection failures are printed without returning a failing exit status.

### Local I/O

- Local deployment reads the complete Wasm file before upload. The node enforces an artifact-size limit, but CLI memory usage still scales with the artifact.
- Billing commands open redb and write exports synchronously. They are operator commands rather than server hot paths, but large billing stores can block the CLI for a significant time.

### Command boundaries

- `instances` reports the cluster-wide list and has no `--app` or `--health` filter.
- `routes` supports host-level `add` and `remove`. Path-prefix bindings belong in `[[gateway.routes]]` in a deployment manifest.
- `secrets` supports `set`, `set-artifact-credential`, and `delete`; it intentionally does not list secret values.
- The proxy understands API-key records, but `wasm-ctl gateway` has no API-key lifecycle subcommand.

### Test coverage

Deploy manifests, remote artifact inputs, gateway rate-limit mode, secret target selection, and node-drain parsing have focused tests. Several display-oriented and streaming commands still lack end-to-end CLI coverage.

## Security Considerations

- Runtime secret set operations fetch the authoritative node registry and encrypt one event for each active node's advertised X25519 transport key. Secret names, application IDs, and target node IDs remain visible to authorized NATS observers.
- Secret deletion targets every recorded node, including stale nodes, so a node that reconnects cannot retain a value merely because it was offline during revocation.
- Prefer the hidden prompt or a mode-0600 `--value-file`; inline `--value` can be exposed through shell history or process inspection.
- NATS supports a private CA, credentials, and mutual TLS through the documented flags and environment variables. The HTTP admin/deploy client currently has no custom-CA flag, so private-PKI deployments must provide trust through the host certificate store.
- An invalid bearer-token header currently causes a warning followed by an unauthenticated request. Treat the warning as a command failure until the CLI is changed to reject it.

The previous version of this file accidentally contained concatenated copies of the e2e, eBPF monitor, internal gateway, and node READMEs. Those documents remain in their owning crate directories; they are not part of the `wasm-ctl` crate reference.
