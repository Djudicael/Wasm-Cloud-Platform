// crates/runtime/src/custom_pipe.rs
// Simplified pipe that inherits from host stdout/stderr

use tokio::sync::mpsc;

/// A simple channel pipe for stdout/stderr capture
pub struct ChannelPipe {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ChannelPipe {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ChannelPipe { tx }, rx)
    }
}

impl std::io::Write for ChannelPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
