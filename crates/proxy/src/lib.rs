pub mod admin;
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

use pingora::server::Server;
use pingora_proxy::http_proxy_service;
use service::WasmProxy;

pub struct ProxyServer {
    pub server: Server,
}

impl ProxyServer {
    pub fn build(
        proxy: WasmProxy,
        http_port: u16,
        https_port: Option<u16>,
        tls: Option<(String, String)>,
    ) -> Self {
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
