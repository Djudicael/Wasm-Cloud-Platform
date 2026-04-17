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
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");

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
fn route(path: &str) -> (u16, &'static str, String) {
    match path {
        "/" => (200, "text/plain", "Hello from wasip2!".to_string()),
        "/health" => (200, "application/json", r#"{"status":"healthy"}"#.to_string()),
        "/call-echo" => {
            // Make outbound HTTP call to echo-service
            let echo_host = std::env::var("ECHO_SERVICE_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
            let url = format!("{}/echo", echo_host);
            
            // Simple HTTP client using std::net
            let result = make_http_request(&url);
            
            match result {
                Ok(response) => (200, "text/plain", response),
                Err(e) => (500, "text/plain", format!("Failed to call echo-service: {}", e)),
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
    
    let mut stream = std::net::TcpStream::connect(format!("{}:{}", host, port))
        .map_err(|e| e.to_string())?;
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );
    
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|e| e.to_string())?;
    
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
