// crates/runtime/src/custom_pipe.rs
use bytes::Bytes;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use wasmtime_wasi::{HostOutputStream, StdoutStream, StreamError, Subscribe};

/// A custom pipe that implements StdoutStream and forwards all writes to a channel.
pub struct ChannelPipe {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl ChannelPipe {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            ChannelPipe {
                tx,
                buffer: Arc::new(Mutex::new(Vec::new())),
            },
            rx,
        )
    }
}

impl StdoutStream for ChannelPipe {
    fn stream(&self) -> Box<dyn HostOutputStream> {
        Box::new(ChannelOutputStream {
            tx: self.tx.clone(),
            buffer: self.buffer.clone(),
        })
    }

    fn isatty(&self) -> bool {
        false
    }
}

struct ChannelOutputStream {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl HostOutputStream for ChannelOutputStream {
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.extend_from_slice(&bytes);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        if let Ok(mut buf) = self.buffer.lock() {
            if !buf.is_empty() {
                let data = std::mem::take(&mut *buf);
                // Ignore send errors (receiver might be dropped)
                let _ = self.tx.send(data);
            }
        }
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        // Always ready to write
        Ok(usize::MAX)
    }
}

#[async_trait::async_trait]
impl Subscribe for ChannelOutputStream {
    async fn ready(&mut self) {}
}

impl Drop for ChannelOutputStream {
    fn drop(&mut self) {
        // Flush any remaining data
        let _ = self.flush();
    }
}
