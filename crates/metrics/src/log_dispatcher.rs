// crates/metrics/src/log_dispatcher.rs
use crate::WasmLogRecord;
use std::sync::Arc;
use tokio::sync::mpsc;

pub enum LogSink {
    /// Print to the node process stdout (dev mode).
    NodeStdout,
    /// Forward via HTTP to a Vector/FluentBit aggregator.
    Http {
        endpoint: String,
        client: reqwest::Client,
    },
    /// Write to a NATS subject (for cross-node log collection).
    Nats {
        subject: String,
        client: async_nats::Client,
    },
}

pub struct LogDispatcher {
    tx: mpsc::Sender<WasmLogRecord>,
}

impl LogDispatcher {
    pub fn start(sinks: Vec<LogSink>) -> Self {
        let (tx, mut rx) = mpsc::channel::<WasmLogRecord>(4096);
        let sinks = Arc::new(sinks);

        tokio::spawn(async move {
            // Batch records for efficiency
            let mut batch: Vec<WasmLogRecord> = Vec::with_capacity(100);
            let mut flush_interval = tokio::time::interval(std::time::Duration::from_millis(500));

            loop {
                tokio::select! {
                    Some(record) = rx.recv() => {
                        batch.push(record);
                        if batch.len() >= 100 {
                            flush_batch(&sinks, &mut batch).await;
                        }
                    }
                    _ = flush_interval.tick() => {
                        if !batch.is_empty() {
                            flush_batch(&sinks, &mut batch).await;
                        }
                    }
                }
            }
        });

        LogDispatcher { tx }
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
                    if let Ok(line) = serde_json::to_string(record) {
                        tracing::info!(target: "wasm_log", "{}", line);
                    }
                }
            }
            LogSink::Http { endpoint, client } => {
                if let Ok(payload) = serde_json::to_vec(batch) {
                    match client
                        .post(endpoint)
                        .body(payload)
                        .header("content-type", "application/json")
                        .send()
                        .await
                    {
                        Ok(resp) => {
                            let status = resp.status();
                            if !status.is_success() {
                                tracing::warn!(
                                    status = %status,
                                    "log HTTP export returned non-success status"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "log HTTP export failed");
                        }
                    }
                }
            }
            LogSink::Nats { subject, client } => {
                for record in batch.iter() {
                    if let Ok(payload) = serde_json::to_vec(record) {
                        let _ = client.publish(subject.clone(), payload.into()).await;
                    }
                }
            }
        }
    }
    batch.clear();
}
