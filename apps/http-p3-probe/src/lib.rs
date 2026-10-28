//! Disposable P3 transport probe. Targets are operator-controlled environment.
#[cfg(target_arch = "wasm32")]
mod guest {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use wasip3::http::types::{ErrorCode, Request, Response};
    use wasip3::http_compat::{
        http_from_wasi_request, http_from_wasi_response, http_into_wasi_request,
        http_into_wasi_response,
    };
    wasip3::http::service::export!(Probe);
    struct Probe;
    impl wasip3::exports::http::handler::Guest for Probe {
        async fn handle(request: Request) -> Result<Response, ErrorCode> {
            let request = http_from_wasi_request(request)?;
            let text = match request.uri().path() {
                "/probe/tcp" => {
                    use wasip3::sockets::types::{
                        IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket,
                    };
                    let target: std::net::SocketAddrV4 = std::env::var("PROBE_TCP_TARGET")
                        .expect("TCP probe target")
                        .parse()
                        .unwrap();
                    let octets = target.ip().octets();
                    let mut count = 0;
                    for _ in 0..3 {
                        let socket = TcpSocket::create(IpAddressFamily::Ipv4).unwrap();
                        let address = IpSocketAddress::Ipv4(Ipv4SocketAddress {
                            address: (octets[0], octets[1], octets[2], octets[3]),
                            port: target.port(),
                        });
                        if socket.connect(address).await.is_err() {
                            break;
                        }
                        count += 1;
                        drop(socket);
                    }
                    format!("connected:{count}")
                }
                "/probe/http" => {
                    let target =
                        std::env::var("PROBE_HTTP_TARGET").expect("configured probe target");
                    let req = http::Request::get(target)
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                    match wasip3::http::client::send(http_into_wasi_request(req)?).await {
                        Ok(response) => {
                            let response = http_from_wasi_response(response)?;
                            let body = response.into_body().collect().await?;
                            String::from_utf8_lossy(&body.to_bytes()).into_owned()
                        }
                        Err(_) => "denied".into(),
                    }
                }
                "/probe/postgres" => {
                    let url = std::env::var("DATABASE_URL").expect("probe database");
                    let config =
                        wasi_pg_client::Config::from_uri(&url).expect("probe database URI");
                    match wasi_pg_client::Connection::connect(&config).await {
                        Ok(mut conn) => {
                            let rows = conn
                                .query("SELECT 42::int4")
                                .await
                                .expect("P3 database query");
                            let value: i32 = rows.iter().next().unwrap().get(0).unwrap();
                            conn.close().await.expect("P3 database close");
                            format!("postgres:{value}")
                        }
                        Err(_) => "denied".into(),
                    }
                }
                "/probe/trigger-identity" => request
                    .headers()
                    .get("x-trigger-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .into(),
                _ => "platform-p3-probe".into(),
            };
            let response = http::Response::new(Full::new(Bytes::from(text)));
            http_into_wasi_response(response)
        }
    }
}
