//! Transparent metadata injection for WASI Preview 2 TCP streams.
//!
//! When a Wasm app makes an outbound HTTP connection, the host wraps the
//! `wasi:io/streams::output-stream` and injects an `X-Source-App` header
//! into the first HTTP request. This lets the internal gateway identify
//! the caller without the app knowing anything about platform headers.
//!
//! NOTE: This module is kept for future use. The "Blind App" principle
//! requires the Host (not the app) to inject identity metadata into
//! outbound requests. The ideal implementation would intercept the
//! `finish_connect` syscall in wasmtime-wasi and wrap the `OutputStream`
//! with `MetadataInjectingStream`. However, wasmtime-wasi v43 does not
//! expose a hook point for this — the TCP `OutputStream` is created
//! internally by `finish_connect()` with no way to wrap it without
//! implementing a custom `HostTcpSocket` (25+ methods of boilerplate).
//!
//! For now, namespace isolation relies on service discovery: the Supervisor
//! only injects `<APP>_SERVICE_URL` env vars for same-namespace apps. The
//! `socket_addr_check` blocks cross-namespace connections to direct app ports,
//! but the gateway port (9080) is open to all namespaces. When wasmtime-wasi
//! provides better stream customization hooks, this module can be wired in to
//! achieve true Host-level identity injection without the app's involvement.

use bytes::Bytes;
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError};

/// Type alias matching the wasmtime-wasi-io convention.
type StreamResult<T> = Result<T, StreamError>;

/// Wraps an output stream and injects `X-Source-App` on the first HTTP request.
pub struct MetadataInjectingStream {
    inner: Box<dyn OutputStream>,
    app_id: String,
    state: InjectState,
    buffer: Vec<u8>,
}

enum InjectState {
    /// We haven't seen any bytes yet — buffering until we find the end of headers.
    Buffering,
    /// Header has been injected, passthrough mode.
    Passthrough,
}

impl MetadataInjectingStream {
    pub fn new(inner: Box<dyn OutputStream>, app_id: String) -> Self {
        MetadataInjectingStream {
            inner,
            app_id,
            state: InjectState::Buffering,
            buffer: Vec::new(),
        }
    }

    /// Attempt to inject the header into a byte buffer containing the start
    /// of an HTTP request. Returns the modified bytes if injection succeeded.
    fn try_inject(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        // Look for the end of HTTP headers: \r\n\r\n
        let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;

        // Split into headers and body
        let headers = &bytes[..header_end + 2]; // include the first \r\n
        let rest = &bytes[header_end + 2..]; // \r\n + body

        // Build new buffer with injected header
        let mut out = Vec::with_capacity(bytes.len() + 64);
        out.extend_from_slice(headers);
        out.extend_from_slice(format!("X-Source-App: {}\r\n", self.app_id).as_bytes());
        out.extend_from_slice(rest);

        Some(out)
    }
}

// The Pollable trait is defined with #[async_trait] which transforms
// `async fn ready(&mut self)` into a method returning a pinned, boxed
// future with specific lifetime bounds. We implement the transformed
// signature directly to match what the trait expects.
impl Pollable for MetadataInjectingStream {
    fn ready<'life0, 'async_trait>(
        &'life0 mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + std::marker::Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.ready()
    }
}

impl OutputStream for MetadataInjectingStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        match self.state {
            InjectState::Passthrough => self.inner.write(bytes),
            InjectState::Buffering => {
                self.buffer.extend_from_slice(&bytes);

                // Try to find the end of HTTP headers
                if let Some(injected) = self.try_inject(&self.buffer) {
                    // Injection succeeded — switch to passthrough and flush
                    self.state = InjectState::Passthrough;
                    self.inner.write(Bytes::from(injected))?;
                    self.buffer.clear();
                    Ok(())
                } else {
                    // Not enough data yet — check if buffer is getting too large
                    if self.buffer.len() > 8192 {
                        // Probably not HTTP or headers are huge — give up and flush raw
                        self.state = InjectState::Passthrough;
                        let buf = std::mem::take(&mut self.buffer);
                        self.inner.write(Bytes::from(buf))?;
                        Ok(())
                    } else {
                        // Still buffering — report success (bytes accepted)
                        Ok(())
                    }
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        if let InjectState::Buffering = self.state {
            // Flush any buffered bytes without injection
            if !self.buffer.is_empty() {
                let buf = std::mem::take(&mut self.buffer);
                self.inner.write(Bytes::from(buf))?;
            }
            self.state = InjectState::Passthrough;
        }
        self.inner.flush()
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        self.inner.check_write()
    }
}
