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
        let body = match path.split('?').next().unwrap_or("/") {
            "/health" => "healthy\n".to_string(),
            "/probe/connections/hold" => probe_connection_pressure(&path),
            "/probe/fds/hold" => probe_fd_pressure(&path),
            _ => "Hello from wasi:http!\n".to_string(),
        };

        let headers = Fields::new();
        headers
            .append("content-type", b"text/plain")
            .expect("content-type header append should succeed");

        let response = OutgoingResponse::new(headers);
        response
            .set_status_code(200)
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
