# Deploy Ingress Operations

This runbook covers the operational model for `wasm-deploy-ingress` in production.

## Scope

Use this document when you need to:

- expose deploy ingress to CI/CD
- rotate deploy-ingress auth tokens
- rotate the deploy-ingress KEK
- run more than one ingress instance
- debug failed remote deploys
- verify leader/follower state

## Production Topology

Recommended shape:

1. one or more `wasm-deploy-ingress` instances on control-plane hosts
2. shared NATS / JetStream connectivity
3. shared KEK source across all ingress instances in the HA set
4. HTTPS load balancer or ingress in front of the deploy API
5. private network path from nodes to the artifact server port

Minimal split:

- deploy API: exposed to CI/CD
- artifact port: reachable by nodes

Do not expose node admin APIs when deploy ingress is the intended CI/CD entrypoint.

## Auth Model

Deploy ingress uses bearer-token auth with two permission levels:

- read token:
  - `GET /artifacts/{sha256}/verification`
- write token:
  - `POST /deploy/intent`
  - `PUT /deploy/artifact-credentials`

`GET /health` does not require auth.

Recommended usage:

- CI deploy jobs get the write token only
- operator diagnostics tooling gets the read token only

Do not reuse:

- node bootstrap secrets
- runtime application secrets
- registry fetch credentials

## Token Rotation

### Read token

1. generate a new token
2. update deploy-ingress environment/config on all instances
3. restart or roll instances
4. update operator tooling
5. revoke the old token

### Write token

1. generate a new token
2. update deploy-ingress environment/config on all instances
3. update CI secret storage
4. trigger a test deploy
5. revoke the old token

Because deploy ingress currently uses a static bearer-token model, rotation is a coordinated config rollout.

## KEK Management

The KEK protects deploy-time artifact credentials at rest.

Requirements:

- every HA ingress instance must use the same KEK source
- the KEK must survive restart
- `generate` is not a production setting

Supported practical choices:

- `env:...`
- `file`
- `passphrase-env:...`

### KEK rotation

Current implementation expects a stable KEK. There is no online multi-KEK rewrap path yet.

Safe rotation procedure:

1. stop mutating deploy activity
2. export or re-enter deploy-time artifact credentials
3. shut down ingress instances
4. replace the KEK on all ingress instances
5. start ingress instances
6. re-store artifact credentials with `wasm-ctl secrets set-artifact-credential`
7. verify remote deploy success

If you rotate the KEK without re-seeding credentials, replicated credential entries will no longer decrypt.

## HA and Failover

When HA is enabled:

- one ingress instance is leader
- followers reject mutating writes with `503 deploy_ingress_not_leader`
- followers include leader hint data in the response
- followers replicate artifact metadata and stored raw Wasm

Operational checks:

```bash
curl https://deploy.example.com/health
```

Expected fields:

- `ha_enabled`
- `is_leader`
- `leader_ingress_id`
- `leader_artifact_server_url`

### Failover expectations

After leader loss:

1. follower observes lease expiry
2. follower acquires leader lease
3. follower starts accepting mutating requests

The critical requirement is shared KEK plus shared JetStream.

## Deploy-Time Credential Management

Artifact fetch credentials are separate from runtime app secrets.

Store them with:

```bash
wasm-ctl secrets set-artifact-credential --key ghcr-reader
```

Use cases:

- bearer token for GitHub/GHCR
- full `Authorization` header via `authorization:...`

These credentials are used only by deploy ingress when pulling remote artifacts.

## Debugging Failed Remote Deploys

Check in this order:

1. deploy-ingress health:
   - leader/follower state
2. auth:
   - `401` means missing/invalid token
   - `403` means wrong permission or policy rejection
   - `503` means request hit a follower
3. remote artifact fetch:
   - registry credential present
   - URL/ref reachable
   - digest correct
4. signature/provenance policy:
   - issuer allowed
   - repository allowed
   - namespace allowed
   - digest-pinned OCI ref required if hardened mode is enabled
5. node audience:
   - active nodes visible in cluster events

Useful lookups:

```bash
curl -H "Authorization: Bearer <read-token>" \
  https://deploy.example.com/artifacts/<sha256>/verification
```

Audit file:

- check `WASM_DEPLOY_INGRESS_AUDIT_PATH`
- accepted requests write `deploy_intent_accepted`
- rejected requests write `deploy_intent_rejected`

## Common Failure Modes

### `401 unauthorized`

- token missing
- malformed `Authorization` header
- wrong bearer token

### `403 forbidden`

- read token used on a write endpoint
- signature policy rejected issuer/repository/namespace
- hardened digest-pin policy rejected mutable OCI tag ref

### `413 payload too large`

- deploy-intent request body exceeded ingress body limit

### `502 bad gateway`

- remote registry or URL fetch failed
- registry credentials invalid

### `503 deploy_ingress_not_leader`

- request hit a follower
- client should retry against the leader/front door

## Recommended Monitoring Signals

- deploy-ingress health endpoint
- audit file growth and reject events
- NATS / JetStream availability
- artifact fetch failure rate
- leader churn frequency

If leader churn is frequent, treat that as a control-plane stability issue, not a normal operating mode.
