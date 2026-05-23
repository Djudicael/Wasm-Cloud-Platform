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
        let body = match path.as_str() {
            "/health" => b"healthy\n".as_slice(),
            _ => b"Hello from wasi:http!\n".as_slice(),
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

        let stream = outgoing_body.write().expect("response body writer should open");
        stream
            .blocking_write_and_flush(body)
            .expect("response body write should succeed");
        drop(stream);

        OutgoingBody::finish(outgoing_body, None).expect("response body finish should succeed");
    }
}

bindings::export!(HttpHelloComponent with_types_in bindings);
