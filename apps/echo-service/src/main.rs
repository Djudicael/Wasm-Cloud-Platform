// echo-service: a simple HTTP service that echoes a message
// Used for testing East-West traffic between apps

#[cfg(target_family = "wasm")]
fn main() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{}:{}", bind_addr, port);
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

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        let path = request_line.split_whitespace().nth(1).unwrap_or("/");

        let (status, body) = route(path);

        let response = format!(
            "HTTP/1.1 {} OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body,
        );

        let _ = stream.write_all(response.as_bytes());
    }
}

#[cfg(target_family = "wasm")]
fn route(path: &str) -> (u16, String) {
    match path {
        "/" => (200, "Echo service running".to_string()),
        "/echo" => (200, "Echo from echo-service!".to_string()),
        "/health" => (200, r#"{"status":"healthy"}"#.to_string()),
        "/info" => {
            let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
            (200, format!(r#"{{"port":{}}}"#, port))
        }
        _ => (404, "Not Found".to_string()),
    }
}

#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn main() {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    let app = Router::new()
        .route("/echo", get(|| async { "Echo from native!" }))
        .route("/health", get(|| async { r#"{"status":"healthy"}"# }));

    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr: SocketAddr = format!("{bind_addr}:{port}").parse().unwrap();
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("Echo service listening on {}", addr);

    axum::serve(listener, app).await.unwrap();
}
