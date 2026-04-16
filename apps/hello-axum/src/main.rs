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

        // Read the request line (e.g. "GET /health HTTP/1.1")
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

        // Parse method and path
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/");

        let (status, content_type, body) = route(path);

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
fn route(path: &str) -> (u16, &'static str, &'static str) {
    match path {
        "/" => (200, "text/plain", "Hello from wasip2!"),
        "/health" => (200, "application/json", r#"{"status":"healthy"}"#),
        _ => (404, "text/plain", "Not Found"),
    }
}

#[cfg(target_family = "wasm")]
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
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
