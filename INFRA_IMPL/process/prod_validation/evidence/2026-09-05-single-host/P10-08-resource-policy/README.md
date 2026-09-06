# P10-08 resource-policy evidence

Date: 2026-09-05  
Topology: three 2-GiB platform microVMs, separate NATS and PostgreSQL microVMs,
HAProxy front door, disposable Prometheus/Alertmanager/Tempo/Collector stack.

## Result

P10-08 passed for the local production-like envelope.

- Node image schema 14 configured a 1,536-MiB platform memory budget inside
  each approximately 1,981-MiB guest boundary, leaving explicit guest headroom.
- Each node had 1,571 MiB free against a 512-MiB hard disk reserve and 122,908
  free inodes against a 10,000-inode reserve. All nodes reported `healthy` and
  accepted requests.
- Process memory after the workload was 286-299 MiB (18-19% of the effective
  budget).
- The OIDC mixed soak completed 600/600 requests at 5 requests/second for 120
  seconds: 300 frontend, 150 discovery, 120 database-readiness, and 30 login
  requests. Every response was HTTP 200. Route p99 was 15.91, 13.05, 64.18,
  and 225.11 ms respectively.
- A valid component declaring four 512-MiB instances (2,048 MiB) was published
  as a negative test. All three nodes rejected it before persistence; the OIDC
  backend remained database-ready.
- Promtool and the live delivery gate passed 36 rules and 30 required live
  metric names. The configured disk-reserve metrics drive hard and warning
  alerts. Seven synthetic operational categories each produced one deduplicated
  firing delivery and one resolved delivery after three identical submissions.
- Prometheus exposed 12/12 healthy targets and no alert remained after the
  initial deployment error-rate window expired.

The first optimized GNU-linker attempt restarted WSL during peak link memory.
Rebuilding with Clang/LLD and two jobs completed the same LTO release profile.
This is a build-host sizing observation, not a 2-GiB platform-node runtime
failure.

The OIDC frontend install reported one moderate application dependency advisory.
It is application remediation evidence and is not classified as a platform
resource-policy blocker.

The application seeder printed a disposable local API key under a label that
the testbed redaction filter did not previously recognize. The key was not
retained in this evidence package, and the deployment script now also redacts
API-key, client-secret, and credential labels. Any credential exposed in CI or
operator output must still be treated as compromised and rotated.

## Evidence files

- `RESULT_SUMMARY.json`: exact per-node capacity values, soak count, admission
  result, and final OIDC readiness;
- `summary.json` and `soak-5-*.json`: per-route sustained workload results;
- `alerting-result.json`: current rule inventory, live expression queries,
  required metrics, and notification delivery results.

Production promotion still needs the selected host/cgroup and volume values,
the exact signed candidate, a longer operator-defined workload, real volume
growth/recovery, N-1 resource evidence, and the real paging path.
