// postgres-app: a sample application that talks to PostgreSQL through
// `wasi-pg-client` on both native and Wasm targets.
// Both targets expose the same basic HTTP surface:
//   GET /         -> readiness banner
//   GET /health   -> static health JSON
//   GET /query    -> performs a simple PostgreSQL roundtrip

use wasi_pg_client::{Config, Connection};

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

async fn execute_query(database_url: &str) -> Result<String, String> {
    let config = Config::from_uri(database_url).map_err(|e| format!("parse: {e}"))?;
    let mut conn = Connection::connect(&config)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let result = conn
        .query("SELECT 1::INT4")
        .await
        .map_err(|e| format!("query: {e}"))?;

    let mut rows = Vec::new();
    for row in result.iter() {
        let value: i32 = row.get(0).map_err(|e| format!("decode: {e}"))?;
        rows.push(value);
    }

    conn.close().await.map_err(|e| format!("close: {e}"))?;
    serde_json::to_string(&rows).map_err(|e| format!("encode: {e}"))
}

fn route_response(path: &str) -> (u16, &'static str, String) {
    match path {
        "/" => (200, "text/plain", "PostgreSQL client ready".to_string()),
        "/health" => (
            200,
            "application/json",
            r#"{"status":"healthy"}"#.to_string(),
        ),
        _ => (404, "text/plain", "Not Found".to_string()),
    }
}

#[cfg(target_family = "wasm")]
fn run_query() -> Result<String, String> {
    wstd::runtime::block_on(execute_query(&database_url()))
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
        match execute_query(&database_url()).await {
            Ok(body) => body,
            Err(error) => format!(r#"{{"error":"{}"}}"#, error.replace('"', "\\\"")),
        }
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

        let (status, content_type, body) = if path == "/query" {
            match run_query() {
                Ok(body) => (200, "application/json", body),
                Err(error) => (
                    500,
                    "application/json",
                    format!(r#"{{"error":"{}"}}"#, error.replace('"', "\\\"")),
                ),
            }
        } else {
            route_response(path)
        };
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
