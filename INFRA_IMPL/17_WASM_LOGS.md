# Step 17 — Wasm stdout/stderr Capture & Log Forwarding

## Goal
When a Wasm app calls `println!()`, `eprintln!()`, or uses `tracing`, those bytes go to
the Wasm's WASI stdout/stderr. Without capture, they vanish into `/dev/null`.

---

## Context & Rationale

### The Problem This Solves

A Wasm module runs inside a sandboxed WASI environment. Its stdout and stderr are
virtual — they have no relationship to the host process's stdout/stderr. Without explicit
capture, every `println!()` and `tracing::info!()` call inside the Wasm app produces
output that is silently discarded.

For debugging and operations this is unacceptable. When `api-users:v2` returns 500 errors,
operators need to see the app's logs to diagnose the cause.

### Why Not Just Attach the Wasm stdout to the Node's stdout?

Attaching all Wasm modules to the node's stdout would mix logs from all apps together
with the node's own logs. There would be no way to filter by app, correlate with a
specific request, or stream to an external log aggregator.

The architecture instead:
1. Each Wasm instance gets its own `WasiPipe` for stdout and stderr
2. Background tasks drain the pipes and push structured log records into a shared channel
3. The `LogDispatcher` routes records to configured sinks (node stdout, HTTP endpoint, NATS)
4. Operators can filter by `app_id` and tail logs via the admin SSE endpoint

This gives per-app log isolation while allowing flexible forwarding.

### The WASI Pipe Mechanism

`wasmtime-wasi` provides pipe streams — a pair of connected `Read`/`Write` ends:
- The **write end** is given to the Wasm module as its `stdout`/`stderr`
- The **read end** is held by the Supervisor

The Wasm module writes log bytes to its virtual stdout; the Supervisor reads them out of
the pipe. This is analogous to a Unix pipe (`|`) but entirely in-process.

The drain must run on a **blocking thread** (`spawn_blocking`) because reading from a pipe
is a blocking operation — it blocks until bytes are available. Blocking on an async thread
would stall the Tokio executor.

### Why Parse Structured JSON Logs?

The `WasmLogRecord.structured` field is populated if the log line is valid JSON. This
enables:
- **Searchable fields**: if the app logs `{"level":"ERROR","message":"db timeout","query":"..."}`,
  log aggregators (Grafana Loki, Elasticsearch) can index the `query` field and make
  it searchable
- **Trace correlation**: if the app includes `{"trace_id":"abc123"}`, the log line can
  be linked to an OpenTelemetry trace span from Pingora

Apps that use `tracing-subscriber` with `.json()` output automatically produce this format.
Apps that use `println!()` produce plain text — captured as `structured: None`.

### The Batching Strategy (Why 500ms / 100 records)

Writing each log line individually to an HTTP endpoint or NATS would produce thousands
of tiny payloads per second under load — overwhelming the downstream aggregator.

Batching at 100 records or 500ms (whichever comes first) strikes the right balance:
- **100 records**: prevents single bursts from filling the channel
- **500ms**: ensures logs appear in the aggregator within half a second even at low volume

The channel capacity (4096 records) provides backpressure: if the downstream aggregator
is slow, the channel fills up and new records are dropped with a warning. This protects
the request handling path from being blocked by slow log forwarding.

### Admin SSE Log Tailing: Why Server-Sent Events?

Operators need a way to see logs in real time from the terminal. WebSockets work but
require a full duplex connection. SSE (Server-Sent Events) is simpler: a long-lived HTTP
GET that the server pushes data to.

`curl -N http://node:9090/logs/api-users:v2` opens an SSE stream. This works with any
HTTP client and requires zero JavaScript.

---

This file covers:
1. Intercepting stdout/stderr at the `WasiEnv` level
2. Piping the bytes to an async channel
3. Parsing structured JSON logs from the Wasm app
4. Forwarding to a log aggregator (Vector / FluentBit / stdout of the node process)

---

## 1. How WASI Captures stdout/stderr

`wasmtime-wasi` allows substituting custom streams for stdout and stderr.

```rust
// crates/runtime/src/wasi.rs (extend build_wasi_env)
use wasmtime_wasi::pipe::MemoryOutputPipe;
use tokio::sync::mpsc;

pub struct WasiStreams {
    /// Receive log lines from the Wasm module's stdout.
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    /// Receive log lines from the Wasm module's stderr.
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
}

pub fn build_wasi_env_with_capture(
    store: &mut wasmtime::Store<()>,
    cfg: &WasiConfig,
) -> Result<(wasmtime_wasi::WasiCtx, WasiStreams), common::error::PlatformError> {
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(512);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(512);

    // MemoryOutputPipe is a memory buffer provided by wasmtime-wasi.
    // The Write end goes into WasiCtx; the Read end is drained by the Supervisor.
    let stdout_write = MemoryOutputPipe::new(10000);
    let stderr_write = MemoryOutputPipe::new(10000);

    // Spawn tasks that drain the pipes and push into the mpsc channels
    let app_id = cfg.app_name.clone();
    let app_id_err = app_id.clone();

    tokio::task::spawn_blocking(move || {
        drain_pipe(stdout_read, stdout_tx, &app_id, "stdout");
    });
    tokio::task::spawn_blocking(move || {
        drain_pipe(stderr_read, stderr_tx, &app_id_err, "stderr");
    });

    let mut builder = wasmtime_wasi::WasiCtxBuilder::new();
    for (k, v) in &cfg.env_vars {
        builder.env(k, v);
    }
    builder = builder
        .env("PORT", &cfg.wasm_port.to_string())
        .stdout(Box::new(stdout_write))
        .stderr(Box::new(stderr_write))
        .sandbox_fs(Default::default())
        .allow_connect(true);

    let wasi_env = builder
        .finalize(store)
        .map_err(|e| common::error::PlatformError::Runtime(format!("WasiEnv finalize: {e}")))?;

    Ok((wasi_env, WasiStreams { stdout_rx, stderr_rx }))
}

fn drain_pipe(
    pipe: wasmtime_wasi::pipe::MemoryOutputPipe,
    tx: mpsc::Sender<Vec<u8>>,
    app_id: &str,
    stream: &str,
) {
    use std::io::BufRead;
    let reader = std::io::BufReader::new(&mut pipe);
    for line in reader.lines() {
        match line {
            Ok(l) if !l.is_empty() => {
                let _ = tx.blocking_send(l.into_bytes());
            }
            Err(e) => {
                tracing::debug!(app = app_id, stream, error = %e, "pipe read error");
                break;
            }
            _ => {}
        }
    }
}
```

---

## 2. Log Record Structure

```rust
// crates/metrics/src/lib.rs (add)
use serde::{Deserialize, Serialize};

/// A single log line emitted by a Wasm module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmLogRecord {
    /// Which app produced this log.
    pub app_id: String,
    /// Which instance (UUID).
    pub instance_id: String,
    /// "stdout" or "stderr"
    pub stream: String,
    /// ISO-8601 timestamp (added by the Supervisor, not the app).
    pub node_timestamp: String,
    /// The raw log line.
    pub message: String,
    /// If the line is valid JSON, the parsed fields are preserved here.
    pub structured: Option<serde_json::Value>,
    /// Trace ID forwarded from the request context (if injected as TRACE_ID env var).
    pub trace_id: Option<String>,
}

impl WasmLogRecord {
    pub fn from_line(
        app_id: &str,
        instance_id: &str,
        stream: &str,
        line: &[u8],
        trace_id: Option<String>,
    ) -> Self {
        let message = String::from_utf8_lossy(line).to_string();
        // Try to parse as structured JSON (tracing-subscriber with .json() format)
        let structured = serde_json::from_str::<serde_json::Value>(&message).ok();
        WasmLogRecord {
            app_id: app_id.to_string(),
            instance_id: instance_id.to_string(),
            stream: stream.to_string(),
            node_timestamp: chrono::Utc::now().to_rfc3339(),
            message,
            structured,
            trace_id,
        }
    }
}
```

---

## 3. Log Dispatcher

Receives raw bytes from both streams, parses them, and routes to configured sinks.

```rust
// crates/metrics/src/log_dispatcher.rs
use super::WasmLogRecord;
use tokio::sync::mpsc;
use std::sync::Arc;

pub enum LogSink {
    /// Print to the node process stdout (dev mode).
    NodeStdout,
    /// Forward via HTTP to a Vector/FluentBit aggregator.
    Http { endpoint: String, client: reqwest::Client },
    /// Write to a NATS subject (for cross-node log collection).
    Nats { subject: String, bus: messaging::NatsBus },
}

pub struct LogDispatcher {
    sinks: Arc<Vec<LogSink>>,
    tx: mpsc::Sender<WasmLogRecord>,
}

impl LogDispatcher {
    pub fn start(sinks: Vec<LogSink>) -> Self {
        let (tx, mut rx) = mpsc::channel::<WasmLogRecord>(4096);
        let sinks = Arc::new(sinks);
        let sinks_clone = sinks.clone();

        tokio::spawn(async move {
            // Batch records for efficiency
            let mut batch: Vec<WasmLogRecord> = Vec::with_capacity(100);
            let mut flush_interval = tokio::time::interval(
                std::time::Duration::from_millis(500)
            );

            loop {
                tokio::select! {
                    Some(record) = rx.recv() => {
                        batch.push(record);
                        if batch.len() >= 100 {
                            flush_batch(&sinks_clone, &mut batch).await;
                        }
                    }
                    _ = flush_interval.tick() => {
                        if !batch.is_empty() {
                            flush_batch(&sinks_clone, &mut batch).await;
                        }
                    }
                }
            }
        });

        LogDispatcher { sinks, tx }
    }

    pub fn sender(&self) -> mpsc::Sender<WasmLogRecord> {
        self.tx.clone()
    }
}

async fn flush_batch(sinks: &[LogSink], batch: &mut Vec<WasmLogRecord>) {
    for sink in sinks {
        match sink {
            LogSink::NodeStdout => {
                for record in batch.iter() {
                    // Pretty-print or forward as JSON line
                    let line = serde_json::to_string(record).unwrap_or_default();
                    println!("{line}");
                }
            }
            LogSink::Http { endpoint, client } => {
                let payload = serde_json::to_vec(batch).unwrap_or_default();
                if let Err(e) = client.post(endpoint).body(payload)
                    .header("content-type", "application/json")
                    .send().await {
                    tracing::warn!(error = %e, "log HTTP export failed");
                }
            }
            LogSink::Nats { subject, bus } => {
                for record in batch.iter() {
                    let payload = serde_json::to_vec(record).unwrap_or_default();
                    bus.client().publish(subject.clone(), payload.into()).await.ok();
                }
            }
        }
    }
    batch.clear();
}
```

---

## 4. Integration in Supervisor spawn()

```rust
// crates/supervisor/src/lib.rs — spawn() (modified)
// After building WASI env, capture the streams:
let (wasi_env, streams) = runtime::wasi::build_wasi_env_with_capture(&mut store, &wasi_cfg)?;

let app_id_log = app_id.clone();
let instance_id_log = id.clone();
let log_tx = self.log_dispatcher.sender();

// Drain stdout
tokio::spawn(async move {
    let mut rx = streams.stdout_rx;
    while let Some(line) = rx.recv().await {
        let record = WasmLogRecord::from_line(
            &app_id_log.0, &instance_id_log.0.to_string(), "stdout", &line, None,
        );
        log_tx.send(record).await.ok();
    }
});

// Drain stderr
let app_id_log2 = app_id.clone();
let instance_id_log2 = id.clone();
let log_tx2 = self.log_dispatcher.sender();
tokio::spawn(async move {
    let mut rx = streams.stderr_rx;
    while let Some(line) = rx.recv().await {
        let record = WasmLogRecord::from_line(
            &app_id_log2.0, &instance_id_log2.0.to_string(), "stderr", &line, None,
        );
        log_tx2.send(record).await.ok();
    }
});
```

---

## 5. Wasm App: Structured Logging Setup

For the Axum app inside Wasm, use `tracing-subscriber` with JSON output.
This makes the node's log parser capture structured fields (level, target, span, etc.).

```rust
// apps/hello-axum/src/main.rs
fn init_logging() {
    tracing_subscriber::fmt()
        .json()                        // Machine-parseable JSON
        .with_target(true)
        .with_thread_ids(false)        // No thread IDs in Wasm
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("LOG_LEVEL")
        )
        .init();
}

#[tokio::main]
async fn main() {
    init_logging();
    tracing::info!(app = "hello-axum", "starting up");
    // ...
}
```

The Supervisor will see JSON lines like:
```json
{"timestamp":"2026-04-05T12:00:00Z","level":"INFO","target":"hello_axum","fields":{"message":"starting up","app":"hello-axum"}}
```

---

## 6. node.toml: Log Config

```toml
[logging]
# "stdout" | "http" | "nats" — can be a list
sinks = ["stdout", "http"]

# HTTP sink (Vector or FluentBit endpoint)
[logging.http]
endpoint = "http://vector.internal:9200/logs"

# NATS sink
[logging.nats]
subject = "logs.wasm"

# Maximum lines to buffer before dropping (backpressure)
buffer_capacity = 4096
flush_interval_ms = 500
```

---

## 7. Admin API: Live Log Streaming (SSE)

Allow operators to tail logs for a specific app via the admin API.

```rust
// crates/proxy/src/admin.rs (add)
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

async fn stream_logs(
    Path(app_id): Path<String>,
    State(s): State<AdminState>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(100);
    // Register tx with the log dispatcher for this app_id
    s.log_registry.subscribe(&app_id, tx).await;

    let stream = ReceiverStream::new(rx).map(|line| {
        Ok(SseEvent::default().data(line))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// Route: GET /logs/:app_id  (streaming)
// Usage: curl -N http://localhost:9090/logs/api-users:v2
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Stream Capture
- [ ] `build_wasi_env_with_capture()` returns a `WasiEnv` and a `WasiStreams` with two receivers
- [ ] A Wasm module that calls `println!("hello")` delivers `"hello"` to the stdout receiver
- [ ] A Wasm module that calls `eprintln!("error")` delivers `"error"` to the stderr receiver
- [ ] Empty lines from the Wasm module do not produce empty log records
- [ ] Closing the Wasm instance closes the pipe, which terminates the drain task cleanly

### Log Record
- [ ] Every `WasmLogRecord` contains `app_id`, `instance_id`, `stream`, `node_timestamp`, and `message`
- [ ] A JSON-formatted log line from the Wasm app populates the `structured` field
- [ ] A plain text log line sets `structured` to `None` (no parse error)
- [ ] The `trace_id` field is populated when a `TRACE_ID` env var was injected

### Log Dispatcher
- [ ] `LogDispatcher::start()` starts a background task that never blocks the Supervisor
- [ ] Records are batched and flushed every 500ms or when the batch reaches 100 entries
- [ ] The `NodeStdout` sink prints records to the node process stdout as JSON lines
- [ ] The `Http` sink posts batches to the configured endpoint; failures are logged as warnings — not panics
- [ ] If the internal channel is full, new records are dropped with a warning — not blocked

### Admin API Live Tail
- [ ] `GET /logs/:app_id` returns a streaming SSE response
- [ ] New log lines from the app appear in the SSE stream within 500ms
- [ ] `curl -N http://localhost:9090/logs/api-users:v1` works from the terminal
- [ ] The stream closes cleanly when the client disconnects

### Wasm App Integration
- [ ] The Wasm app uses `tracing-subscriber` with `.json()` output
- [ ] Log lines are parseable `WasmLogRecord.structured` JSON with `level`, `target`, and `message` fields
- [ ] `LOG_LEVEL` env var controls log verbosity (e.g. `LOG_LEVEL=debug` enables debug logs)

### Tests
- [ ] A test runs a Wasm module that prints 5 lines and verifies all 5 are received via the stdout receiver
- [ ] A test verifies that a JSON log line from the app correctly populates the `structured` field
