# metrics

Observability infrastructure for the Wasm Cloud Platform.

## Overview

The `metrics` crate provides a unified observability layer combining Prometheus metrics export, execution sample collection and aggregation, configurable log dispatching, and OpenTelemetry OTLP tracing. It serves as the central hub for all telemetry data produced by platform components.

**Core capabilities:**

- **Prometheus metrics export** — Standard counter/gauge/histogram metrics exposed for scraping
- **Execution sample aggregation** — Per-minute `MetricBucket` collection of WASM invocation samples
- **Log dispatching** — Async log routing to configurable sinks (stdout, HTTP, NATS)
- **OTLP tracing** — OpenTelemetry trace export via OTLP protocol

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                  MetricsCollector                    │
│  ┌───────────────┐  ┌────────────────────────────┐  │
│  │ ExecutionSample│  │     MetricBucket (per-min) │  │
│  │   ingestion    │──▶│ latency_samples, counts    │  │
│  └───────────────┘  └────────────────────────────┘  │
├─────────────────────────────────────────────────────┤
│                   Metrics                           │
│  ┌────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │ Policy     │ │ Health       │ │ Nats         │  │
│  │ Metrics    │ │ Metrics      │ │ Metrics      │  │
│  └────────────┘ └──────────────┘ └──────────────┘  │
│  ┌────────────┐ ┌──────────────┐                    │
│  │ Recovery   │ │              │                    │
│  │ Metrics    │ │              │                    │
│  └────────────┘ └──────────────┘                    │
├─────────────────────────────────────────────────────┤
│                 LogDispatcher                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Stdout   │  │  HTTP    │  │  NATS    │          │
│  │ Sink     │  │  Sink    │  │  Sink    │          │
│  └──────────┘  └──────────┘  └──────────┘          │
├─────────────────────────────────────────────────────┤
│              OpenTelemetry OTLP                      │
│         (trace export via OTLP protocol)             │
└─────────────────────────────────────────────────────┘
```

**Data flow:**

1. Components emit `ExecutionSample` and `WasmLogRecord` instances
2. `MetricsCollector` aggregates samples into time-bucketed `MetricBucket` instances
3. `LogDispatcher` routes log records to configured sinks
4. Prometheus scrapes metric endpoints; OTLP exports traces

## Public API

### Core Types

| Type | Description |
|------|-------------|
| `ExecutionSample` | Single WASM invocation measurement (latency, status, metadata) |
| `WasmLogRecord` | Structured log entry from a WASM guest |
| `MetricsCollector` | Collects and aggregates execution samples into per-minute buckets |
| `MetricBucket` | Time-bucketed aggregation of execution samples for a single minute |

### Metric Types

| Type | Description |
|------|-------------|
| `Metrics` | Top-level metrics registry (policy, health, NATS, recovery) |
| `PolicyMetrics` | Policy evaluation counters and histograms |
| `HealthMetrics` | Application and dependency health gauges |
| `NatsMetrics` | NATS connection and message metrics |
| `RecoveryMetrics` | Component recovery attempt tracking |

### Log Dispatch

| Type | Description |
|------|-------------|
| `LogDispatcher` | Async log router dispatching to configured sinks |
| `LogSink` | Trait for pluggable log output destinations |
| `StdoutSink` | Writes logs to standard output |
| `HttpSink` | POSTs log batches to an HTTP endpoint |
| `NatsSink` | Publishes log records to a NATS subject |

## Known Issues & Improvements

### Correctness

- **Fragile string parsing for disk/memory metrics** — Parsing `DependencyHealth` messages via string splitting is brittle and error-prone
- **NATS monitor is not started by the node** — `NatsMetrics` and `nats_monitor_loop` are implemented, but the current node startup path does not register or spawn them

### Reliability

- **No graceful shutdown** — No shutdown signaling or join handle is exposed for the collector, log dispatcher, or NATS monitor; in-flight data may be lost
- **No rate limiting on log dispatch** — Uncontrolled log volume can overwhelm sinks
- **Failed sink deliveries are not retried** — HTTP failures and non-success statuses are warned about, while NATS publish errors are discarded; the batch is cleared after one attempt

### Performance

- **Unbounded `latency_samples` Vec in `MetricBucket`** — Should use an HDR histogram for bounded, statistically meaningful latency tracking
- **Hardcoded 7-day retention** — Collector retention period is not configurable

### Design

- **RecoveryMetrics owns its own Registry** — Invisible to the Prometheus scraper; metrics are never exported
- **Registries are constructed inconsistently** — `Metrics` owns the main registry, health and eBPF metrics accept it, while `RecoveryMetrics` creates a separate registry
- **Delivery outcomes are not returned to producers** — the bounded dispatcher sender provides queue backpressure, but producers cannot observe the later per-sink result

## Security Considerations

- **Log data exfiltration** — HTTP and NATS sinks transmit log data over the network. Ensure endpoints are authenticated and use TLS. HTTP failures are logged but the batch is not retried
- **No rate limiting** — A compromised or misbehaving component could flood log sinks, causing denial of service or masking malicious activity in the noise
- **Metric labels from untrusted input** — If application identifiers or tenant IDs are used as Prometheus labels without sanitization, cardinality explosions or injection attacks are possible
- **OTLP endpoint credentials** — Ensure OTLP export endpoints are properly authenticated and use TLS to prevent trace data leakage
