# P10-04 alerting remediation evidence

Validation date: 2026-08-30  
Scope: three schema-10 Firecracker platform nodes, HAProxy, PostgreSQL-backed
OIDC Hub, state-scoped Prometheus 3.5.0 and Alertmanager 0.28.1  
Result: **LOCAL PASS / PRODUCTION RECEIVER AND OWNERSHIP EVIDENCE PENDING**

## Proven

- The live Prometheus configuration, four loaded rule files, and Alertmanager
  configuration pass their native validation tools.
- `deploy/prometheus/tests/alert_rules.test.yml` supplies positive threshold
  inputs for all 32 expressions and `promtool test rules` passes.
- Every loaded expression was submitted to the live Prometheus query API. All
  32 returned a vector successfully and matched zero series in the final healthy
  state.
- Twenty-five always-present source metrics were present. Three event-created
  counter vectors were correctly absent in the final healthy scrape and remain
  covered by deterministic inputs and earlier fault drills.
- Three identical Alertmanager submissions were made for each required category:
  node down, application not ready, PostgreSQL monitoring down, NATS monitoring
  down, elevated HAProxy 5xx rate, node disk exhaustion, and telemetry export
  failure. The state-scoped receiver recorded exactly seven firing deliveries,
  no duplicate page, and exactly seven resolved deliveries.
- The receiver output is mode 0600, its exact container and path are lifecycle
  state, and the receiver stops gracefully on SIGTERM.
- All three schema-10 nodes expose `wasm_admin_auth_enabled=1`, accept requests,
  and serve one healthy frontend and backend OIDC instance. Public readiness
  remained `database=ok` throughout the rolling replacement.
- Final Prometheus state has 11/11 targets up and no firing alert.

## Defects corrected

1. `AdminAuthDisabled` inferred configuration from zero request counters and
   `process_start_time_seconds`. An unused protected API looked identical to
   disabled authentication. Nodes now export the effective auth state directly.
2. Six WASI-policy rules compared per-second `rate()` output with numeric
   thresholds documented per minute. The alerts were 60 times less sensitive
   than their descriptions. Their expressions now convert the documented
   per-minute limits to per-second values.
3. Elevated HTTP errors had no loaded production-validation rule. The tracked
   rule now evaluates HAProxy backend 5xx responses as a fraction of all backend
   responses and fires above 5% for two minutes.
4. The local Alertmanager receiver discarded notifications, so delivery and
   deduplication could not be proven. A state-scoped recorder now provides
   redacted firing/resolution evidence.
5. The first recorder process required SIGKILL during replacement. Its final
   implementation handles SIGTERM; an exact stop/start test completed in under
   one second and returned healthy.

## Evidence boundary

Deterministic inputs prove expression thresholds, and the state-scoped webhook
proves Alertmanager grouping, delivery, resolution, and deduplication. Earlier
microVM phases supplied real node, NATS, PostgreSQL, Collector, trace-backend,
and eBPF interruption evidence. Disk exhaustion and elevated 5xx conditions
were not forced against the healthy live application because the same behavior
can be proven without damaging the filesystem or manufacturing an outage.

Production still needs the operator-selected authenticated receiver, real
on-call ownership, reachable runbook URLs, inhibition/silence policy, HA
Alertmanager behavior, and provider delivery evidence. The local recorder is
not part of the production architecture.
