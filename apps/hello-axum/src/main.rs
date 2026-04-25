// hello-axum: a minimal HTTP server that works both natively (axum+tokio)
// and as a wasm32-wasip2 component (std::net, wasi:cli/run).

// ── WASM target: synchronous HTTP/1.1 server over std::net ──────────────
#[cfg(target_family = "wasm")]
fn main() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).expect("failed to bind");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }

        // Drain remaining headers (read until empty line)
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        // Parse method and full path (including query string)
        let full_path = request_line.split_whitespace().nth(1).unwrap_or("/");

        let (status, content_type, body) = route(full_path);

        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            reason(status),
            content_type,
            body.len(),
            body,
        );

        let _ = stream.write_all(response.as_bytes());
    }
}

#[cfg(target_family = "wasm")]
fn extract_query_param(path: &str, key: &str) -> Option<String> {
    let query_part = path.split('?').nth(1)?;
    for pair in query_part.split('&') {
        let mut kv = pair.splitn(2, '=');
        let k = kv.next()?;
        let v = kv.next().unwrap_or("");
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(target_family = "wasm")]
fn route(path: &str) -> (u16, &'static str, String) {
    // Strip query string for exact matches
    let base_path = path.split('?').next().unwrap_or(path);

    match base_path {
        "/" => (200, "text/plain", "Hello from wasip2!".to_string()),
        "/health" => (
            200,
            "application/json",
            r#"{"status":"healthy"}"#.to_string(),
        ),
        "/call-echo" => {
            // Make outbound HTTP call to echo-service.
            // The Supervisor injects ECHO_SERVICE_SERVICE_URL only for apps
            // in the same namespace (service discovery isolation).
            let echo_host = std::env::var("ECHO_SERVICE_SERVICE_URL")
                .unwrap_or_else(|_| "http://echo-service.internal:9080".to_string());
            let url = format!("{}/echo", echo_host);

            eprintln!(
                "DEBUG: ECHO_SERVICE_SERVICE_URL env var = {:?}",
                std::env::var("ECHO_SERVICE_SERVICE_URL")
            );
            eprintln!("DEBUG: Calling echo-service at: {}", url);

            let result = make_http_request(&url);

            match result {
                Ok(response) => (200, "text/plain", response),
                Err(e) => (
                    500,
                    "text/plain",
                    format!("Failed to call echo-service: {}", e),
                ),
            }
        }
        "/discover" => {
            // Return whether ECHO_SERVICE_SERVICE_URL was injected by the Supervisor.
            // Cross-namespace tests use this to verify service discovery isolation:
            // apps only see services in their own namespace.
            match std::env::var("ECHO_SERVICE_SERVICE_URL") {
                Ok(url) => (
                    200,
                    "application/json",
                    format!(r#"{{"echo_service_url":"{}"}}"#, url),
                ),
                Err(_) => (
                    200,
                    "application/json",
                    r#"{"echo_service_url":null}"#.to_string(),
                ),
            }
        }
        "/call-raw" => {
            // Try to connect to a specific host:port directly, bypassing
            // service discovery. Used by e2e tests to verify that the
            // network interceptor (socket_addr_check) blocks cross-namespace
            // TCP connections to direct app ports (but NOT the gateway port).
            //
            // Query params: host (e.g. "127.0.0.1") and port (e.g. "10101")
            // Returns JSON: {"connected": true} or {"connected": false, "error": "..."}
            let host = extract_query_param(path, "host").unwrap_or_else(|| "127.0.0.1".to_string());
            let port_str = extract_query_param(path, "port").unwrap_or_else(|| "80".to_string());
            let port: u16 = match port_str.parse() {
                Ok(p) => p,
                Err(_) => {
                    return (
                        400,
                        "application/json",
                        format!(
                            r#"{{"connected":false,"error":"invalid port: {}"}}"#,
                            port_str
                        ),
                    )
                }
            };

            match std::net::TcpStream::connect(format!("{}:{}", host, port)) {
                Ok(_) => (
                    200,
                    "application/json",
                    format!(r#"{{"connected":true,"target":"{}:{}"}}"#, host, port),
                ),
                Err(e) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"connected":false,"target":"{}:{}","error":"{}"}}"#,
                        host,
                        port,
                        e.to_string().replace('"', "\\\"")
                    ),
                ),
            }
        }
        _ => (404, "text/plain", "Not Found".to_string()),
    }
}

#[cfg(target_family = "wasm")]
fn make_http_request(url: &str) -> Result<String, String> {
    use std::io::{Read, Write};

    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let mut stream =
        std::net::TcpStream::connect(format!("{}:{}", host, port)).map_err(|e| e.to_string())?;

    // Per the "Blind App" principle, the app never injects identity headers.
    // The Host (wasmtime runtime) will transparently inject identity metadata
    // when wasmtime-wasi provides hooks for wrapping TCP output streams.
    // For now, namespace isolation relies on service discovery filtering.
    // socket_addr_check blocks cross-namespace connections to direct app ports,
    // but the gateway port (9080) is open to all namespaces.
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;

    // Extract body from HTTP response (skip headers)
    if let Some(body_start) = response.find("\r\n\r\n") {
        Ok(response[body_start + 4..].to_string())
    } else {
        Ok(response)
    }
}

#[cfg(target_family = "wasm")]
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

// ── Native target: axum + tokio ─────────────────────────────────────────
#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn main() {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    let app = Router::new()
        .route("/", get(|| async { "Hello from native!" }))
        .route("/health", get(|| async { r#"{"status":"healthy"}"# }));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("Listening on {}", addr);

    axum::serve(listener, app).await.unwrap();
}
