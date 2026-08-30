# Production alerting validation

This runbook validates the Wasm Cloud Platform alert expressions and delivery
integration. Prometheus, Alertmanager, paging providers, and ticketing systems
remain operator-owned services; the platform pipeline qualifies its rules and
its integration with those services.

## Tracked rule set

The canonical rule files are:

- `deploy/prometheus/admin_auth_alerts.yml`;
- `deploy/prometheus/platform_resource_alerts.yml`;
- `deploy/prometheus/validation_alerts.yml`;
- `deploy/prometheus/wasi_policy_alerts.yml`.

Every rule must use an emitted metric with bounded labels and must state a
severity. Production overlays must assign an owner and a tested runbook URL.
The production Alertmanager route must group on stable labels, set bounded
repeat intervals, inhibit derivative alerts where appropriate, and deliver to
an independently operated receiver.

`AdminAuthDisabled` uses the explicit `wasm_admin_auth_enabled` gauge. Request
counters cannot distinguish disabled authentication from a protected API that
has received no traffic. Prometheus `rate()` is per second; policy limits
documented per minute must divide their threshold by 60.

## Deterministic rule tests

Run in WSL2:

```bash
podman run --rm --entrypoint /bin/promtool \
  -v "$PWD/deploy/prometheus:/rules:ro" -w /rules \
  docker.io/prom/prometheus:v3.5.0 \
  test rules tests/alert_rules.test.yml
```

The fixture supplies representative series that cross the firing threshold for
all 32 tracked expressions. Any added, removed, or renamed rule must update the
fixture and the expected inventory in `scripts/vm/validate-alerting.sh`.

## Live microVM validation

After provisioning the production-like testbed and observability stack, run:

```bash
bash scripts/vm/validate-alerting.sh \
  --state-file .prod-validation-single-host-state.json \
  --output /tmp/alerting-result.json
```

The validator:

1. resolves exact Prometheus and Alertmanager container identities from the
   companion lifecycle state;
2. checks the live Prometheus and Alertmanager configurations;
3. checks all four tracked rule files and runs the deterministic test fixture;
4. requires the exact 32-rule inventory;
5. executes every loaded expression against live Prometheus;
6. requires every always-present source metric;
7. sends three identical test alerts for each required operational category;
8. verifies one firing delivery per category, one resolved delivery after
   recovery, and no duplicate notification.

The local webhook recorder is deliberately state-scoped and writes a mode-0600
JSONL file. It proves Alertmanager routing behavior only; it is not a production
notification destination.

Some counter vectors have no live series until their first event. The current
event-created set is:

- `otelcol_exporter_enqueue_failed_spans_total`;
- `wasm_ebpf_ring_buffer_dropped_events_total`;
- `wasm_ebpf_ring_buffer_drop_counter_read_errors_total`.

Their absence during a healthy scrape is valid. Their spelling and threshold
behavior remain covered by deterministic inputs and the corresponding live
fault drills.

## Required production evidence

Before promotion, replace the local receiver with the real on-call path and
prove:

- authentication and TLS to the notification provider;
- one firing and one resolved message reach the intended team;
- duplicate evaluations do not create duplicate pages;
- grouping, inhibition, silence permissions, and repeat intervals match policy;
- every page includes environment, cluster, instance/node or application,
  severity, owner, and a reachable runbook;
- receiver failure and Alertmanager peer failure are themselves monitored;
- notification history is retained according to incident-response policy.

Do not generate destructive production faults merely to test routing. Validate
expressions deterministically, use a synthetic receiver-safe alert in staging,
and reuse controlled failure evidence from the microVM plan.
