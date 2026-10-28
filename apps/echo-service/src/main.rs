// echo-service: a simple HTTP service that echoes a message
// Used for testing East-West traffic between apps

#[cfg(target_family = "wasm")]
fn main() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    if let Ok(mode) = std::env::var("RESOURCE_PROBE_IO_MODE") {
        use std::io::Read;
        let target = std::env::var("RESOURCE_PROBE_TCP_TARGET").expect("probe target");
        if mode == "udp-read" {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("UDP probe bind");
            socket.recv_from(&mut [0]).expect("UDP probe read");
            return;
        }
        let mut stream = std::net::TcpStream::connect(target).expect("probe connect");
        match mode.as_str() {
            "connect" => {}
            "read" => {
                stream.read_exact(&mut [0]).expect("probe read");
            }
            "write" => {
                stream
                    .write_all(&vec![b'x'; 8 * 1024 * 1024])
                    .expect("probe write");
            }
            _ => panic!("unknown probe I/O mode"),
        }
        return;
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{}:{}", bind_addr, port);
    let listener = TcpListener::bind(&addr).expect("failed to bind");
    // Optional deterministic termination for runtime integration probes.
    let max_requests = std::env::var("RESOURCE_PROBE_MAX_REQUESTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);

    for (index, stream) in listener.incoming().enumerate() {
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
        if max_requests.is_some_and(|limit| index + 1 >= limit) {
            break;
        }
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
