// postgres-app: a sample application that talks to PostgreSQL over raw TCP.
// Both the native and wasm targets expose the same basic HTTP surface:
//   GET /         -> readiness banner
//   GET /health   -> static health JSON
//   GET /query    -> performs a minimal PostgreSQL wire-protocol roundtrip

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/postgres".to_string())
}

fn listen_host() -> String {
    std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn listen_port(default_port: &str) -> String {
    std::env::var("PORT").unwrap_or_else(|_| default_port.to_string())
}

fn execute_query_sync(database_url: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use url::Url;

    let parsed = Url::parse(database_url).map_err(|e| format!("parse: {e}"))?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(5432);
    let user = parsed.username();
    let password = parsed.password().unwrap_or("");
    let db = parsed
        .path_segments()
        .map(|mut s| s.next().unwrap_or("postgres"))
        .unwrap_or("postgres");

    let mut stream =
        TcpStream::connect(format!("{host}:{port}")).map_err(|e| format!("connect: {e}"))?;

    let mut packet = vec![0u8; 4];
    packet.extend_from_slice(b"\x00\x03\x00\x00");

    let params = vec![("user", user), ("database", db), ("password", password)];
    let mut param_data = Vec::new();
    for (key, value) in params {
        param_data.extend_from_slice(key.as_bytes());
        param_data.push(0);
        param_data.extend_from_slice(value.as_bytes());
        param_data.push(0);
    }
    param_data.push(0);

    let len = (4 + param_data.len()) as i32;
    packet[0..4].copy_from_slice(&len.to_be_bytes());
    packet.extend_from_slice(&param_data);

    stream.write_all(&packet).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut auth = [0u8; 8];
    stream.read_exact(&mut auth).map_err(|e| e.to_string())?;

    let query = "SELECT 1";
    let mut q = vec![b'Q'];
    let len: i32 = (1 + query.len() + 1).try_into().unwrap();
    q.extend_from_slice(&len.to_be_bytes());
    q.extend_from_slice(query.as_bytes());
    q.push(0);

    stream.write_all(&q).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut resp = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    if resp.first().map(|&b| b == b'E').unwrap_or(false) {
        return Err("error".to_string());
    }

    Ok(r#"["ok"]"#.to_string())
}

fn route_response(path: &str) -> (u16, &'static str, String) {
    match path {
        "/" => (200, "text/plain", "PostgreSQL client ready".to_string()),
        "/health" => (
            200,
            "application/json",
            r#"{"status":"healthy"}"#.to_string(),
        ),
        "/query" => match execute_query_sync(&database_url()) {
            Ok(body) => (200, "application/json", body),
            Err(error) => (
                500,
                "application/json",
                format!(r#"{{"error":"{}"}}"#, error.replace('"', "\\\"")),
            ),
        },
        _ => (404, "text/plain", "Not Found".to_string()),
    }
}

#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn root() -> String {
        route_response("/").2
    }

    async fn health() -> &'static str {
        r#"{"status":"healthy"}"#
    }

    async fn query_db() -> String {
        route_response("/query").2
    }

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/query", get(query_db));

    let addr: SocketAddr = format!("{}:{}", listen_host(), listen_port("8080"))
        .parse()
        .unwrap();
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("PostgreSQL app listening on {}", addr);

    let _ = axum::serve(listener, app).await;
    Ok(())
}

#[cfg(target_family = "wasm")]
fn main() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let addr = format!("{}:{}", listen_host(), listen_port("8080"));
    let listener = TcpListener::bind(&addr).expect("failed to bind");
    println!("PostgreSQL app listening on {}", addr);

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };

        let mut buf = [0u8; 1024];
        let bytes_read = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if bytes_read == 0 {
            continue;
        }

        let request = String::from_utf8_lossy(&buf[..bytes_read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");

        let (status, content_type, body) = route_response(path);
        let status_text = if status == 200 {
            "OK"
        } else if status == 404 {
            "Not Found"
        } else {
            "Internal Server Error"
        };

        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            status_text,
            content_type,
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}
