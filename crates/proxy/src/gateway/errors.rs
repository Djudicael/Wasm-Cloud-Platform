use pingora_proxy::Session;

/// Send a gateway error response with a structured JSON body.
pub async fn send_gateway_error(
    session: &mut Session,
    status: u16,
    error_code: &str,
    message: &str,
) -> pingora_core::Result<bool> {
    let body = serde_json::json!({
        "error": error_code,
        "message": message,
        "status": status,
    });

    let mut resp = pingora::http::ResponseHeader::build(status, None).map_err(|e| {
        pingora_core::Error::because(
            pingora_core::ErrorType::InternalError,
            "gateway error response",
            e,
        )
    })?;
    let _ = resp.insert_header("Content-Type", "application/json");

    session.write_response_header(Box::new(resp), false).await?;
    session
        .write_response_body(Some(body.to_string().into()), true)
        .await?;
    Ok(true) // abort the request
}
