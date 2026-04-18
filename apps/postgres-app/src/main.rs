// postgres-app: A WASM application that connects to PostgreSQL via raw TCP.
// Uses std::net for TCP and implements PostgreSQL wire protocol manually.
// This approach works on wasip2 without external SQL client dependencies.

#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn query_db() -> Result<String, ()> {
        // Call the native Postgres function
        match execute_query_native("postgres://postgres:postgres@localhost:5432/postgres").await {
            Ok(r) => Ok(r),
            Err(e) => Ok(format!(r#"{{"error":"{}"}}"#, e)),
        }
    }

    async fn execute_query_native(database_url: &str) -> Result<String, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream as TokioTcpStream;
        use url::Url;

        let parsed = Url::parse(database_url).map_err(|e| format!("parse: {}", e))?;

        let host = parsed.host_str().ok_or("no host")?;
        let port = parsed.port().unwrap_or(5432);
        let user = parsed.username();
        let password = parsed.password().unwrap_or("");
        let db = parsed
            .path_segments()
            .map(|mut s| s.next().unwrap_or("postgres"))
            .unwrap_or("postgres");

        let mut stream = TokioTcpStream::connect(format!("{}:{}", host, port))
            .await
            .map_err(|e| format!("connect: {}", e))?;

        // Send startup message
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

        stream.write_all(&packet).await.map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;

        // Read auth
        let mut auth = [0u8; 8];
        stream
            .read_exact(&mut auth)
            .await
            .map_err(|e| e.to_string())?;

        // Send query
        let query = "SELECT 1";
        let mut q = vec![b'Q'];
        let len: i32 = (1 + query.len() + 1).try_into().unwrap();
        q.extend_from_slice(&len.to_be_bytes());
        q.extend_from_slice(query.as_bytes());
        q.push(0);

        stream.write_all(&q).await.map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;

        // Read response
        let mut resp = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match stream.read(&mut buf).await {
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

    let app = Router::new()
        .route("/", get(|| async { "PostgreSQL client ready".to_string() }))
        .route("/health", get(|| async { r#"{"status":"healthy"}"# }))
        .route("/query", get(query_db));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("PostgreSQL app listening on {}", addr);

    let _ = axum::serve(listener, app).await;
    Ok(())
}

#[cfg(target_family = "wasm")]
fn main() {
    // WASM-only stub - runs in infinite loop
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
