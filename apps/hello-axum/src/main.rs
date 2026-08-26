// hello-axum: a minimal HTTP server that works both natively (axum+tokio)
// and as a wasm32-wasip2 component (std::net, wasi:cli/run).

// ── WASM target: synchronous HTTP/1.1 server over std::net ──────────────
#[cfg(target_family = "wasm")]
fn main() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = format!("{}:{}", bind_addr, port);
    let listener = TcpListener::bind(&addr).expect("failed to bind");
    let mut shutdown_requested = false;

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

        let (status, content_type, body, should_shutdown) = route(full_path);

        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            reason(status),
            content_type,
            body.len(),
            body,
        );

        let _ = stream.write_all(response.as_bytes());
        if should_shutdown {
            shutdown_requested = true;
        }
        if shutdown_requested {
            break;
        }
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
fn route(path: &str) -> (u16, &'static str, String, bool) {
    // Strip query string for exact matches
    let base_path = path.split('?').next().unwrap_or(path);

    match base_path {
        "/" => (200, "text/plain", "Hello from wasip2!".to_string(), false),
        "/app-health" => (
            200,
            "application/json",
            r#"{"status":"healthy"}"#.to_string(),
            false,
        ),
        "/health" => (
            200,
            "application/json",
            r#"{"status":"healthy"}"#.to_string(),
            false,
        ),
        "/_platform/shutdown" => (
            200,
            "application/json",
            r#"{"status":"shutting_down"}"#.to_string(),
            true,
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
                Ok(response) => (200, "text/plain", response, false),
                Err(e) => (
                    500,
                    "text/plain",
                    format!("Failed to call echo-service: {}", e),
                    false,
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
                    false,
                ),
                Err(_) => (
                    200,
                    "application/json",
                    r#"{"echo_service_url":null}"#.to_string(),
                    false,
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
                        false,
                    )
                }
            };

            match std::net::TcpStream::connect(format!("{}:{}", host, port)) {
                Ok(_) => (
                    200,
                    "application/json",
                    format!(r#"{{"connected":true,"target":"{}:{}"}}"#, host, port),
                    false,
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
                    false,
                ),
            }
        }
        "/probe/file" => match probe_file_io(1) {
            Ok(bytes) => (
                200,
                "application/json",
                format!(r#"{{"operation":"file","bytes":{bytes}}}"#),
                false,
            ),
            Err(error) => (500, "text/plain", error, false),
        },
        "/probe/disk" => {
            let mib = query_u32(path, "mib", 32).clamp(1, 256);
            match probe_file_io(mib) {
                Ok(bytes) => (
                    200,
                    "application/json",
                    format!(r#"{{"operation":"disk","bytes":{bytes}}}"#),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/cache" => {
            let max_mib = std::env::var("EBPF_PROBE_MAX_CACHE_MIB")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(256)
                .clamp(1, 1024);
            let mib = query_u32(path, "mib", 128).clamp(1, max_mib);
            match populate_page_cache(mib) {
                Ok(bytes) => (
                    200,
                    "application/json",
                    format!(r#"{{"operation":"cache","bytes":{bytes}}}"#),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/cache/clear" => match std::fs::remove_file("/tmp/ebpf-page-cache.bin") {
            Ok(()) => (
                200,
                "application/json",
                r#"{"operation":"cache_clear","removed":true}"#.to_string(),
                false,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                200,
                "application/json",
                r#"{"operation":"cache_clear","removed":false}"#.to_string(),
                false,
            ),
            Err(error) => (500, "text/plain", error.to_string(), false),
        },
        "/probe/memory" => {
            // Keep the ordinary test application bounded. A production-
            // validation deployment may explicitly raise this ceiling while
            // also granting a matching per-instance memory limit.
            let max_mib = std::env::var("EBPF_PROBE_MAX_MEMORY_MIB")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(512)
                .clamp(1, 1200);
            let mib = query_u32(path, "mib", 64).clamp(1, max_mib);
            let bytes = mib as usize * 1024 * 1024;
            let mut allocation = vec![0_u8; bytes];
            for offset in (0..bytes).step_by(4096) {
                allocation[offset] = (offset / 4096) as u8;
            }
            let checksum = allocation
                .iter()
                .step_by(4096)
                .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)));
            (
                200,
                "application/json",
                format!(r#"{{"operation":"memory","bytes":{bytes},"checksum":{checksum}}}"#),
                false,
            )
        }
        "/probe/syscalls" => {
            let iterations = query_u32(path, "iterations", 128).clamp(1, 4096);
            let mut completed = 0_u32;
            for _ in 0..iterations {
                if std::fs::metadata("/tmp").is_ok() {
                    completed += 1;
                }
            }
            (
                200,
                "application/json",
                format!(
                    r#"{{"operation":"syscalls","iterations":{iterations},"completed":{completed}}}"#
                ),
                false,
            )
        }
        _ => (404, "text/plain", "Not Found".to_string(), false),
    }
}

#[cfg(target_family = "wasm")]
fn query_u32(path: &str, key: &str, default: u32) -> u32 {
    extract_query_param(path, key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(target_family = "wasm")]
fn probe_file_io(mib: u32) -> Result<u64, String> {
    use std::io::{Read, Write};

    // `std::process::id()` is not available on all WASI implementations.
    // The validation workload has one serial request loop, so a fixed scratch
    // name is deterministic and cannot race with another request.
    let path = "/tmp/ebpf-probe.bin";
    let bytes = mib as usize * 1024 * 1024;
    let chunk = vec![0xA5_u8; 64 * 1024];
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut written = 0_usize;
    while written < bytes {
        let count = (bytes - written).min(chunk.len());
        file.write_all(&chunk[..count])
            .map_err(|error| error.to_string())?;
        written += count;
    }
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);

    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut read_buffer = vec![0_u8; 64 * 1024];
    let mut read = 0_u64;
    loop {
        let count = file
            .read(&mut read_buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        read += count as u64;
    }
    drop(file);
    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok(read)
}

#[cfg(target_family = "wasm")]
fn populate_page_cache(mib: u32) -> Result<u64, String> {
    use std::io::{Read, Write};

    let path = "/tmp/ebpf-page-cache.bin";
    let bytes = mib as usize * 1024 * 1024;
    let chunk = vec![0x5A_u8; 64 * 1024];
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut written = 0_usize;
    while written < bytes {
        let count = (bytes - written).min(chunk.len());
        file.write_all(&chunk[..count])
            .map_err(|error| error.to_string())?;
        written += count;
    }
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);

    // Read the complete file so the pages are hot and reclaimable. Unlike the
    // ordinary file probe, retain the file until `/probe/cache/clear`.
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut read = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        read += count as u64;
    }
    Ok(read)
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
        .route("/app-health", get(|| async { r#"{"status":"healthy"}"# }))
        .route("/health", get(|| async { r#"{"status":"healthy"}"# }));

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr: SocketAddr = format!("{bind_addr}:{port}").parse().unwrap();
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("Listening on {}", addr);

    axum::serve(listener, app).await.unwrap();
}
