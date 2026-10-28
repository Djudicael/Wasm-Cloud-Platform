mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "http-hello",
        generate_all,
    });
}

use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct HttpHelloComponent;

impl bindings::exports::wasi::http::incoming_handler::Guest for HttpHelloComponent {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request
            .path_with_query()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "/".to_string());
        let mut status = 200;
        let body = match path.split('?').next().unwrap_or("/") {
            "/probe/trigger-identity" => request
                .headers()
                .get("x-trigger-id")
                .first()
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .unwrap_or_else(|| "none".into()),
            route if route.starts_with("/_platform/triggers/") => {
                let header = |key: &str| {
                    request
                        .headers()
                        .get(key)
                        .first()
                        .map(|v| String::from_utf8_lossy(v).into_owned())
                        .unwrap_or_default()
                };
                let name = route.trim_start_matches("/_platform/triggers/");
                let scheduled = header("x-trigger-scheduled-at-ms");
                if header("x-trigger-name") != name
                    || scheduled.parse::<u64>().is_err()
                    || !header("x-trigger-id").ends_with(&format!(":{name}:{scheduled}"))
                    || header("x-trigger-node").is_empty()
                    || !header("x-user-id").is_empty()
                {
                    status = 422;
                }
                if let Ok(delay) = std::env::var("TRIGGER_PROBE_DELAY_MS") {
                    std::thread::sleep(std::time::Duration::from_millis(delay.parse().unwrap()));
                }
                if std::env::var_os("RESOURCE_PROBE_HTTP_TARGET").is_some() {
                    probe_outbound_http()
                } else {
                    "triggered".into()
                }
            }
            "/health" => "healthy\n".to_string(),
            "/probe/connections/hold" => probe_connection_pressure(&path),
            "/probe/fds/hold" => probe_fd_pressure(&path),
            "/probe/http/get" => probe_outbound_http(),
            "/probe/tcp/write" => probe_socket_write(false),
            "/probe/udp/write" => probe_socket_write(true),
            "/probe/tcp/read" => probe_socket_stall(false),
            "/probe/udp/read" => {
                let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("UDP probe bind");
                socket.recv_from(&mut [0]).expect("UDP probe read");
                "completed".into()
            }
            "/probe/tcp/stall-write" => probe_socket_stall(true),
            "/probe/tcp/connect" => {
                let target = std::env::var("RESOURCE_PROBE_TCP_TARGET").expect("probe target");
                let _stream = std::net::TcpStream::connect(target).expect("probe connect");
                "connected".into()
            }
            "/probe/tcp/reconnect" => {
                let target = std::env::var("RESOURCE_PROBE_TCP_TARGET").expect("probe target");
                for _ in 0..3 {
                    drop(std::net::TcpStream::connect(&target).expect("probe reconnect"));
                }
                "connected:3".into()
            }
            _ => "Hello from wasi:http!\n".to_string(),
        };

        let headers = Fields::new();
        headers
            .append("content-type", b"text/plain")
            .expect("content-type header append should succeed");

        let response = OutgoingResponse::new(headers);
        response
            .set_status_code(status)
            .expect("status code should be valid");

        let outgoing_body = response.body().expect("response body should be available");
        ResponseOutparam::set(response_out, Ok(response));

        let stream = outgoing_body
            .write()
            .expect("response body writer should open");
        stream
            .blocking_write_and_flush(body.as_bytes())
            .expect("response body write should succeed");
        drop(stream);

        OutgoingBody::finish(outgoing_body, None).expect("response body finish should succeed");
    }
}

fn probe_socket_stall(write: bool) -> String {
    use std::io::{Read, Write};
    let target = std::env::var("RESOURCE_PROBE_TCP_TARGET").expect("probe target");
    let mut stream = std::net::TcpStream::connect(target).expect("probe connect");
    if write {
        stream
            .write_all(&vec![b'x'; 8 * 1024 * 1024])
            .expect("probe write");
    } else {
        stream.read_exact(&mut [0]).expect("probe read");
    }
    "completed".into()
}

fn probe_socket_write(udp: bool) -> String {
    use std::io::Write;
    let Ok(target) = std::env::var("RESOURCE_PROBE_TCP_TARGET") else {
        return "socket_probe_not_configured".into();
    };
    let payload = b"test";
    let mut written = 0;
    if udp {
        let socket = match std::net::UdpSocket::bind("127.0.0.1:0") {
            Ok(socket) => socket,
            Err(error) => return format!("setup_error:{error}"),
        };
        for _ in 0..3 {
            match socket.send_to(payload, &target) {
                Ok(count) => written += count,
                Err(_) => return format!("written:{written};denied:true"),
            }
        }
    } else {
        let mut stream = match std::net::TcpStream::connect(&target) {
            Ok(stream) => stream,
            Err(error) => return format!("setup_error:{error}"),
        };
        for _ in 0..3 {
            match stream.write_all(payload) {
                Ok(()) => written += payload.len(),
                Err(_) => return format!("written:{written};denied:true"),
            }
        }
    }
    format!("written:{written};denied:false")
}

fn probe_outbound_http() -> String {
    use bindings::wasi::http::{
        outgoing_handler,
        types::{Method, OutgoingRequest, Scheme},
    };
    let Ok(target) = std::env::var("RESOURCE_PROBE_HTTP_TARGET") else {
        return "http_probe_not_configured".into();
    };
    let (scheme, target) = if let Some(target) = target.strip_prefix("http://") {
        (Scheme::Http, target)
    } else if let Some(target) = target.strip_prefix("https://") {
        (Scheme::Https, target)
    } else {
        return "http_probe_invalid_target".into();
    };
    let (authority, path) = target.split_once('/').unwrap_or((target, ""));
    let request = OutgoingRequest::new(Fields::new());
    request.set_method(&Method::Get).unwrap();
    request.set_scheme(Some(&scheme)).unwrap();
    request.set_authority(Some(authority)).unwrap();
    request
        .set_path_with_query(Some(&format!("/{path}")))
        .unwrap();
    let future = match outgoing_handler::handle(request, None) {
        Ok(future) => future,
        Err(error) => return format!("denied:{error:?}"),
    };
    future.subscribe().block();
    match future.get() {
        Some(Ok(Ok(response))) => format!("status:{}", response.status()),
        Some(Ok(Err(error))) => format!("denied:{error:?}"),
        _ => "http_probe_failed".into(),
    }
}

fn query_u32(path: &str, name: &str, default: u32) -> u32 {
    path.split_once('?')
        .map(|(_, query)| query)
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == name).then(|| value.parse::<u32>().ok()).flatten()
            })
        })
        .unwrap_or(default)
}

fn probe_connection_pressure(path: &str) -> String {
    let count = query_u32(path, "count", 64).clamp(1, 4096);
    let hold_ms = query_u32(path, "hold_ms", 3_000).clamp(100, 120_000);
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

    let opened = streams.len();
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    drop(streams);
    let error = open_error
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"operation\":\"connection_hold\",\"requested\":{count},\"opened\":{opened},\"open_error\":{error}}}"
    )
}

fn probe_fd_pressure(path: &str) -> String {
    let count = query_u32(path, "count", 128).clamp(1, 4096);
    let hold_ms = query_u32(path, "hold_ms", 3_000).clamp(100, 120_000);
    let file_path = "/tmp/resource-http-fd-probe";
    if let Err(error) = std::fs::write(file_path, b"fd pressure probe") {
        return format!("{{\"operation\":\"fd_hold\",\"setup_error\":\"{error}\"}}");
    }

    let mut files = Vec::new();
    let mut open_error = None;
    for _ in 0..count {
        match std::fs::File::open(file_path) {
            Ok(file) => files.push(file),
            Err(error) => {
                open_error = Some(error.to_string());
                break;
            }
        }
    }

    let opened = files.len();
    std::thread::sleep(std::time::Duration::from_millis(hold_ms.into()));
    drop(files);
    let _ = std::fs::remove_file(file_path);
    let error = open_error
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"operation\":\"fd_hold\",\"requested\":{count},\"opened\":{opened},\"open_error\":{error}}}"
    )
}

bindings::export!(HttpHelloComponent with_types_in bindings);
