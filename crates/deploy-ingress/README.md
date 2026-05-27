# deploy-ingress

Control-plane ingress for remote Wasm artifact deploys.

## Overview

`deploy-ingress` is the platform-facing process that accepts deploy intent from CI/CD, fetches remote Wasm artifacts, verifies them, stores them in the platform artifact plane, and publishes the normal `DeployApp` event to the cluster.

This keeps remote deploys off the node admin API and removes the need for CI to upload a local `.wasm` to one node first.

## Responsibilities

- accept `POST /deploy/intent`
- accept `PUT /deploy/artifact-credentials`
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

## Known Boundaries

- node liveness/availability is derived from NATS cluster events, not from a separate control-plane registry service
- artifact credentials are local to deploy ingress, not globally replicated to nodes
- there is no signature/provenance enforcement policy yet
- there is no dedicated HA coordinator yet; if you run multiple deploy-ingress instances, keep their credential/artifact store strategy explicit
