# Step 45 â€” Production Readiness Deep Audit

## Goal

This document is a **second-pass, deeper audit** focused on the production-readiness of the platform base.

It goes beyond the first audit by concentrating on:

- distributed control-plane correctness,
- event delivery semantics,
- cluster bootstrap behavior,
- hidden duplicate-processing paths,
- and readiness gaps that will matter as soon as the platform is used under real multi-node conditions.

This step should be read **after**:

- `INFRA_IMPL/42_PLATFORM_CODE_AUDIT_SUMMARY.md`
- `INFRA_IMPL/43_PLATFORM_CODE_AUDIT_CORE_FINDINGS.md`
- `INFRA_IMPL/44_PLATFORM_CODE_AUDIT_SUPPORTING_FINDINGS.md`

---

## Audit status

- [x] Deep static review completed
- [x] Additional control-plane and production-hardening review completed
- [x] Full post-clean compile/test validation completed

### Important note

This step began as a **source-level deep audit**, but it now also has targeted WSL validation for the corrected control-plane/bootstrap paths.

Most relevant recent WSL validation:

- [x] `cargo test -p messaging` passed
- [x] `cargo test -p node --bin wasm-node` passed
- [x] `cargo test -p node --test cluster_bootstrap` passed against a real NATS+JetStream instance
- [x] `cargo test -p secrets -p messaging -p internal_gateway -p supervisor` passed

The broader workspace/beta validation rerun is now complete:

- [x] `cargo check --workspace`
- [x] `cargo test --workspace --no-run`
- [x] targeted integration tests for `node`, `supervisor`, `proxy`, `storage`, `messaging`, `runtime`

---

## Severity rubric

- **P0 â€” Critical**: immediate production blocker, hidden data/control-plane corruption, or security/correctness flaw with broad blast radius
- **P1 â€” High**: materially undermines a platform guarantee or makes a distributed subsystem unreliable
- **P2 â€” Medium**: important readiness or operability gap that should be fixed before beta/GA
- **P3 â€” Low**: polish or non-blocking hardening

---

## P0 â€” Critical Findings

### P0-1. Durable consumers are not subject-filtered, so the same event is processed multiple times per node

- [x] Add subject-filter support to `subscribe_durable()` and pass the intended subject/filter when creating consumers.
- [x] Replace the current â€œone consumer per subject stringâ€ illusion with actual subject-filtered JetStream consumers.
- [x] Add tests proving each event is handled exactly once per node per intended stream.
- [x] Remove accidental dependency on broken fan-out behavior for gateway/config/health/platform flows.

**Evidence**

- `crates/messaging/src/lib.rs:213-244` creates durable consumers without any subject filter.
- `crates/node/src/main.rs:834-883` creates multiple consumers on `CONTROL` and `NODE` streams as if they were per-subject, while the code itself notes this is not yet supported.
- `crates/messaging/src/lib.rs:132-203` shows each stream contains multiple subjects.

**Concrete effect right now**

- `CONTROL` events are effectively processed by **4 consumers per node**.
- `NODE` events are effectively processed by **3 consumers per node**.

That multiplies side effects in `EventDispatcher`, including config writes, secret writes, node-table updates, bootstrap responses, and snapshot imports.

**Why this matters**

This is one of the most serious hidden correctness problems in the platform today. It distorts the control plane in a way that is easy to miss because many handlers are â€œmostly idempotentâ€, but not all of them are safely so.

It also means some features appear to work only because the node is accidentally receiving **everything** from the stream, not because subscriptions are correctly wired.

**Validation after fix**

- [x] `crates/messaging::subscribe_durable()` now accepts an optional `filter_subject` and configures JetStream pull consumers with that filter.
- [x] `crates/node/src/main.rs` now subscribes with explicit subject filters across `DEPLOY`, `CONTROL`, `NODE`, `HEALTH`, `PLATFORM`, and `EBPF`.
- [x] `cargo test -p node --test cluster_bootstrap` passed in WSL after the filtered-consumer wiring was added.
- [x] `cargo test -p messaging test_control_event_is_handled_exactly_once_by_intended_filtered_consumer -- --nocapture` passed in WSL.
- [x] `cargo test -p messaging test_node_event_is_handled_exactly_once_by_intended_filtered_consumer -- --nocapture` passed in WSL.

---

### P0-2. JetStream ACK behavior is effectively at-most-once for business-logic failures

- [x] Change durable handler API from `Future<Output = ()>` to `Future<Output = Result<(), PlatformError>>`.
- [x] ACK only on successful business processing.
- [x] NAK / retry on transient failures.
- [x] Quarantine poison messages after retry exhaustion.
- [x] Add tests for deploy failure -> redelivery -> eventual success.

**Evidence**

- `crates/messaging/src/lib.rs:248-269` ACKs every successfully deserialized event after `handler(event).await`, regardless of whether the handler actually completed useful work.
- `crates/node/src/handlers.rs:28-31` defines `EventDispatcher::handle()` as returning `()`.
- `crates/node/src/handlers.rs:378-465` logs and returns on many deploy failures without surfacing failure to JetStream.
- `crates/node/src/handlers.rs:111-129` and `crates/node/src/handlers.rs:345-363` do the same pattern for secrets/config/gateway handling.

**Why this matters**

Durable consumers imply recovery and redelivery safety. Right now a transient failure in artifact fetch, compile, storage write, or config write can permanently lose the event even though the system is using JetStream.

That is a production blocker for deploy reliability.

**Validation after fix**

- [x] `subscribe_durable()` now NAKs when the handler returns `Err` instead of ACKing unconditionally.
- [x] `EventDispatcher::handle()` now returns `Result<(), PlatformError>` so node handlers can participate in ACK/NAK decisions.
- [x] `cargo test -p messaging` passed in WSL with a new redelivery test that forces one handler failure before succeeding.
- [x] `crates/messaging/src/lib.rs` now creates a `QUARANTINE` JetStream stream on `quarantine.>`.
- [x] Durable consumers now publish exhausted handler/deserialization failures to `quarantine.{stream}.{consumer}` with metadata and payload bytes, then send `+TERM`.
- [x] `cargo test -p messaging test_poison_handler_failure_is_quarantined_after_retry_exhaustion -- --nocapture` passed in WSL.

---

## P1 â€” High Findings

### P1-1. `HEALTH`, `PLATFORM`, and `EBPF` streams are created but never subscribed by nodes

- [x] Subscribe nodes to `HEALTH`, `PLATFORM`, and `EBPF` streams.
- [x] Add stream-by-stream subscription tests that verify every defined event class has a live consumer path.
- [x] Add startup diagnostics that log missing subscriptions for defined streams.

**Evidence**

- `crates/messaging/src/lib.rs:164-203` creates `HEALTH`, `PLATFORM`, and `EBPF` streams.
- `crates/node/src/main.rs:812-883` subscribes only to `DEPLOY`, `CONTROL`, and `NODE`.
- `crates/node/src/handlers.rs:193-286` contains handlers for `NodeUpgrade`, `NodeDraining`, `NodeUnderPressure`, `NodePressureRecovered`, and `SecurityIncident`.
- `crates/proxy/src/health_events.rs:20-90` publishes health events.

**Why this matters**

The codebase currently defines and publishes several platform-critical event types that no node is actually consuming. That means upgrade orchestration, pressure coordination, and health propagation are not fully live features yet.

This is especially dangerous because the handlers exist, which can create a false sense of feature completeness.

**Validation after fix**

- [x] `crates/messaging/src/lib.rs` now exposes the authoritative JetStream subject catalog via `JETSTREAM_STREAM_SUBJECT_SPECS`, and `setup_jetstream()` consumes that shared catalog.
- [x] `crates/node/src/main.rs` now logs startup diagnostics for declared stream subjects that lack a matching node durable subscription, and for node subscriptions that are not backed by declared stream subjects.
- [x] `crates/node/src/main.rs` now has coverage for:
  - exact event-subject-to-subscription routing
  - declared stream-subject coverage
  - unbacked node-subscription detection
- [x] `cargo test -p node --bin wasm-node test_subscription_matrix_covers_each_event_type_exactly_once -- --nocapture` passed in WSL.
- [x] `cargo test -p node --bin wasm-node test_node_subscription_specs_cover_all_declared_stream_subjects -- --nocapture` passed in WSL.
- [x] `cargo test -p node --bin wasm-node test_node_subscription_specs_only_reference_declared_stream_subjects -- --nocapture` passed in WSL.

---

### P1-2. Fresh-node bootstrap is multi-responder, and snapshot import is not idempotent

- [x] Introduce a bootstrap session ID / nonce in `NodeJoined` and `StateSnapshot`.
- [x] Ensure exactly one responder is elected, or ensure duplicate snapshots are safely ignored.
- [x] Add an in-memory/imported snapshot guard so only the first valid snapshot is applied.
- [x] Make snapshot import explicitly idempotent before allowing multiple responders.

**Evidence**

- `crates/node/src/handlers.rs:507-514` explicitly allows every node with a smaller ID to respond to a joining node.
- `crates/node/src/handlers.rs:608-735` imports snapshots with real side effects: config saves, route loads, secret writes, artifact-hash saves, and compilation tasks.
- `crates/messaging/src/events.rs:79-105` has no bootstrap session ID or dedupe key in the snapshot protocol.

**Why this matters**

Even before fixing duplicate stream consumption, the bootstrap protocol is designed in a way that permits multiple full-cluster snapshot responses. After fixing stream filters, this would still be a race unless import becomes explicitly deduplicated.

With the current unfiltered `NODE` consumers, the risk is amplified further.

**Validation after fix**

- [x] `crates/messaging/src/events.rs` now carries `bootstrap_session_id` and `bootstrap_nonce` in both `NodeJoined` and `StateSnapshot`.
- [x] `crates/node/src/main.rs` now persists pending/applied bootstrap session metadata so an empty but valid snapshot does not cause endless re-bootstrap on restart.
- [x] `crates/node/src/handlers.rs` now ignores self `NodeJoined`, accepts only the first valid matching snapshot for the active bootstrap session, and ignores duplicates/stale snapshots afterwards.
- [x] `cargo test -p node --test cluster_bootstrap` passed in WSL with updated bootstrap session correlation coverage.

---

### P1-3. Route replay recovery is broken against the canonical wire format, and rebuild is not repeatable

- [x] Deserialize route replay messages using the same `MessageEnvelope<Event>` logic as normal subscribers.
- [x] Stop ACKing replay messages when parsing failed or when replay did not actually process the event.
- [x] Use an ephemeral replay consumer or explicitly reset the durable consumer on rebuild.
- [x] Add repeated-rebuild tests for route-table recovery.

**Evidence**

- `crates/storage/src/integrity.rs:159-229` replays route events by reading raw JSON and expecting a top-level `type` field.
- `crates/messaging/src/lib.rs:76-83` publishes events in `MessageEnvelope` format.
- `crates/storage/src/integrity.rs:161-171` uses a fixed durable consumer name `recovery-routes-replay`.
- `crates/storage/src/integrity.rs:226-229` ACKs messages during replay.

**Why this matters**

This undermines one of the core shared-nothing recovery mechanisms. A node that needs to rebuild local route state from the durable control plane can silently fail to restore history, while still advancing the replay cursor.

That is exactly the kind of subtle failure that becomes catastrophic during real incident recovery.

**Validation after fix**

- [x] `crates/storage/src/integrity.rs` now decodes route replay payloads from `MessageEnvelope<serde_json::Value>` first, with backward-compatible bare-event fallback.
- [x] Route replay now uses an **ephemeral**, filtered pull consumer for `routes.>` instead of a reusable durable cursor.
- [x] `cargo test -p storage` passed in WSL, including new unit tests for replay payload decoding from both envelope-wrapped and legacy bare route events.
- [x] `cargo test -p node --no-run` passed in WSL after the replay-path changes.
- [x] `crates/storage/src/integrity.rs` now includes repeatability coverage for rebuild/replay:
  - `test_replay_routes_from_jetstream_restores_final_route_state`
  - `test_replay_routes_from_jetstream_is_repeatable`

---

### P1-4. Remote-node routing state is wrong: `NodeLoad` carries no address, and receivers fill in their own local address

- [x] Extend `Event::NodeLoad` to include a routable proxy/supervisor address or advertised node endpoint.
- [x] Stop using a local self-address when processing remote node load events.
- [x] Add cluster tests for cross-node least-loaded routing and DNS webhook peer IP propagation.

**Evidence**

- `crates/messaging/src/events.rs:70-76` defines `NodeLoad` without any network address.
- `crates/node/src/handlers.rs:137-152` stores `supervisor_addr: self.supervisor_addr` when processing a `NodeLoad` event.
- `crates/node/src/main.rs:803-807` initializes `supervisor_addr` using `127.0.0.1:<admin.port>`.
- `crates/proxy/src/service.rs:72-78` uses `node.supervisor_addr` for overload routing.
- `crates/proxy/src/node_table.rs:10-17` models that address as the remote nodeâ€™s target.

**Why this matters**

The proxyâ€™s remote-node routing story is structurally incomplete. The node table currently cannot learn an actual remote forwarding address from `NodeLoad`, yet the proxy assumes it can use that field for remote routing decisions.

This is partly latent today because `node_is_overloaded()` returns `false`, but the state model is still wrong and will break as soon as this path is enabled.

**Validation after fix**

- [x] `crates/messaging/src/events.rs` now carries `NodeLoad.proxy_address`.
- [x] `crates/supervisor/src/scaling.rs` now publishes that routable proxy address in node load reports.
- [x] `crates/node/src/main.rs` now shares one `NodeLoadTable` between the dispatcher and the proxy, instead of maintaining disconnected copies.
- [x] `crates/node/src/handlers.rs` now stores the remote nodeâ€™s published proxy address instead of filling in a local self-address.
- [x] `crates/proxy/src/service.rs` now resolves and uses the advertised remote proxy address for cross-node steering.
- [x] `crates/node/src/handlers.rs` now has `test_route_webhook_uses_peer_ips_from_node_load_updates`.
- [x] `crates/e2e/tests/cluster_registry.rs` now has `test_live_overloaded_node_routes_first_request_to_remote_proxy`.
- [x] WSL validations passed:
  - `cargo build -p node --bin wasm-node`
  - `cargo test -p node --bin wasm-node test_route_webhook_uses_peer_ips_from_node_load_updates -- --nocapture`
  - `cargo test -p e2e --test cluster_registry -- --ignored --nocapture test_live_overloaded_node_routes_first_request_to_remote_proxy`

---

### P1-5. Bootstrap snapshot omits gateway policy state

- [x] Extend `StateSnapshot` to include gateway route config state.
- [x] Import gateway config during fresh-node bootstrap.
- [x] Add an integration test where a fresh node joins a cluster with non-default gateway policy already present.

**Evidence**

- `crates/messaging/src/events.rs:93-105` defines `StateSnapshot` with configs, routes, encrypted secrets, and artifact hashes only.
- `crates/node/src/handlers.rs:522-568` prepares only those fields for snapshot export.
- `crates/storage/src/lib.rs:548-646` stores gateway config in persistent storage.
- `crates/node/src/main.rs:1007-1016` loads gateway configs from local storage on startup.

**Why this matters**

A fresh node can join and load apps/routes while still missing existing gateway auth, CORS, rate-limit, or transform policy. That creates policy drift between nodes and can become a security problem, not just a consistency issue.

**Validation after fix**

- [x] `crates/messaging/src/events.rs` now includes `gateway_configs` and `api_keys` in `StateSnapshot`.
- [x] `crates/node/src/handlers.rs` now imports both persisted gateway route config and API-key policy state during bootstrap.
- [x] `crates/node/tests/cluster_bootstrap.rs` now verifies that a fresh node receives non-default gateway policy during the bootstrap flow.

---

### P1-6. Some current behavior only works accidentally because broken consumers receive all subjects

- [x] Audit every event type against actual stream subscription coverage after subject filtering is fixed.
- [x] Add a matrix test: event type -> stream -> consumer -> handler.
- [x] Fix any features currently relying on accidental over-delivery.

**Implemented**

- `crates/node/src/main.rs` now keeps the canonical node subscription matrix in one place via `NODE_SUBSCRIPTION_SPECS`.
- `crates/node/src/main.rs` now has `test_subscription_matrix_covers_each_event_type_exactly_once`, which walks representative events through `Event::subject()` and proves each event type maps to exactly one subscribed stream/filter.
- The audited accidental-over-delivery gaps are closed in the current matrix:
  - `gateway.config.*` is subscribed on `CONTROL`
  - `routes.*` is subscribed on `DEPLOY`, not `NODE`
  - `HEALTH`, `PLATFORM`, and `EBPF` event classes are all represented in the explicit subscription map

**WSL validation**

- `cargo test -p node --bin wasm-node test_subscription_matrix_covers_each_event_type_exactly_once -- --nocapture`
- `cargo test -p node --bin wasm-node test_sanitize_subject_stabilizes_consumer_suffixes -- --nocapture`
- `cargo test -p node --bin wasm-node --no-run`

**Why this matters**

**Validation after fix**

- [x] `crates/messaging/src/lib.rs` now creates a `QUARANTINE` JetStream stream on `quarantine.>`.
- [x] Durable consumers now copy exhausted handler/deserialization failures to `quarantine.{stream}.{consumer}` with metadata and payload bytes, then send `+TERM`.
- [x] `cargo test -p messaging test_poison_handler_failure_is_quarantined_after_retry_exhaustion -- --nocapture` passed in WSL.

Once the subject-filter bug is fixed, some currently â€œworkingâ€ features may stop working until their real subscriptions are added. This needs to be treated as part of the fix, not as a separate later cleanup.

---

## P2 â€” Medium Findings

### P2-1. JetStream stream setup currently defines overlapping EBPF subjects, and messaging durable replay tests fail in WSL

- [x] Fix the `EBPF` stream subject list so JetStream subjects do not overlap.
- [x] Add a WSL/CI test that exercises `setup_jetstream()` directly against a real NATS server.
- [x] Decide whether `ebpf.pressure.recovered.*` should live under a non-overlapping prefix or whether the broader subject should be narrowed.

**Evidence**

- `crates/messaging/src/lib.rs:192-201` defines `EBPF` subjects including both `ebpf.pressure.>` and `ebpf.pressure.recovered.>`.
- WSL validation: `cargo test -p messaging` failed in `crates/messaging/src/tests.rs:113-117` with JetStream error `subject "ebpf.pressure.>" overlaps with "ebpf.pressure.recovered.>"`.
- After provisioning a real NATS+JetStream instance in WSL, `cargo test -p node --tests` still failed in `crates/node/tests/cluster_bootstrap.rs:55`, `124`, `198`, and `283` for the same JetStream overlap error during `setup_jetstream()`.

**Why this matters**

This is not just a test issue â€” it means real JetStream setup can fail depending on server enforcement. A broken stream-definition path is a production control-plane issue.

**Validation after fix**

- [x] `cargo test -p messaging` passed in WSL after narrowing the `EBPF` subjects to non-overlapping single-token wildcards.
- [x] `cargo test -p node --test cluster_bootstrap` passed in WSL against a real NATS+JetStream instance after the same fix.

---

### P2-2. Health transition publication can lose state on publish failure

- [x] Only update `last_status` after a successful publish, or add retry buffering.
- [x] Add tests for NATS outage during health transition.

**Implemented**

- `crates/proxy/src/health_events.rs` now fails closed when the NATS client is already disconnected, waits for publish confirmation, and only advances `last_status` after that path succeeds.
- `crates/proxy/src/health_events.rs` now has `test_health_transition_retries_after_publish_failure`, which stops the shared NATS fixture, verifies the failed transition does not advance cached state, then resumes NATS and verifies the same edge is emitted successfully.

**WSL validation**

- `cargo test -p proxy test_health_transition_retries_after_publish_failure -- --nocapture`
- `cargo test -p proxy --lib -- --nocapture`

**Why this matters**

If NATS is briefly unavailable during a health-state edge transition, that exact transition is dropped and may never be re-emitted.

---

### P2-3. Leader election by string comparison is too weak for bootstrap coordination

- [x] Replace lexicographic node ID comparison with an explicit cluster bootstrap election strategy.
- [x] Decide whether the bootstrap leader is smallest node ID, oldest node, healthiest node, or first responder with lease.

**Implemented**

- `crates/node/src/handlers.rs` now uses explicit first-responder bootstrap coordination: any eligible existing node may publish a snapshot, and the joining node accepts only the first valid `session_id`/`nonce` match and ignores later duplicates.
- `crates/node/src/handlers.rs` now has `test_handle_state_snapshot_accepts_first_matching_session_only`, which proves mismatched snapshots are ignored, the first matching snapshot is applied, and later duplicates do not overwrite state.

**WSL validation**

- `cargo test -p node --bin wasm-node test_handle_state_snapshot_accepts_first_matching_session_only -- --nocapture`
- `cargo test -p node --bin wasm-node --no-run`

**Why this matters**

Bootstrap coordination is now explicit and independent of node ID ordering, so cluster join correctness no longer depends on lexicographic naming conventions.

---

## Deep-audit implementation order

### Immediate distributed-correctness fixes

- [x] P0-1 Subject-filter durable consumers
- [x] P0-2 ACK only on successful business processing
- [x] P1-1 Subscribe nodes to `HEALTH`, `PLATFORM`, and `EBPF`
- [x] P1-2 Make bootstrap import single-shot and idempotent
- [x] P1-3 Fix route replay wire format and replay semantics

### Next distributed-state fixes

- [x] P1-4 Add routable remote node address to load reporting
- [x] P1-5 Include gateway policy in snapshot state
- [x] P1-6 Audit accidental dependencies on over-delivery
- [x] P2-1 Fix overlapping EBPF stream subjects in JetStream setup
- [x] P2-2 Retry/edge buffering for health transitions
- [x] P2-3 Replace string-based bootstrap leader heuristic with an explicit responder election or lease model

---

## Validation checklist for this step

### Eventing correctness

- [x] One event on `CONTROL` is handled exactly once per node
- [x] One event on `NODE` is handled exactly once per node
- [x] Transient deploy failure results in redelivery, not permanent loss
- [x] Malformed message handling does not poison unrelated consumers
- [x] JetStream stream creation succeeds against real NATS without subject-overlap errors

Validation added:

- `cargo test -p messaging test_malformed_message_does_not_block_other_filtered_consumers -- --nocapture`
- `cargo test -p messaging -- --nocapture`

### Bootstrap and recovery

- [x] Only one snapshot import is applied per fresh-node join
- [x] Gateway config is present on fresh node after bootstrap
- [x] Route-table partial rebuild restores correct route state from JetStream history
- [x] Repeating the same rebuild produces the same local state

Validation added:

- `cargo test -p node --bin wasm-node test_handle_state_snapshot_accepts_first_matching_session_only -- --nocapture`
- `cargo test -p storage test_replay_routes_from_jetstream_restores_final_route_state -- --nocapture`
- `cargo test -p storage test_replay_routes_from_jetstream_is_repeatable -- --nocapture`
- `cargo test -p storage -- --nocapture`

### Cluster routing state

- [x] Remote node table learns a real routable address
- [x] Least-loaded routing uses a remote node's actual endpoint
- [x] DNS/webhook peer IP updates reflect real peer nodes, not the local node
