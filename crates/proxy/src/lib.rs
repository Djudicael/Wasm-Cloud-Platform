pub mod admin;
pub mod auth_middleware;
pub mod backpressure;
pub mod config;
pub mod dns_webhook;
pub mod gateway;
pub mod health;
pub mod health_events;
pub mod metrics;
pub mod node_table;
pub mod rate_limiter;
pub mod router;
pub mod service;
pub mod tls;
pub mod upstream;
pub mod upstream_health;

use config::ProxyTimeouts;
use pingora::apps::HttpServerOptions;
use pingora::listeners::tls::TlsSettings;
use pingora::server::Server;
use pingora_proxy::ProxyServiceBuilder;
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

        // Pingora cannot reliably peek for the cleartext HTTP/2 preface after a
        // TLS handshake. Keep h2c on the cleartext listener only and use ALPN
        // negotiation for HTTP/2 on the TLS listener.
        let mut http_options = HttpServerOptions::default();
        http_options.h2c = true;
        let mut http_service = ProxyServiceBuilder::new(&server.configuration, proxy.clone())
            .server_options(http_options)
            .build();

        // HTTP listener
        http_service.add_tcp(&format!("0.0.0.0:{http_port}"));
        server.add_service(http_service);

        // HTTPS listener (optional)
        if let (Some(port), Some((cert, key))) = (https_port, tls) {
            let mut tls_settings =
                TlsSettings::intermediate(&cert, &key).expect("Failed to load TLS settings");
            tls_settings.enable_h2();

            let mut tls_service = ProxyServiceBuilder::new(&server.configuration, proxy).build();
            tls_service.add_tls_with_settings(&format!("0.0.0.0:{port}"), None, tls_settings);
            server.add_service(tls_service);
        }
        ProxyServer { server }
    }

    /// Run the Pingora server (blocks the current thread).
    pub fn run(self) -> ! {
        self.server.run_forever()
    }
}
