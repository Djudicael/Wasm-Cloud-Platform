/// Configuration for the log forwarder.
#[derive(Debug, Clone)]
pub struct LogForwarderConfig {
    pub sinks: Vec<ForwarderSinkConfig>,
    pub buffer_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub enum ForwarderSinkConfig {
    Loki {
        endpoint: String,
        labels: Vec<(String, String)>,
    },
    Elasticsearch {
        endpoint: String,
        index_prefix: String,
    },
    Vector {
        endpoint: String,
    },
    Http {
        endpoint: String,
    },
    Nats {
        subject: String,
    },
}

impl LogForwarderConfig {
    pub fn flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.flush_interval_ms)
    }
}
