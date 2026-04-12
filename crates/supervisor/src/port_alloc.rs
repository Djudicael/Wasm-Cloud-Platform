use common::error::PlatformError;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Mutex;

/// Allocates TCP ports from a fixed range for Wasm instances.
pub struct PortAllocator {
    free: Mutex<BTreeSet<u16>>,
    bind_addr: std::net::IpAddr,
}

impl PortAllocator {
    /// Create allocator covering [start, end] on the given bind address.
    pub fn new(bind_addr: std::net::IpAddr, start: u16, end: u16) -> Self {
        let free = (start..=end).collect();
        PortAllocator {
            free: Mutex::new(free),
            bind_addr,
        }
    }

    /// Allocate the next available port. Returns Err if the pool is exhausted.
    pub fn allocate(&self) -> Result<u16, PlatformError> {
        let mut free = self.free.lock().unwrap();
        let port = free
            .iter()
            .next()
            .copied()
            .ok_or_else(|| PlatformError::Runtime("port pool exhausted".into()))?;
        free.remove(&port);
        tracing::debug!(port, "allocated port");
        Ok(port)
    }

    /// Return a port to the pool after an instance stops.
    pub fn release(&self, port: u16) {
        let mut free = self.free.lock().unwrap();
        free.insert(port);
        tracing::debug!(port, "released port");
    }

    /// Get the full SocketAddr for an allocated port.
    pub fn socket_addr(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.bind_addr, port)
    }
}
