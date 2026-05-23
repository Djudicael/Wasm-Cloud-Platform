mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "wasi-grpc-echo",
        generate_all,
    });
}

mod pb {
    include!(concat!(env!("OUT_DIR"), "/echo.rs"));
}

use bindings::wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;
use pb::{EchoReply, EchoRequest};
use prost::Message;

const GRPC_ECHO_PATH: &str = "/echo.EchoService/Echo";

struct WasiGrpcEcho;

impl bindings::exports::wasi::http::incoming_handler::Guest for WasiGrpcEcho {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let path = request
            .path_with_query()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "/".to_string());

        match path.as_str() {
            "/" => write_plain_text_response(response_out, 200, "wasi grpc echo\n"),
            "/health" => write_plain_text_response(response_out, 200, "healthy\n"),
            GRPC_ECHO_PATH => handle_grpc_echo(request, response_out),
            _ => write_plain_text_response(response_out, 404, "not found\n"),
        }
        .unwrap_or_else(|err| panic!("wasi-grpc-echo request handling failed for path {path}: {err}"));
    }
}

fn method_name(request: &IncomingRequest) -> String {
    match request.method() {
        bindings::wasi::http::types::Method::Get => "GET".to_string(),
        bindings::wasi::http::types::Method::Post => "POST".to_string(),
        bindings::wasi::http::types::Method::Put => "PUT".to_string(),
        bindings::wasi::http::types::Method::Delete => "DELETE".to_string(),
        bindings::wasi::http::types::Method::Patch => "PATCH".to_string(),
        bindings::wasi::http::types::Method::Head => "HEAD".to_string(),
        bindings::wasi::http::types::Method::Options => "OPTIONS".to_string(),
        bindings::wasi::http::types::Method::Connect => "CONNECT".to_string(),
        bindings::wasi::http::types::Method::Trace => "TRACE".to_string(),
        bindings::wasi::http::types::Method::Other(name) => name,
    }
}

fn ensure_method(request: &IncomingRequest, expected: &str) -> Result<(), String> {
    let actual = method_name(request);
    if actual == expected {
        Ok(())
    } else {
        Err(format!("unexpected method {actual}, expected {expected}"))
    }
}

fn handle_grpc_echo(
    request: IncomingRequest,
    response_out: ResponseOutparam,
) -> Result<(), String> {
    ensure_method(&request, "POST")?;
    let request_headers = request.headers();
    let content_type_values = request_headers.get("content-type");
    let content_type = content_type_values
        .first()
        .and_then(|value| std::str::from_utf8(value).ok())
        .unwrap_or_default();
    if !content_type.starts_with("application/grpc") {
        return write_grpc_error(response_out, "unsupported content-type", "13");
    }

    let request_body = read_request_body(&request)?;
    let echo_request = decode_grpc_unary_message::<EchoRequest>(&request_body)?;
    let response_message = EchoReply {
        message: echo_request.message,
    };
    let response_payload = encode_grpc_unary_message(&response_message)?;

    let headers = Fields::new();
    headers
        .append("content-type", b"application/grpc")
        .map_err(|_| "failed to append grpc content-type header".to_string())?;
    headers
        .append("grpc-encoding", b"identity")
        .map_err(|_| "failed to append grpc-encoding header".to_string())?;

    let response = OutgoingResponse::new(headers);
    response
        .set_status_code(200)
        .map_err(|_| "invalid grpc status code".to_string())?;

    let outgoing_body = response
        .body()
        .map_err(|_| "response body should be available".to_string())?;
    ResponseOutparam::set(response_out, Ok(response));

    let stream = outgoing_body
        .write()
        .map_err(|_| "response body writer should open".to_string())?;
    stream
        .blocking_write_and_flush(&response_payload)
        .map_err(|err| format!("grpc response write failed: {err}"))?;
    drop(stream);

    let trailers = Fields::new();
    trailers
        .append("grpc-status", b"0")
        .map_err(|_| "failed to append grpc-status trailer".to_string())?;
    OutgoingBody::finish(outgoing_body, Some(trailers))
        .map_err(|err| format!("grpc response finalize failed: {err}"))?;
    Ok(())
}

fn write_grpc_error(
    response_out: ResponseOutparam,
    message: &str,
    status: &str,
) -> Result<(), String> {
    let headers = Fields::new();
    headers
        .append("content-type", b"application/grpc")
        .map_err(|_| "failed to append grpc content-type header".to_string())?;

    let response = OutgoingResponse::new(headers);
    response
        .set_status_code(200)
        .map_err(|_| "invalid grpc status code".to_string())?;

    let outgoing_body = response
        .body()
        .map_err(|_| "response body should be available".to_string())?;
    ResponseOutparam::set(response_out, Ok(response));

    let stream = outgoing_body
        .write()
        .map_err(|_| "response body writer should open".to_string())?;
    drop(stream);

    let trailers = Fields::new();
    trailers
        .append("grpc-status", status.as_bytes())
        .map_err(|_| "failed to append grpc-status trailer".to_string())?;
    trailers
        .append("grpc-message", message.as_bytes())
        .map_err(|_| "failed to append grpc-message trailer".to_string())?;
    OutgoingBody::finish(outgoing_body, Some(trailers))
        .map_err(|err| format!("grpc error finalize failed: {err}"))?;
    Ok(())
}

fn write_plain_text_response(
    response_out: ResponseOutparam,
    status: u16,
    body: &str,
) -> Result<(), String> {
    let headers = Fields::new();
    headers
        .append("content-type", b"text/plain")
        .map_err(|_| "failed to append content-type header".to_string())?;

    let response = OutgoingResponse::new(headers);
    response
        .set_status_code(status)
        .map_err(|_| "invalid status code".to_string())?;

    let outgoing_body = response
        .body()
        .map_err(|_| "response body should be available".to_string())?;
    ResponseOutparam::set(response_out, Ok(response));

    let stream = outgoing_body
        .write()
        .map_err(|_| "response body writer should open".to_string())?;
    stream
        .blocking_write_and_flush(body.as_bytes())
        .map_err(|err| format!("plain-text response write failed: {err}"))?;
    drop(stream);
    OutgoingBody::finish(outgoing_body, None)
        .map_err(|err| format!("plain-text response finalize failed: {err}"))?;
    Ok(())
}

fn read_request_body(request: &IncomingRequest) -> Result<Vec<u8>, String> {
    let incoming_body = request
        .consume()
        .map_err(|_| "request body already consumed".to_string())?;
    let stream = incoming_body
        .stream()
        .map_err(|_| "request body stream should open".to_string())?;

    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(8192) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    continue;
                }
                bytes.extend_from_slice(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(err) => return Err(format!("request body read failed: {err}")),
        }
    }
    drop(stream);
    let _ = IncomingBody::finish(incoming_body);
    Ok(bytes)
}

fn decode_grpc_unary_message<M>(bytes: &[u8]) -> Result<M, String>
where
    M: Message + Default,
{
    if bytes.len() < 5 {
        return Err("grpc request body too short".to_string());
    }
    if bytes[0] != 0 {
        return Err("compressed grpc requests are not supported".to_string());
    }
    let message_len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if bytes.len() != 5 + message_len {
        return Err("grpc request frame length mismatch".to_string());
    }
    M::decode(&bytes[5..]).map_err(|err| format!("protobuf decode failed: {err}"))
}

fn encode_grpc_unary_message<M>(message: &M) -> Result<Vec<u8>, String>
where
    M: Message,
{
    let mut payload = Vec::new();
    message
        .encode(&mut payload)
        .map_err(|err| format!("protobuf encode failed: {err}"))?;

    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(0);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);
    Ok(framed)
}

bindings::export!(WasiGrpcEcho with_types_in bindings);
