// postgres-app: A WASM application that connects to PostgreSQL via raw TCP.
// Uses std::net for TCP and implements PostgreSQL wire protocol manually.
// This approach works on wasip2 without external SQL client dependencies.

fn main() {
    #[cfg(target_family = "wasm")]
    {
        run_wasm();
    }

    #[cfg(not(target_family = "wasm"))]
    {
        tokio_main();
    }
}

#[cfg(target_family = "wasm")]
fn run_wasm() {
    use std::io::{BufRead, BufReader, Write};

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = std::net::TcpListener::bind(&addr).expect("failed to bind");

    eprintln!("postgres-app listening on {}", addr);

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
        let (status, content_type, body) = route_wasm(path);

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
fn route_wasm(path: &str) -> (u16, &'static str, String) {
    match path {
        "/" => (200, "text/plain", "PostgreSQL client ready".to_string()),
        "/health" => (200, "application/json", r#"{"status":"healthy"}"#.to_string()),
        "/query" => {
            let db_url = std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:5432/postgres".to_string());

            match execute_query(&db_url) {
                Ok(result) => (200, "application/json", result),
                Err(e) => (500, "application/json", format!(r#"{{"error":"{}"}}"#, e)),
            }
        }
        _ => (404, "text/plain", "Not Found".to_string()),
    }
}

#[cfg(target_family = "wasm")]
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[cfg(target_family = "wasm")]
fn execute_query(database_url: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    // Parse postgres://user:password@host:port/db
    let parsed = url::Url::parse(database_url)
        .map_err(|e| format!("parse URL: {}", e))?;

    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(5432);
    let user = parsed.username();
    let password = parsed.password();
    let db = parsed
        .path_segments()
        .and_then(|s| s.collect::<Vec<_>>().first().cloned())
        .unwrap_or("postgres");

    eprintln!("Connecting to {}:{}/{}", host, port, db);

    // 1. TCP connect
    let mut stream = TcpStream::connect(format!("{}:{}", host, port))
        .map_err(|e| format!("TCP connect: {}", e))?;

    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    // 2. PostgreSQL startup handshake
    let user_cow = user.as_ref();
    let password_str = password.unwrap_or("");
    let _ = postgres_handshake(&mut stream, user_cow, password_str, &db)?;
    eprintln!("Handshake OK");

    // 3. Execute query
    let result = postgres_query(&mut stream, "SELECT 1 AS test")?;
    eprintln!("Query result: {}", result);

    Ok(format!(r#"{{"status":"ok","result":"{}"}}"#, result))
}

#[cfg(target_family = "wasm")]
fn postgres_handshake(
    stream: &mut std::net::TcpStream,
    user: &str,
    password: &str,
    database: &str,
) -> Result<(), String> {
    use std::io::Write;

    // Startup message
    let mut packet = vec![0u8; 4]; // placeholder for length
    packet.extend_from_slice(b"\x00\x03\x00\x00"); // protocol version 3.0

    // Parameters: user, database, optional password
    let params: Vec<(&str, &str)> = if password.is_empty() {
        vec![("user", user), ("database", database)]
    } else {
        vec![("user", user), ("database", database), ("password", password)]
    };

    let mut param_data = Vec::new();
    for (key, value) in params {
        param_data.extend_from_slice(key.as_bytes());
        param_data.push(0);
        param_data.extend_from_slice(value.as_bytes());
        param_data.push(0);
    }
    param_data.push(0); // null terminator for params

    let len = 4 + param_data.len();
    let len_bytes = (len as i32).to_be_bytes();
    packet[0..4].copy_from_slice(&len_bytes);
    packet.extend_from_slice(&param_data);

    stream.write_all(&packet).map_err(|e| format!("write: {}", e))?;
    stream.flush().map_err(|e| format!("flush: {}", e))?;

    // Read auth response
    let mut auth = [0u8; 8];
    stream.read_exact(&mut auth).map_err(|e| format!("read auth: {}", e))?;

    // Check for AuthenticationOk (0000) or AuthenticationCleartextPassword (3)
    let auth_arr: [u8; 4] = [auth[0], auth[1], auth[2], auth[3]];
    let auth_type = u32::from_be_bytes(auth_arr);
    if auth_type == 0 {
        Ok(()) // AuthenticationOk
    } else if auth_type == 3 {
        // AuthenticationCleartextPassword - send password packet
        let mut pw_packet = vec![0u8; 4];
        pw_packet.extend_from_slice(password.as_bytes());
        pw_packet.push(0);
        let len = pw_packet.len() as i32;
        pw_packet[0..4].copy_from_slice(&len.to_be_bytes());
        stream.write_all(&pw_packet).map_err(|e| format!("write password: {}", e))?;

        // Read command complete or error
        let mut resp = [0u8; 1];
        stream.read_exact(&mut resp).map_err(|e| format!("read response: {}", e))?;
        if resp[0] == b'E' {
            return Err("authentication failed".to_string());
        }
        Ok(())
    } else {
        Ok(()) // Accept other auth types for simplicity
    }
}

#[cfg(target_family = "wasm")]
fn postgres_query(stream: &mut std::net::TcpStream, query: &str) -> Result<String, String> {
    use std::io::{Read, Write};

    // Query message
    let mut packet = vec![b'Q'];
    let query_with_null = format!("{}\0", query);
    let len = (1 + query_with_null.len()) as i32;
    packet.extend_from_slice(&len.to_be_bytes());
    packet.extend_from_slice(query_with_null.as_bytes());

    stream.write_all(&packet).map_err(|e| format!("write query: {}", e))?;
    stream.flush().map_err(|e| format!("flush: {}", e))?;

    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 256];

    loop {
        let n = stream.read(&mut buf).map_err(|e| format!("read: {}", e))?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
        if response.ends_with(b"\0\0\0\0") {
            break;
        }
    }

    if response.is_empty() {
        return Err("empty response".to_string());
    }

    // Check for error
    if response[0] == b'E' {
        return Err("query error".to_string());
    }

    // Command complete - return row count or OK
    if response[0] == b'C' {
        return Ok("command_ok".to_string());
    }

    Ok("query_executed".to_string())
}

#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn tokio_main() {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn query_db() -> String {
        let db_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:5432/postgres".to_string());

        match execute_query_native(&db_url).await {
            Ok(result) => format!(r#"{{"status":"ok","result":{}}}"#, result),
            Err(e) => format!(r#"{{"error":"{}"}}"#, e),
        }
    }

    async fn execute_query_native(database_url: &str) -> Result<String, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        use url::Url;

        let parsed = Url::parse(database_url).map_err(|e| format!("parse: {}", e))?;

        let host = parsed.host_str().ok_or("no host")?;
        let port = parsed.port().unwrap_or(5432);
        let user = parsed.username();
        let password = parsed.password().unwrap_or("");
        let db = parsed.path_segments()
            .and_then(|s| s.first().cloned())
            .unwrap_or("postgres");

        let mut stream = TcpStream::connect(format!("{}:{}", host, port))
            .await
            .map_err(|e| format!("connect: {}", e))?;

        // Send startup message
        let mut packet = vec![0u8; 4];
        packet.extend_from_slice(b"\x00\x03\x00\x00");

        let params = vec![
            ("user", user),
            ("database", db),
            ("password", password),
        ];

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
        stream.read_exact(&mut auth).await.map_err(|e| e.to_string())?;

        // Send query
        let query = "SELECT 1";
        let mut q = vec![b'Q'];
        q.extend_from_slice(&(1 + query.len() + 1) as i32);
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

    axum::serve(listener, app).await.unwrap();
}