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

        // Preserve only the bearer credential needed for delegated east-west
        // authorization. Platform identity headers are intentionally ignored.
        let mut authorization = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => {
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_string());
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // Parse method and full path (including query string)
        let full_path = request_line.split_whitespace().nth(1).unwrap_or("/");

        let (status, content_type, body, should_shutdown) =
            route(full_path, authorization.as_deref());

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
fn route(path: &str, authorization: Option<&str>) -> (u16, &'static str, String, bool) {
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
            let internal_host = std::env::var("ECHO_SERVICE_HOST").ok();

            eprintln!(
                "DEBUG: ECHO_SERVICE_SERVICE_URL env var = {:?}",
                std::env::var("ECHO_SERVICE_SERVICE_URL")
            );
            eprintln!("DEBUG: Calling echo-service at: {}", url);

            let result = make_http_request(&url, authorization, internal_host.as_deref());

            match result {
                Ok((status, response)) => (status, "text/plain", response, false),
                Err(e) => (
                    500,
                    "text/plain",
                    format!("Failed to call echo-service: {}", e),
                    false,
                ),
            }
        }
        "/call-echo-info" => {
            let echo_host = std::env::var("ECHO_SERVICE_SERVICE_URL")
                .unwrap_or_else(|_| "http://echo-service.internal:9080".to_string());
            let url = format!("{}/info", echo_host);
            let internal_host = std::env::var("ECHO_SERVICE_HOST").ok();
            match make_http_request(&url, authorization, internal_host.as_deref()) {
                Ok((status, response)) => (status, "text/plain", response, false),
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
        "/probe/disk/hold" => {
            let max_mib = std::env::var("RESOURCE_PROBE_MAX_DISK_MIB")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(1024)
                .clamp(1, 1536);
            let mib = query_u32(path, "mib", 768).clamp(1, max_mib);
            let hold_ms = query_u32(path, "hold_ms", 30_000).clamp(100, 120_000);
            match probe_disk_pressure(mib, hold_ms) {
                Ok((bytes, write_error)) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"operation":"disk_hold","requested_mib":{mib},"bytes":{bytes},"write_error":{}}}"#,
                        json_optional_string(write_error.as_deref())
                    ),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/inodes/hold" => {
            let max_count = std::env::var("RESOURCE_PROBE_MAX_INODES")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(125_000)
                .clamp(1, 150_000);
            let count = query_u32(path, "count", 110_000).clamp(1, max_count);
            let hold_ms = query_u32(path, "hold_ms", 30_000).clamp(100, 120_000);
            match probe_inode_pressure(count, hold_ms) {
                Ok((created, create_error)) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"operation":"inode_hold","requested":{count},"created":{created},"create_error":{}}}"#,
                        json_optional_string(create_error.as_deref())
                    ),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/fds/hold" => {
            let count = query_u32(path, "count", 128).clamp(1, 4096);
            let hold_ms = query_u32(path, "hold_ms", 10_000).clamp(100, 120_000);
            match probe_fd_pressure(count, hold_ms) {
                Ok((opened, open_error)) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"operation":"fd_hold","requested":{count},"opened":{opened},"open_error":{}}}"#,
                        json_optional_string(open_error.as_deref())
                    ),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/connections/hold" => {
            let count = query_u32(path, "count", 64).clamp(1, 4096);
            let hold_ms = query_u32(path, "hold_ms", 10_000).clamp(100, 120_000);
            match probe_connection_pressure(count, hold_ms) {
                Ok((opened, open_error)) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"operation":"connection_hold","requested":{count},"opened":{opened},"open_error":{}}}"#,
                        json_optional_string(open_error.as_deref())
                    ),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
        }
        "/probe/cpu" => {
            let millis = query_u32(path, "millis", 1000).clamp(1, 30_000);
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(millis.into());
            let mut iterations = 0_u64;
            while std::time::Instant::now() < deadline {
                iterations = iterations.wrapping_add(1);
                std::hint::black_box(iterations.rotate_left(13));
            }
            (
                200,
                "application/json",
                format!(r#"{{"operation":"cpu","millis":{millis},"iterations":{iterations}}}"#),
                false,
            )
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
            let hold_ms = query_u32(path, "hold_ms", 0).clamp(0, 120_000);
            let mut allocation = vec![0_u8; bytes];
            for offset in (0..bytes).step_by(4096) {
                allocation[offset] = (offset / 4096) as u8;
            }
            let checksum = allocation
                .iter()
                .step_by(4096)
                .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)));
            if hold_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
            }
            (
                200,
                "application/json",
                format!(
                    r#"{{"operation":"memory","bytes":{bytes},"checksum":{checksum},"hold_ms":{hold_ms}}}"#
                ),
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
        "/probe/event-burst" => {
            let iterations = query_u32(path, "iterations", 10_000).clamp(1, 1_000_000);
            match probe_fd_event_burst(iterations) {
                Ok(completed) => (
                    200,
                    "application/json",
                    format!(
                        r#"{{"operation":"event-burst","iterations":{iterations},"completed":{completed}}}"#
                    ),
                    false,
                ),
                Err(error) => (500, "text/plain", error, false),
            }
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
fn probe_fd_event_burst(iterations: u32) -> Result<u32, String> {
    use std::io::Write;

    let path = "/tmp/ebpf-event-burst.bin";
    let mut seed = std::fs::File::create(path).map_err(|error| error.to_string())?;
    seed.write_all(b"ring-pressure")
        .map_err(|error| error.to_string())?;
    drop(seed);

    let mut completed = 0_u32;
    for _ in 0..iterations {
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        drop(file);
        completed += 1;
    }

    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok(completed)
}

#[cfg(target_family = "wasm")]
fn json_optional_string(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_string(),
        |text| format!(r#""{}""#, text.replace('\\', "\\\\").replace('"', "\\\"")),
    )
}

#[cfg(target_family = "wasm")]
fn probe_disk_pressure(mib: u32, hold_ms: u32) -> Result<(u64, Option<String>), String> {
    use std::io::Write;

    let path = "/tmp/resource-disk-pressure.bin";
    let requested = u64::from(mib) * 1024 * 1024;
    let chunk = vec![0xD5_u8; 64 * 1024];
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut written = 0_u64;
    let mut write_error = None;
    while written < requested {
        let count = (requested - written).min(chunk.len() as u64) as usize;
        match file.write_all(&chunk[..count]) {
            Ok(()) => written += count as u64,
            Err(error) => {
                write_error = Some(error.to_string());
                break;
            }
        }
    }
    let _ = file.sync_all();
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    drop(file);
    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok((written, write_error))
}

#[cfg(target_family = "wasm")]
fn probe_inode_pressure(count: u32, hold_ms: u32) -> Result<(u32, Option<String>), String> {
    let root = "/tmp/resource-inode-pressure";
    match std::fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    std::fs::create_dir(root).map_err(|error| error.to_string())?;
    let mut created = 0_u32;
    let mut create_error = None;
    for index in 0..count {
        let path = format!("{root}/{index:06x}");
        match std::fs::File::create(path) {
            Ok(file) => {
                drop(file);
                created += 1;
            }
            Err(error) => {
                create_error = Some(error.to_string());
                break;
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    std::fs::remove_dir_all(root).map_err(|error| error.to_string())?;
    Ok((created, create_error))
}

#[cfg(target_family = "wasm")]
fn probe_fd_pressure(count: u32, hold_ms: u32) -> Result<(u32, Option<String>), String> {
    use std::io::Write;

    let path = "/tmp/resource-fd-pressure.bin";
    let mut seed = std::fs::File::create(path).map_err(|error| error.to_string())?;
    seed.write_all(b"fd-pressure")
        .map_err(|error| error.to_string())?;
    drop(seed);
    let mut files = Vec::new();
    let mut open_error = None;
    for _ in 0..count {
        match std::fs::File::open(path) {
            Ok(file) => files.push(file),
            Err(error) => {
                open_error = Some(error.to_string());
                break;
            }
        }
    }
    let opened = files.len() as u32;
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    drop(files);
    std::fs::remove_file(path).map_err(|error| error.to_string())?;
    Ok((opened, open_error))
}

#[cfg(target_family = "wasm")]
fn probe_connection_pressure(count: u32, hold_ms: u32) -> Result<(u32, Option<String>), String> {
    let target = std::env::var("RESOURCE_PROBE_TCP_TARGET")
        .unwrap_or_else(|_| "172.20.0.10:4222".to_string());
    let mut streams = Vec::new();
    let mut open_error = None;
    for _ in 0..count {
        match std::net::TcpStream::connect(&target) {
            Ok(stream) => streams.push(stream),
            Err(error) => {
                open_error = Some(error.to_string());
                break;
            }
        }
    }
    let opened = streams.len() as u32;
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    drop(streams);
    Ok((opened, open_error))
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
fn make_http_request(
    url: &str,
    authorization: Option<&str>,
    host_override: Option<&str>,
) -> Result<(u16, String), String> {
    use std::io::{Read, Write};

    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();
    let request_host = host_override.unwrap_or(host);
    if request_host.contains('\r') || request_host.contains('\n') {
        return Err("invalid internal host override".to_string());
    }

    let mut stream =
        std::net::TcpStream::connect(format!("{}:{}", host, port)).map_err(|e| e.to_string())?;

    // Per the "Blind App" principle, the app never injects identity headers.
    // The Host (wasmtime runtime) will transparently inject identity metadata
    // when wasmtime-wasi provides hooks for wrapping TCP output streams.
    // Port 9080 is admitted by the WASI socket policy so the internal gateway
    // can enforce kernel-derived workload identity and namespace policy.
    let authorization_header = match authorization {
        Some(value) if !value.contains('\r') && !value.contains('\n') => {
            format!("Authorization: {value}\r\n")
        }
        Some(_) => return Err("invalid authorization header".to_string()),
        None => String::new(),
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
        path, request_host, authorization_header
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;

    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "invalid upstream HTTP status".to_string())?;

    // Extract body from HTTP response (skip headers) and preserve its status.
    if let Some(body_start) = response.find("\r\n\r\n") {
        Ok((status, response[body_start + 4..].to_string()))
    } else {
        Ok((status, response))
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
