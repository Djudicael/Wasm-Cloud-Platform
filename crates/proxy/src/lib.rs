pub mod admin;
pub mod auth_middleware;
pub mod backpressure;
pub mod config;
pub mod dns_webhook;
pub mod health;
pub mod metrics;
pub mod node_table;
pub mod rate_limiter;
pub mod router;
pub mod service;
pub mod tls;
pub mod upstream;

use config::ProxyTimeouts;
use pingora::server::Server;
use pingora_proxy::http_proxy_service;
use service::WasmProxy;

pub struct ProxyServer {
    pub server: Server,
}

impl ProxyServer {
    /// Build the proxy server.
    ///
    /// `timeouts` defines the intended slowloris / keepalive / header-read
    /// timeouts.  Pingora does not expose all of these through its public
    /// `ServerConf` API, so we log the intended values here and apply what
    /// we can at the request-filter level (see `service.rs`).  Pingora's
    /// own internal defaults are used for the transport layer.
    pub fn build(
        proxy: WasmProxy,
        http_port: u16,
        https_port: Option<u16>,
        tls: Option<(String, String)>,
        timeouts: ProxyTimeouts,
    ) -> Self {
        tracing::info!(
            header_read_timeout_secs = timeouts.request_header_read_timeout.as_secs(),
            body_read_timeout_secs = timeouts.request_body_read_timeout.as_secs(),
            keepalive_idle_timeout_secs = timeouts.keepalive_idle_timeout.as_secs(),
            max_header_size = timeouts.max_header_size,
            max_connections_per_ip = timeouts.max_connections_per_ip,
            "proxy timeouts configured (Pingora uses its internal defaults for transport-level timeouts)"
        );

        let mut server = Server::new(None).expect("Pingora server init failed");
        server.bootstrap();

        let mut svc = http_proxy_service(&server.configuration, proxy);

        // HTTP listener
        svc.add_tcp(&format!("0.0.0.0:{http_port}"));

        // HTTPS listener (optional)
        if let (Some(port), Some((cert, key))) = (https_port, tls) {
            svc.add_tls(&format!("0.0.0.0:{port}"), &cert, &key)
                .expect("Failed to add TLS listener");
        }

        server.add_service(svc);
        ProxyServer { server }
    }

    /// Run the Pingora server (blocks the current thread).
    pub fn run(self) -> ! {
        self.server.run_forever()
    }
}
