# Step 43 — Platform Code Audit: Core Findings

## How to use this document

This file focuses on the **highest-impact platform issues**: security, shared-nothing recovery, runtime isolation, and lifecycle correctness.

Severity rubric follows the existing INFRA_IMPL convention:

- **P0 — Critical**: security vulnerability, data exposure, secret corruption, or correctness failure with high blast radius
- **P1 — High**: gap that materially undermines a core platform guarantee
- **P2 — Medium**: quality/operability gap with meaningful but narrower impact
- **P3 — Low**: polish / cleanup / future hardening

Each finding includes:

- implementation checkbox items,
- source evidence,
- why it matters,
- recommended fix direction,
- validation guidance.

---

## P0 — Critical Findings

### P0-1. Secret update pipeline is internally incompatible and can corrupt secret storage

- [x] Define **one canonical secret transport format** for `ctl -> NATS -> node`.
- [x] Stop writing raw secret bytes directly into the `SECRETS` table from event handlers.
- [x] Route secret updates through `SecretProvider::set()` or a dedicated import API that preserves the `AppSecretBundle` format.
- [x] Add an end-to-end integration test: CLI publish -> node consume -> `SecretProvider::get()` returns plaintext.

**Evidence**

- `crates/ctl/src/cmds/secrets.rs:49-55` publishes `Event::SecretUpdate` and labels raw plaintext bytes as `encrypted_value`.
- `crates/node/src/handlers.rs:115-125` receives that event and stores the bytes with `store.save_secrets(&app_id, &encrypted_value)`.
- `crates/storage/src/secrets.rs:6-16` blindly inserts those raw bytes into the `SECRETS` table.
- `crates/secrets/src/local_provider.rs:48-95` expects `SECRETS` entries to deserialize as a `bincode`-encoded `AppSecretBundle`, not raw secret bytes.

**Why this matters**

This is not just an encryption TODO. It is a **type/format mismatch across subsystem boundaries**. As written, a secret update delivered via the control plane can leave storage in a format the local secret provider does not understand. In practice, that means secret rotation can become unreadable, corrupt the expected bundle format, or bypass the platform’s intended DEK/KEK hierarchy.

For a shared-nothing platform, secret state has to be recoverable and locally readable after restart. Right now that contract is not stable.

**Recommended fix direction**

Prefer one of these designs and enforce it consistently:

1. **Provider-first path**: the node receives a secret update event and calls `secret_provider.set(app_id, key, plaintext)` after performing proper decryption.
2. **Bundle-import path**: the control plane sends a formally versioned encrypted bundle/update payload, and the node imports it through a typed API that updates `AppSecretBundle` correctly.

Do not let control-plane handlers write raw bytes directly into secret storage unless those bytes are exactly the storage format expected by the local provider.

**Validation**

- Secret update survives node restart.
- `LocalSecretProvider::get()` works after a rotation received from NATS.
- Existing app secrets remain readable after a new secret is added.
- Negative test: malformed secret event is rejected without corrupting stored bundle state.

**Validation after fix**

- [x] `crates/secrets/src/transport.rs` now defines one canonical, versioned secret transport envelope used for both `SecretUpdate` and bootstrap snapshot secret transfer.
- [x] `crates/ctl/src/cmds/secrets.rs` now publishes a typed `SecretTransportEnvelope` instead of raw bytes.
- [x] `crates/node/src/handlers.rs` now accepts only plaintext transport for normal secret rotation and only bootstrap-ciphertext transport for bootstrap snapshot import, failing closed on mismatched variants.
- [x] `cargo test -p node --bin wasm-node` passed in WSL with updated handler tests for canonical transport handling.
- [x] `crates/node/src/handlers.rs` now includes a NATS-backed secret-update roundtrip test proving a published `SecretUpdate` event is consumed and persisted through `LocalSecretProvider`, and `SecretProvider::get()` returns the original plaintext.
- [x] Stronger end-to-end encrypted operator-to-node secret distribution is now implemented for steady-state secret rotation.

- [x] `crates/node/src/main.rs` now initializes a node-local X25519 secret-transport keypair, persisting it in sealed form when `runtime.key_source=file|env:...` is used and advertising the public key through the authoritative cluster node registry.
- [x] `crates/common/src/types.rs` and `/admin/cluster/nodes` now expose `secret_transport_public_key` for active node targeting.
- [x] `crates/ctl/src/cmds/secrets.rs` now fetches the authoritative cluster node registry, encrypts the secret separately for each active node, and publishes per-node targeted `SecretUpdate` events instead of plaintext fanout.
- [x] `crates/node/src/handlers.rs` now ignores non-targeted secret updates and decrypts the `node_transport_ciphertext_v1` payload before calling `SecretProvider::set()`.
- [x] WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_secret_update_event_roundtrip_persists_encrypted_targeted_secret_via_secret_provider -- --nocapture`
  - `cargo test -p node --bin wasm-node test_file_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
  - `cargo test -p ctl select_secret_targets_filters_stale_nodes_and_requires_public_keys -- --nocapture`

---

### P0-2. The KEK is stored in plaintext at rest

- [x] Replace plaintext KEK persistence with a sealed/encrypted form.
- [x] Decide the node root of trust: OS key store, external KMS, passphrase-derived wrapping key, or hardware-backed storage.
- [x] Add startup behavior that fails closed when secure KEK loading is required but unavailable.
- [x] Add migration logic for existing plaintext KEKs in development environments.

**Evidence**

- `crates/storage/src/secrets.rs:49-76` stores and loads raw KEK bytes from the `KEK` table.
- `crates/node/src/main.rs:701-749` generates or reloads a KEK and persists it through `store.save_kek(kek.as_bytes())`.

**Why this matters**

The current implementation protects app secrets with DEKs/KEKs in memory, but a disk compromise on one node can immediately expose the KEK for that node. That collapses the intended at-rest protection model.

This is especially important in a shared-nothing system because each node is supposed to be independently survivable and independently secure. A local disk theft should not equal full secret disclosure.

**Recommended fix direction**

Short-term acceptable options:

- encrypt the KEK with a passphrase-derived wrapping key,
- or load the KEK only from an operator-provided file/secret source and never persist plaintext.

Longer-term preferred options:

- Windows DPAPI / OS keyring,
- cloud KMS / Vault transit,
- TPM-backed or HSM-backed sealing when available.

**Validation**

- Restart test with secure KEK loading enabled.
- Existing secrets remain readable after restart.
- On-disk store inspection confirms the KEK is not recoverable as raw bytes.

**Validation after fix**

- [x] `crates/node/src/main.rs` now treats `runtime.key_source=file|env:VAR_NAME` as the seal-key source and persists a **sealed KEK blob** in redb instead of a raw KEK.
- [x] `crates/node/src/main.rs` now also supports `runtime.key_source=passphrase-env:VAR_NAME`, deriving the seal key with Argon2id from the operator passphrase plus a persisted random salt.
- [x] `crates/node/src/main.rs` now also supports `runtime.key_source=command` with `runtime.key_command = ["..."]`, allowing operators to source the seal key from an external helper that can bridge to DPAPI, Vault transit, cloud KMS, TPM tooling, or other local brokers without storing plaintext in node config.
- [x] `crates/node/src/main.rs` now also supports `runtime.key_source=vault-kv`, fetching the seal key directly from a Vault KV v2 secret using `runtime.key_vault_url`, `runtime.key_vault_token_env`, `runtime.key_vault_mount`, `runtime.key_vault_path`, and `runtime.key_vault_field`.
- [x] `crates/node/src/main.rs` now also supports `runtime.key_source=vault-transit`, deriving the 32-byte seal key from a Vault transit HMAC using `runtime.key_vault_url`, `runtime.key_vault_token_env`, `runtime.key_vault_transit_mount`, `runtime.key_vault_transit_key`, and `runtime.key_vault_transit_context`.
- [x] `crates/node/src/main.rs` now also supports `runtime.key_source=aws-kms-hmac`, deriving the 32-byte seal key from `GenerateMac(HMAC_SHA_256)` using `runtime.key_aws_kms_region`, `runtime.key_aws_kms_endpoint` (optional), `runtime.key_aws_kms_key_id`, and `runtime.key_aws_kms_context`.
- [x] Legacy 32-byte plaintext KEKs already present in redb are now migrated in place into the sealed-at-rest form.
- [x] `key_source=generate` now fails closed when persisted secret state exists and cannot be safely unlocked.
- [x] `cargo test -p node --bin wasm-node` passed in WSL with tests covering sealed KEK initialization, sealed reload, legacy migration, and wrong-key rejection.
- [x] Additional WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_passphrase_env_key_source_initializes_and_reloads_sealed_kek -- --nocapture`
  - `cargo test -p node --bin wasm-node test_passphrase_env_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
- [x] Additional WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_command_key_source_initializes_and_reloads_sealed_kek -- --nocapture`
  - `cargo test -p node --bin wasm-node test_command_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
- [x] Additional WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_vault_kv_key_source_initializes_and_reloads_sealed_kek -- --nocapture`
  - `cargo test -p node --bin wasm-node test_vault_kv_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
- [x] Additional WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_vault_transit_key_source_initializes_and_reloads_sealed_kek -- --nocapture`
  - `cargo test -p node --bin wasm-node test_vault_transit_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
- [x] Additional WSL validation passed with:
  - `cargo test -p node --bin wasm-node test_aws_kms_hmac_key_source_initializes_and_reloads_sealed_kek -- --nocapture`
  - `cargo test -p node --bin wasm-node test_aws_kms_hmac_key_source_initializes_and_reloads_secret_transport_keypair -- --nocapture`
- [x] Added a stronger operator-provided root-of-trust option beyond raw key files/env: `runtime.key_source=passphrase-env:VAR_NAME` now derives the seal key with Argon2id and a persisted random salt.
- [x] For non-Windows production targets, the practical root-of-trust options are now covered by built-in Vault KV, built-in Vault transit, built-in AWS KMS HMAC, and the `key_source=command` hook for external HSM/TPM brokers.
- [ ] Native TPM/HSM provider SDK integration remains deferred optional future hardening above the current sealed-at-rest baseline; it is not required for the current non-Windows production target.

---

### P0-3. Admin plane is insecure by default and TLS is not actually enforced

- [x] Change production defaults so the admin API is **not open unauthenticated** unless explicitly requested.
- [x] Bind the admin API to loopback by default, or require an explicit advertised/admin bind address.
- [x] Enforce `require_tls=true` as a startup failure unless TLS is actually configured.
- [x] Only trust `X-Forwarded-For` / `X-Real-IP` from explicitly trusted upstream proxies.
- [x] Add deployment-mode validation: dev mode can relax this, production mode cannot.

**Evidence**

- `crates/common/src/config.rs:192-193` documents that when auth is disabled, all admin endpoints are accessible without authentication.
- `crates/common/src/config.rs:249-256` sets `enabled: false` by default.
- `crates/node/src/main.rs:1325-1335` only logs a warning when admin auth requires TLS, but does not enforce actual admin TLS.
- `crates/node/src/main.rs:2506-2515` binds the admin API on `0.0.0.0`.
- `crates/proxy/src/auth_middleware.rs:260-275` extracts client IP from `X-Forwarded-For` and `X-Real-IP`.

**Why this matters**

The current combination is dangerous:

- admin API reachable on all interfaces,
- authentication off by default,
- TLS requirement not enforced,
- rate-limit source IP can be spoofed if the service is directly exposed.

Even if you currently operate only on trusted networks, this is too easy to deploy incorrectly.

**Recommended fix direction**

A safer default matrix would be:

- development: loopback bind, auth optional, clear warnings,
- production: non-loopback bind requires auth and TLS, otherwise startup fails.

Also split “client IP extraction” from “trusted proxy IP extraction” so forwarded headers are ignored unless the request came from a configured proxy subnet.

**Validation**

- Startup fails when production config requests remote admin bind without auth/TLS.
- Requests with spoofed `X-Forwarded-For` from untrusted peers do not affect rate limiting.
- Loopback-only dev setup still works ergonomically.

**Validation after fix**

- [x] `crates/common/src/auth.rs` and `crates/common/src/config.rs` now expose `auth.trusted_proxies` as explicit IP/CIDR configuration with validation.
- [x] `crates/proxy/src/auth_middleware.rs` now derives client IP from the direct peer socket address by default and only honors forwarded headers when that peer matches `auth.trusted_proxies`.
- [x] `crates/node/src/main.rs` now serves the admin app with `SocketAddr` connect info so the middleware can enforce trusted-proxy checks on both HTTP and HTTPS listeners.
- [x] `crates/config/src/lib.rs` now rejects `auth.require_tls = false` with a non-loopback `admin.bind_address` unless `auth.trusted_proxies` is configured, preserving relaxed loopback dev mode while failing closed for exposed admin listeners.
- [x] WSL validation passed:
  - `cargo test -p proxy --lib -- --nocapture`
  - `cargo test -p config --lib -- --nocapture`
  - `cargo test -p node --test cluster_bootstrap -- --nocapture`
  - `cargo test -p node test_secret_update_event_roundtrip_persists_plaintext_via_secret_provider -- --nocapture`

---

### P0-4. Artifact server is publicly bound and unauthenticated

- [x] Bind the artifact server to loopback by default.
- [x] If remote artifact exchange is required, add node-to-node authentication and integrity checks beyond raw SHA-256 path matching.
- [x] Separate “artifact upload by peer nodes” from “artifact retrieval by local runtime” instead of exposing a generic unauthenticated PUT/GET surface.
- [x] Add configuration validation that rejects public artifact bind without authentication.

**Evidence**

- `crates/node/src/main.rs:2516-2525` binds the artifact server on `0.0.0.0`.
- `crates/storage/src/artifact_server.rs:39-76` exposes unauthenticated `PUT /artifacts/{sha256}` and `GET /artifacts/{sha256}`.

**Why this matters**

Anyone who can reach that port can attempt artifact upload/download. Hash verification protects against path/content mismatch, but it does **not** provide authorization. A reachable unauthenticated artifact store can become a persistence, bandwidth, and supply-chain abuse surface.

**Recommended fix direction**

Best near-term fix:

- artifact server loopback-only,
- artifact exchange performed through authenticated node workflows.

If direct peer-to-peer artifact transfer is a goal, require one of:

- mutual TLS,
- signed node tokens,
- short-lived signed upload/download URLs.

**Validation**

- Remote unauthenticated access fails.
- Local deployment path still works.
- Cluster bootstrap artifact transfer still works under the authenticated path.

**Validation after fix**

- [x] `crates/storage/src/artifact_server.rs` now treats remote `GET /artifacts/{sha}` as a signed-manifest-only peer transfer path; compatibility bearer tokens no longer grant generic remote artifact reads.
- [x] `crates/storage/src/artifact_server.rs` keeps loopback access for local operator flows, while remote `PUT /artifacts/{sha}` and `POST /artifacts/{sha}/authorize` remain write-authorized control-plane actions.
- [x] `crates/config/src/lib.rs` now rejects non-loopback `admin.artifact_bind_address` unless `auth.write_token` or legacy `admin.auth_token` is configured.
- [x] WSL validation passed:
  - `cargo test -p storage artifact_server -- --nocapture`
  - `cargo test -p config --lib -- --nocapture`
  - `cargo test -p e2e --test cluster_registry -- --ignored --nocapture`

---

## P1 — High Findings

### P1-1. Route replay/rebuild does not match the canonical message wire format

- [x] Make route replay deserialize the same envelope format used by normal subscribers.
- [x] Reuse a shared event deserializer from `crates/messaging` instead of ad hoc JSON shape matching.
- [x] Revisit durable consumer semantics for rebuild/replay so a full rebuild can be repeated safely.
- [x] Add replay tests for both envelope-wrapped and backward-compatible legacy event formats.

**Evidence**

- `crates/storage/src/integrity.rs:159-228` replays JetStream messages by parsing raw JSON and looking for a top-level `type` field.
- `crates/messaging/src/lib.rs:76-85` publishes events wrapped in `MessageEnvelope<Event>`.
- `crates/messaging/src/lib.rs:288-304` already contains compatibility-aware event deserialization logic.

**Why this matters**

A shared-nothing platform only works if a node can rebuild local state from durable control-plane history or snapshots. Right now, the replay code is not aligned with the canonical wire format, so route state recovery can silently skip legitimate events.

The durable consumer name and ack behavior also suggest the replay stream may not be safely reusable for repeated full rebuilds.

**Recommended fix direction**

- move all event parsing behind one canonical deserializer,
- make replay semantics explicit: either ephemeral full replay, or snapshot + incremental replay, but not a half-stateful hidden consumer.

**Validation**

- Start from empty local routing state and replay historical route events successfully.
- Repeat the rebuild twice in a row and confirm identical results.
- Validate mixed `RouteAdd` and `RouteRemove` histories.

---

### P1-2. Supervisor shutdown is not authoritative; resources can be released before workers are truly gone

- [x] Do not deregister routes/ports until the instance has actually exited.
- [x] Introduce a real cooperative shutdown path for blocking Wasm execution, or a process-level kill/fencing path when cooperation fails.
- [x] Track instance lifecycle with explicit fenced shutdown states instead of inferring from timeout/drop behavior.
- [x] Add regression tests for port reuse and stale listener behavior.

**Evidence**

- `crates/supervisor/src/lib.rs:473-503` documents that `shutdown_rx` is not wired into the blocking run loop.
- `crates/supervisor/src/lib.rs:926-930` releases registry/port resources during shutdown flow.
- `crates/supervisor/src/lib.rs:965-976` warns that timeout led to an abort path, but the model relies on task-level behavior rather than confirmed process exit.

**Why this matters**

This creates a race where:

1. the platform believes an instance is gone,
2. routes/ports are released,
3. but the old worker may still be alive.

That is exactly the kind of lifecycle ambiguity that causes misrouting, failed redeploys, and hard-to-debug availability incidents.

**Recommended fix direction**

Treat instance termination as a **fenced state transition**. Ports and upstream entries should only be reusable once the worker is definitively gone.

**Validation**

- Repeated deploy/remove/redeploy loop on the same app and port range.
- Forced timeout case proves old instance no longer accepts traffic before reuse.
- Draining metrics/logging distinguish graceful vs forced termination.
- WSL: `cargo test -p supervisor test_shutdown_timeout_keeps_stale_listener_fenced_until_reap -- --nocapture`

---

### P1-3. Internal gateway header stripping is ineffective, and endpoint auth is only partially implemented

- [x] Forward the sanitized header map, not a fresh clone from the original request.
- [x] Reject or disable `Authenticated` / `Roles` endpoint policies until they are implemented.
- [x] Add tests for forged `x-namespace`, `x-source-app`, and `x-source-tid` headers.
- [x] Add policy conformance tests so configured auth modes cannot silently degrade to “allow”.

**Evidence**

- `crates/internal_gateway/src/lib.rs:208-221` removes internal identity headers from the extracted header map.
- `crates/internal_gateway/src/lib.rs:445-465` later clones `req.headers()` and forwards those headers instead of the sanitized map.
- `crates/internal_gateway/src/lib.rs:397-412` shows `EndpointAuth::Authenticated` and `EndpointAuth::Roles` as placeholders.

**Why this matters**

This weakens two separate protections:

1. forged identity headers may survive into forwarded internal traffic,
2. some configured endpoint auth modes are effectively configuration theater.

For east-west traffic in a shared-nothing platform, identity trust boundaries must be very clear. Internal metadata must not be spoofable by request headers.

**Recommended fix direction**

Make the internal gateway strict: if an endpoint auth mode is configured but not implemented, request handling should fail closed or configuration should be rejected.

**Validation**

- Header forgery tests prove forwarded request no longer contains client-supplied internal identity headers.
- Endpoint rules using `Authenticated` or `Roles` fail closed until fully implemented.

---

### P1-4. WASI policy enforcement is still too permissive for the intended isolation model

- [x] Wire `policy.filesystem.allowed_paths` into actual WASI preopened directories.
- [x] Revisit the current network policy mapping where inbound permission can implicitly enable TCP capability.
- [x] Document which guarantees are enforced by WASI today vs deferred to eBPF or higher layers.
- [x] Add policy conformance tests for forbidden filesystem paths and disallowed outbound destinations.

**Evidence**

- `crates/runtime/src/executor.rs:137-179` shows network permissions configured coarsely and leaves filesystem preopens as a TODO.
- `crates/supervisor/src/lib.rs:382-387` allows non-loopback destinations broadly in socket checks.
- `crates/runtime/POLICY_LIMITATIONS.md:1-141` explicitly documents known enforcement gaps.

**Why this matters**

The architecture positions the runtime as a strong isolation boundary. Today the code is honest about the remaining gaps, which is good, but the gaps are still real:

- per-path filesystem policy is not enforced at the WASI boundary,
- fine-grained outbound restrictions are not fully enforced where an operator may expect them.

**Recommended fix direction**

Short-term priority should be the fixable gap: `allowed_paths` preopens. At the same time, make docs and runtime logging explicit whenever a requested policy depends on eBPF or outer-layer enforcement.

**Validation**

- App cannot read/write outside configured preopened paths.
- Policy tests demonstrate blocked outbound connects for disallowed cases, or explicit logs show when enforcement is delegated elsewhere.
- `crates/runtime/POLICY_LIMITATIONS.md` now contains an explicit capability matrix covering:
  - what Wasmtime enforces authoritatively today,
  - what is counted authoritatively today,
  - what is still deferred to outer layers or future host/resource wrapping.

---

### P1-5. App instance listeners are host-exposed, allowing proxy/gateway bypass risk

- [x] Bind per-instance listeners to loopback or a private node-only network namespace/bridge.
- [x] If public bind is intentional, document the required firewall model and enforce it in deployment guidance.
- [x] Add a “traffic must pass through gateway/proxy” mode for hardened deployments.

**Evidence**

- `crates/node/src/main.rs:560-564` creates the port allocator with `0.0.0.0`.
- `crates/runtime/src/executor.rs:168-172` documents that the app will bind `0.0.0.0:<port>`.

**Why this matters**

If app listeners are directly reachable from the host/network, requests can bypass:

- authentication,
- rate limiting,
- observability hooks,
- request transformation,
- and potentially mesh identity enforcement.

That is especially risky if the platform promise is “all north-south traffic goes through the gateway”.

**Recommended fix direction**

The safer model is to keep app listeners on a node-private plane and make proxy/internal gateway the only public ingress path.

**Validation**

- External client cannot reach instance port directly in hardened mode.
- Proxy/internal gateway can still reach the instance.

**Validation after fix**

- [x] `crates/common/src/config.rs` and `crates/config/src/lib.rs` now expose `runtime.instance_bind_address`, default it to `127.0.0.1`, and validate that it is an IP literal.
- [x] `crates/node/src/main.rs` now allocates instance ports on `runtime.instance_bind_address` and injects `BIND_ADDR` / `HOST` into app environments so sample apps bind the hardened address by default.
- [x] `crates/supervisor/src/lib.rs` now enforces the bind policy at the WASI socket boundary, allowing binds only to the assigned port on the configured instance bind IP rather than wildcard host binds.
- [x] `apps/hello-axum`, `apps/echo-service`, and `apps/postgres-app` now honor the injected bind address instead of hardcoding `0.0.0.0`.
- [x] Operator examples now document `runtime.instance_bind_address = "127.0.0.1"` in `config/dev.toml`, `config/staging.toml`, and `config/production.toml`, making hardened gateway-only ingress the explicit default.
- [x] WSL validation passed:
  - `cargo test -p config --lib -- --nocapture`
  - `cargo test -p supervisor --lib -- --nocapture`
  - `cargo test -p node --test cluster_bootstrap -- --nocapture`
  - `cargo test -p e2e --test cluster_registry -- --ignored --nocapture`

---

### P1-6. Billing sequence recovery assumes node IDs begin with `node-`

- [x] Fix billing sequence parsing so it does not depend on a hard-coded node ID prefix.
- [x] Decide whether sequence recovery is global or per-node, and encode that intentionally.
- [x] Add restart tests for multiple node ID formats (`node-1`, `prod-a`, `edge-17`).

**Evidence**

- `crates/storage/src/billing.rs:8-18` stores billing keys as `{node_id}:{seq}`.
- `crates/storage/src/billing.rs:25-47` only parses keys that `strip_prefix("node-")`.
- `crates/billing/src/collector.rs:81-96` falls back to this recovery path at startup.

**Why this matters**

A restart on a node whose ID does not begin with `node-` can recover the wrong sequence value, risking duplicate sequence numbers and broken hash-chain continuity.

**Recommended fix direction**

Split parsing from policy:

- parse key by the final `:` separator,
- optionally filter by exact node ID if sequence is supposed to be node-local.

**Validation after fix**

- [x] `crates/storage/src/billing.rs` now parses billing keys by the final `:` separator instead of assuming a `node-` prefix.
- [x] Billing recovery is now intentionally **node-local**:
  - `get_billing_sequence_for_node(node_id)`
  - `get_last_billing_hash_for_node(node_id)`
- [x] `crates/billing/src/collector.rs` now persists the billing cursor under a node-scoped meta key (`billing_cursor:{node_id}`) instead of a single global cursor, preventing one node's restart from inheriting another node's sequence/hash state.
- [x] WSL validation passed with:
  - `cargo test -p storage test_billing_sequence_recovery_is_exact_node_local -- --nocapture`
  - `cargo test -p billing test_billing_restart_sequence_continuity_for_ -- --nocapture`

**Validation**

- Restart preserves monotonic sequence for arbitrary node IDs.
- Hash-chain verification still passes after restart.

---

### P1-7. Metrics aggregation can mix samples from different minutes into the same bucket

- [x] Key in-progress metric buckets by `(app_id, minute_ts)` instead of only `app_id`.
- [x] Flush all completed buckets older than the current minute without overwriting the next minute's accumulator.
- [x] Add a minute-boundary unit test with samples just before and after rollover.

**Evidence**

- `crates/metrics/src/collector.rs:35-69` stores one in-progress bucket per app and only later compares the embedded minute timestamp.

**Why this matters**

This can skew request counts, latency percentiles, and billing/usage rollups precisely at minute boundaries - the exact place where operators tend to trust time-windowed dashboards most.

**Recommended fix direction**

Use a composite key `(app_id, minute_ts)` and keep multiple in-flight minute buckets if necessary.

**Validation**

- Samples at `12:00:59.900` and `12:01:00.050` land in different persisted buckets.
- Percentiles and counts remain stable across rollover.

**Validation after fix**

- [x] `crates/metrics/src/collector.rs` now keys in-flight buckets by `(app_id, minute_ts)` so rollover creates a fresh accumulator immediately instead of waiting for the next flush tick.
- [x] `crates/metrics/src/collector.rs` now flushes only buckets older than the current minute, leaving current and future-minute accumulators intact.
- [x] `crates/metrics/src/collector.rs` now includes minute-boundary tests proving samples at `12:00:59.900` and `12:01:00.050` land in distinct buckets with stable counts and percentiles.
- [x] `crates/metrics/src/nats.rs` now monitors the real JetStream stream set (`DEPLOY`, `CONTROL`, `NODE`, `HEALTH`, `PLATFORM`, `EBPF`) instead of probing a nonexistent aggregate stream name, and exports per-stream byte/message gauges.
- [x] WSL validation passed:
  - `cargo test -p metrics -- --nocapture`

---

### P1-8. Joining nodes advertise a localhost artifact URL, which breaks real multi-node bootstrap

- [x] Introduce an explicit advertised artifact address in node configuration.
- [x] Reject loopback advertisement when cluster mode / multi-node bootstrap is enabled.
- [x] Add integration tests for cross-node artifact fetch using the advertised address.

**Evidence**

- `crates/node/src/main.rs:762-766` constructs `artifact_server_url` as `http://127.0.0.1:<artifact_port>`.
- `crates/messaging/src/events.rs:79-89` defines `NodeJoined.artifact_server_url` specifically so other nodes can push/fetch artifacts.
- `crates/node/src/main.rs:895-901` publishes that URL in `NodeJoined` events.

**Why this matters**

In a true cluster, other nodes cannot use another machine’s `127.0.0.1`. That makes artifact synchronization/bootstrap behavior unreliable outside single-host development.

**Recommended fix direction**

Add explicit advertised node identity/config, for example:

- `node.advertised_host`,
- `admin.artifact_advertised_url`,
- or an automatically derived address with validation.

**Validation**

- Second node can fetch artifacts from first node using the advertised URL.
- Loopback advertisement is rejected outside dev mode.

---

## Core follow-up checklist

### Immediate blockers to fix first

- [x] P0-1 Secret pipeline mismatch
- [x] P0-2 Plaintext KEK persistence
- [x] P0-3 Admin plane exposure and TLS non-enforcement
- [x] P0-4 Public unauthenticated artifact server

### Next wave

- [x] P1-1 Route replay / rebuild correctness
- [x] P1-2 Authoritative instance shutdown
- [x] P1-3 Internal gateway trust boundary hardening
- [x] P1-4 WASI filesystem/network enforcement gap reduction
- [x] P1-5 Direct app port exposure hardening
- [x] P1-8 Replace localhost artifact advertisement

---

## Positive patterns to keep while fixing these issues

- [ ] Keep `MessageEnvelope` versioning in `crates/messaging`
- [ ] Keep migration backup/version checks in `crates/storage`
- [ ] Keep runtime limitation documentation honest and up to date
- [ ] Keep constant-time token comparison in admin/gateway auth paths
- [ ] Keep deny-by-default namespace posture in the internal gateway

