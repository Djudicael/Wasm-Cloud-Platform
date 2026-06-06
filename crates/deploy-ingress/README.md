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

### Auth Matrix

| Endpoint | Method | Required permission when auth is enabled | Notes |
|----------|--------|-------------------------------------------|-------|
| `/health` | `GET` | none | Liveness only |
| `/deploy/intent` | `POST` | write | Mutating deploy request |
| `/deploy/artifact-credentials` | `PUT` | write | Mutating credential store |
| `/artifacts/{sha256}/verification` | `GET` | read | Verification lookup |

If `WASM_DEPLOY_INGRESS_AUTH_ENABLED=false`, all endpoints behave as write-authorized for backward compatibility. That is acceptable for local development only.

### Health

```text
GET /health
```

Returns a simple liveness payload.

Example response:

```json
{
  "status": "ok",
  "ingress_id": "deploy-ingress-0",
  "ha_enabled": true,
  "is_leader": true,
  "leader_ingress_id": "deploy-ingress-0",
  "leader_artifact_server_url": "https://deploy.example.com/artifacts"
}
```

### Deploy Intent

```text
POST /deploy/intent
```

Accepts [`DeployIntentRequest`](../common/src/deploy.rs) JSON and returns `202 Accepted` with `DeployIntentResponse` on success.

When auth is enabled, callers must send:

```text
Authorization: Bearer <write-token>
```

Minimum request shape:

```json
{
  "app_id": "hello-api:v1",
  "config": {
    "id": "hello-api:v1",
    "namespace": "default"
  },
  "routes": [
    {
      "host": "api.example.com",
      "app_id": "hello-api:v1",
      "path_prefix": "/v1",
      "strip_prefix": false,
      "created_at": 1760000000,
      "updated_at": 1760000000
    }
  ],
  "artifact": {
    "url": "https://artifacts.example.com/hello-api.wasm",
    "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "credential_ref": "ghcr-reader"
  }
}
```

Success response:

```json
{
  "app_id": "hello-api:v1",
  "artifact_url": "http://deploy-ingress/artifacts/012345...",
  "expected_hash": "012345...",
  "size_bytes": 123456,
  "source_node_id": "deploy-ingress-0",
  "artifact_transfer_manifests": [],
  "gateway_config_published": false,
  "route_count": 1,
  "api_key_count": 0
}
```

`routes` is optional. When present, deploy-ingress publishes one `RouteAdd`
event per entry after the `DeployApp` event is accepted.

Primary status codes:

| Status | Meaning |
|--------|---------|
| `202 Accepted` | Deploy intent accepted and `DeployApp` published |
| `400 Bad Request` | Invalid deploy payload |
| `401 Unauthorized` | Missing or invalid bearer token |
| `403 Forbidden` | Authenticated caller lacks write permission, or security policy rejected the deploy |
| `422 Unprocessable Entity` | Request body parsed but failed schema extraction/validation before handler logic |
| `502 Bad Gateway` | Remote artifact source failed |
| `503 Service Unavailable` | This ingress instance is a follower; retry on leader |

### Artifact Credential Storage

```text
PUT /deploy/artifact-credentials
```

Stores a deploy-time artifact fetch credential under `_platform/artifact-credentials:v1`.

When auth is enabled, callers must send:

```text
Authorization: Bearer <write-token>
```

This credential store is separate from application runtime secret injection. It exists only so deploy ingress can authenticate to external registries or artifact hosts.

Request:

```json
{
  "key": "ghcr-reader",
  "value": "authorization:Bearer <token>"
}
```

Success response:

```json
{
  "key": "ghcr-reader"
}
```

### Artifact Verification Lookup

```text
GET /artifacts/{sha256}/verification
```

Returns the stored verification record for a previously accepted remote artifact.

When auth is enabled, callers must send:

```text
Authorization: Bearer <read-token>
```

Success response:

```json
{
  "sha256": "012345...",
  "verified": true,
  "algorithm": "ed25519",
  "issuer": "https://token.actions.githubusercontent.com",
  "repository": "example-org/hello-api",
  "namespace": "production",
  "public_key_sha256": "abcd...",
  "verified_at_unix_secs": 1760000000
}
```

If the artifact was never accepted, the endpoint returns `404`.

### Follower Rejection Contract

When HA is enabled, mutating requests sent to a follower return:

```json
{
  "error": "deploy_ingress_not_leader",
  "leader_ingress_id": "deploy-ingress-1",
  "leader_artifact_server_url": "https://deploy.example.com/artifacts"
}
```

with HTTP `503 Service Unavailable`.

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
- `WASM_DEPLOY_INGRESS_ALLOWED_IDENTITIES`
- `WASM_DEPLOY_INGRESS_ALLOWED_REPOSITORIES`
- `WASM_DEPLOY_INGRESS_ALLOWED_NAMESPACES`
- `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS`

Today there is no dedicated TOML config loader for this binary. The production shape is an environment file plus a systemd unit.

See:

- [config/deploy-ingress.env.example](../../config/deploy-ingress.env.example)
- [systemd/wasm-deploy-ingress.service](../../systemd/wasm-deploy-ingress.service)
- [docs/deploy-ingress-operations.md](../../docs/deploy-ingress-operations.md)

## Security Model

- deploy API authentication uses the same bearer-token model as the admin API:
  - read token for `GET`
  - write token for mutating requests
- artifact credentials are stored locally in encrypted form using the deploy-ingress KEK
- runtime application secrets are not resolved or injected here
- digest verification is mandatory for URL-based artifacts
- OCI references are resolved and verified before `DeployApp` is published
- hardened mode can require digest-pinned OCI refs only
- remote artifact fetch is capped at `64 MiB`
- optional Ed25519-signed artifact metadata can be required by policy
- Cosign-style signed payloads can also be verified with a public key via
  `algorithm = "cosign-ed25519"`
- Sigstore bundles can be verified via `algorithm = "sigstore-bundle"`
- signature policy can restrict:
  - issuer
  - identity
  - repository
  - namespace

### Production Auth Guidance

- enable auth on every public or shared-network deploy-ingress instance
- keep the write token scoped to CI and deployment automation only
- give the read token only to operator tooling that needs verification lookups
- terminate TLS in front of deploy ingress or run it directly with a TLS-capable ingress layer
- rotate read and write tokens independently
- treat `WASM_CTL_AUTH_TOKEN` as a deploy secret in CI, not as a node bootstrap secret

Recommended topology:

1. public HTTPS ingress or load balancer in front of the deploy API
2. private artifact port reachability from nodes
3. write token available only to CI deploy jobs
4. read token available only to operator diagnostics
5. shared KEK and shared JetStream for HA ingress instances

For full operating procedures, token rotation, KEK rotation, and failover expectations, see [docs/deploy-ingress-operations.md](../../docs/deploy-ingress-operations.md).

`cosign-ed25519` is a narrower interoperability mode:

- deploy ingress verifies the supplied signature against the supplied public key
- the payload must contain Cosign-style digest and identity fields
- issuer/repository/namespace are still enforced through the normal policy engine

It does not yet implement Fulcio certificate validation or Rekor transparency-log verification.

`sigstore-bundle` is the stronger interoperability mode:

- deploy ingress verifies a Sigstore bundle against Sigstore’s production trust root
- verification includes certificate and transparency-log checks through the upstream verifier
- current policy binding for this mode is issuer + identity

Repository/namespace allowlists remain native-signature policy knobs and are not currently derived from Sigstore bundle identity material.

If `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS=true`, tag refs such as
`oci://ghcr.io/org/app:v1` are rejected and callers must use digest-pinned refs
such as `oci://ghcr.io/org/app@sha256:...`.

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
