# P10-07 PostgreSQL clock and recovery validation

Status: **LOCAL MICROVM PASS / PRODUCTION OPERATOR CONTROLS PENDING**

The run started on 2026-09-01 and completed on 2026-09-02 in WSL2. It used
three platform microVMs behind the local HAProxy front door, one separate NATS
microVM, and one separate PostgreSQL 17.11 microVM. The OpenID Connect WASI Hub
was the stateful application fixture. PostgreSQL is not a required Wasm Cloud
Platform service; this gate validates how a timestamp-sensitive application
dependency behaves with the platform.

## Final acceptance results

| Gate | Result |
|---|---|
| PostgreSQL image | Schema 5; 1-GiB ext4; read-only `e2fsck` passed |
| Initial time gate | PostgreSQL started only after Chrony synchronization |
| Focused suspend drill | Exact recorded VMM paused for 90 seconds; degraded metrics appeared; two sources recovered in about 25 seconds |
| Canonical clock validator | PASS; before skew 0.444 s, Prometheus skew 1.522 s, after skew 1.151 s |
| Clock alerts | Missing metric and unavailable source fired and resolved; skew rule promtool-tested and clear after recovery |
| Alert contract | Five rule files, 35 rules, 28 always-present metric names, seven firing/resolved delivery pairs, deduplication passed |
| OIDC recovery | 26 tables, 11/11 migrations, six Playwright checks |
| Recovery timing | Backup 1.316 s; restore 6.928 s; ready 44.844 s; full journey 53.154 s |
| Recovery clock gate | Source database skew 1.78 s, below the 5-second limit |
| Final health | Three nodes healthy; OIDC `database=ok`; Chrony synchronized with two sources; no active Prometheus alert |
| Cleanup invariants | Original two routes and two applications only; no recovery container |

## Problems found and fixed

1. The first image lacked a usable HTTP metrics applet and had writable-path
   ownership errors for Chrony. The image now installs `busybox-extras` and
   assigns explicit runtime ownership.
2. Prometheus 3 rejected the minimal guest endpoint without a content type. The
   scrape config now declares the Prometheus text fallback protocol.
3. PostgreSQL exporter options were malformed and its WAL collector was not
   appropriate for this fixture. The DSN was simplified, WAL collection was
   disabled, and provisioning now waits for the required series.
4. A long host/session pause showed that a responsive database can still have a
   stale clock. Synchronization, usable-source count, database epoch, and sample
   age are now separate metrics and alerts.
5. Chrony recovery commands initially returned `501 Not authorised` because
   they fell back to UDP command transport. The image now enables a local Unix
   command socket and disables the UDP command port.
6. Root-owned command-socket directories were rejected by Alpine Chrony even
   when the group was Chrony. `/run/chrony` must be owned by `chrony:chrony`
   with mode `0770`.
7. Resetting source measurements on every 30-second observer iteration
   repeatedly discarded valid new samples and prevented source selection. The
   observer now resets once per degraded episode, then continues forced steps
   and bursts until synchronized.
8. A healthy OpenTelemetry Collector does not necessarily expose
   `otelcol_exporter_send_failed_spans_total`. It is event-created, like the
   enqueue-failure counter, and is tested deterministically rather than required
   in every healthy scrape.

The serial logs for failed attempts are retained because they explain the
production-significant behavior and prevent regressions from being dismissed as
test flakiness.

## Evidence

- `clock-result.json`: canonical exact-PID suspend/recovery and alert result.
- `alerting-result.json`: live rule, metric, delivery, resolution, and
  deduplication result.
- `focused-suspend-recovery/postgres-serial.log`: successful authenticated
  one-time reset and source recovery.
- `final-postgres-serial.log`: final canonical clock and recovery-run serial
  snapshot.
- `disaster-recovery/result.json`: final backup, restore, readiness, browser,
  schema, and clock measurements.
- `disaster-recovery/recovery-manifest.json`: redacted topology/application
  recovery manifest.
- `failed-attempt-*`: retained serial evidence for each corrected defect.
- `SHA256SUMS`: integrity list for this evidence directory.

No runtime credential, private key, database password, or disposable encryption
passphrase is intentionally stored in this directory. The encrypted database
snapshot is evidence of the local round trip only; its disposable encryption
key was destroyed, so it is not a recoverable production backup.

## Production boundary

This pass proves the platform can run the OIDC WASI workload while its database
clock degrades and recovers, and that the monitoring contract detects the local
fault. It does not qualify production NTP/NTS availability, managed PostgreSQL,
database HA, scheduled RPO, off-host replication, immutable retention, or
KMS/HSM key custody. Those remain operator-owned production gates. Use at least
three independent approved sources, authenticated time where available, and
page before the documented five-second application clock limit is crossed.

## Environment state

The final environment remains running for interactive testing, as requested.
The public application is available at `http://127.0.0.1:8088`. When testing is
finished, destroy only the recorded topology:

```bash
bash scripts/vm/destroy-testbed.sh \
  --state-file .prod-validation-p10-07-state.json
```
