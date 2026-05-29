# deploy-ingress

Control-plane ingress for remote Wasm artifact deploys.

## Overview

`deploy-ingress` is the platform-facing process that accepts deploy intent from CI/CD, fetches remote Wasm artifacts, verifies them, stores them in the platform artifact plane, and publishes the normal `DeployApp` event to the cluster.

This keeps remote deploys off the node admin API and removes the need for CI to upload a local `.wasm` to one node first.

## Responsibilities

- accept `POST /deploy/intent`
- accept `PUT /deploy/artifact-credentials`
- accept `GET /artifacts/{sha256}/verification`
- fetch HTTP(S) and OCI-based Wasm artifacts
- verify artifact digests before publication
- store raw Wasm in the artifact store
- serve artifacts back to nodes through signed transfer manifests
- publish `DeployApp` and related gateway updates over NATS
- maintain a lightweight active-node registry from NATS cluster events

## API

### Health

```text
GET /health
```

Returns a simple liveness payload.

### Deploy Intent

```text
POST /deploy/intent
```

Accepts [`DeployIntentRequest`](../common/src/deploy.rs) JSON and returns `202 Accepted` with `DeployIntentResponse` on success.

### Artifact Credential Storage

```text
PUT /deploy/artifact-credentials
```

Stores a deploy-time artifact fetch credential under `_platform/artifact-credentials:v1`.

This credential store is separate from application runtime secret injection. It exists only so deploy ingress can authenticate to external registries or artifact hosts.

### Artifact Verification Lookup

```text
GET /artifacts/{sha256}/verification
```

Returns the stored verification record for a previously accepted remote artifact.

## Configuration

The binary is configured through CLI flags or environment variables.

Important variables:

- `WASM_DEPLOY_INGRESS_ID`
- `WASM_DEPLOY_INGRESS_NATS_URL`
- `WASM_DEPLOY_INGRESS_NATS_CREDS`
- `WASM_DEPLOY_INGRESS_DB_PATH`
- `WASM_DEPLOY_INGRESS_BIND_ADDRESS`
- `WASM_DEPLOY_INGRESS_PORT`
- `WASM_DEPLOY_INGRESS_ARTIFACT_PORT`
- `WASM_DEPLOY_INGRESS_ADVERTISED_ARTIFACT_URL`
- `WASM_DEPLOY_INGRESS_AUTH_ENABLED`
- `WASM_DEPLOY_INGRESS_AUTH_READ_TOKEN`
- `WASM_DEPLOY_INGRESS_AUTH_WRITE_TOKEN`
- `WASM_DEPLOY_INGRESS_KEY_SOURCE`
- `WASM_DEPLOY_INGRESS_KEY_FILE`
- `WASM_DEPLOY_INGRESS_AUDIT_PATH`
- `WASM_DEPLOY_INGRESS_HA_ENABLED`
- `WASM_DEPLOY_INGRESS_HA_LEASE_BUCKET`
- `WASM_DEPLOY_INGRESS_CREDENTIAL_BUCKET`
- `WASM_DEPLOY_INGRESS_HA_LEASE_TTL_SECS`
- `WASM_DEPLOY_INGRESS_HA_LEASE_REFRESH_SECS`
- `WASM_DEPLOY_INGRESS_REQUIRE_SIGNATURE`
- `WASM_DEPLOY_INGRESS_ALLOWED_ISSUERS`
- `WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES`
- `WASM_DEPLOY_INGRESS_ALLOWED_NAMESPACES`

Today there is no dedicated TOML config loader for this binary. The production shape is an environment file plus a systemd unit.

See:

- [config/deploy-ingress.env.example](../../config/deploy-ingress.env.example)
- [systemd/wasm-deploy-ingress.service](../../systemd/wasm-deploy-ingress.service)

## Security Model

- deploy API authentication uses the same bearer-token model as the admin API:
  - read token for `GET`
  - write token for mutating requests
- artifact credentials are stored locally in encrypted form using the deploy-ingress KEK
- runtime application secrets are not resolved or injected here
- digest verification is mandatory for URL-based artifacts
- OCI references are resolved and verified before `DeployApp` is published
- remote artifact fetch is capped at `64 MiB`
- optional Ed25519-signed artifact metadata can be required by policy
- signature policy can restrict:
  - issuer
  - repository
  - namespace

## HA Behavior

When HA is enabled:

- deploy ingress instances coordinate through a JetStream KV lease bucket
- exactly one instance is the active writer at a time
- follower instances reject mutating requests with `503 deploy_ingress_not_leader`
- deploy-time artifact credentials are stored in a shared KV bucket, encrypted with the deploy-ingress KEK
- accepted artifacts are announced over JetStream and replicated into follower artifact stores

This means a failover instance can continue:

- resolving private artifact credentials
- serving previously ingested artifacts
- accepting new deploy intents after it acquires the leader lease

Operational requirement:

- all deploy-ingress instances in the HA set must use the same KEK source
- otherwise shared credential entries cannot be decrypted after failover

## Known Boundaries

- node liveness/availability is derived from NATS cluster events, not from a separate control-plane registry service
- artifact replication is eventually consistent across deploy-ingress instances, not transactional
- signature verification currently uses a platform-provided Ed25519 envelope, not Sigstore/Cosign
