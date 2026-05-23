#[cfg(target_family = "wasm")]
compile_error!(
    "grpc-echo currently cannot target wasm32-wasip2 with tonic transport: \
     the gRPC server stack depends on Tokio networking/HTTP2 server features \
     that are not available on wasm today."
);

#[cfg(target_family = "wasm")]
fn main() {}

#[cfg(not(target_family = "wasm"))]
use std::net::SocketAddr;
#[cfg(not(target_family = "wasm"))]
use tokio::net::TcpListener;
#[cfg(not(target_family = "wasm"))]
use tokio_stream::wrappers::TcpListenerStream;
#[cfg(not(target_family = "wasm"))]
use tonic::{transport::Server, Request, Response, Status};

#[cfg(not(target_family = "wasm"))]
pub mod pb {
    tonic::include_proto!("echo");
}

#[cfg(not(target_family = "wasm"))]
use pb::echo_service_server::{EchoService, EchoServiceServer};
#[cfg(not(target_family = "wasm"))]
use pb::{EchoReply, EchoRequest};

#[cfg(not(target_family = "wasm"))]
fn listen_host() -> String {
    std::env::var("BIND_ADDR")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

#[cfg(not(target_family = "wasm"))]
fn listen_port() -> String {
    std::env::var("PORT").unwrap_or_else(|_| "50051".to_string())
}

#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct EchoApi;

#[cfg(not(target_family = "wasm"))]
#[tonic::async_trait]
impl EchoService for EchoApi {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoReply>, Status> {
        Ok(Response::new(EchoReply {
            message: request.into_inner().message,
        }))
    }
}

#[cfg(not(target_family = "wasm"))]
async fn serve(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    Server::builder()
        .add_service(EchoServiceServer::new(EchoApi))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = format!("{}:{}", listen_host(), listen_port()).parse()?;
    let listener = TcpListener::bind(addr).await?;
    println!("gRPC echo service listening on {}", addr);
    serve(listener).await
}

#[cfg(not(target_family = "wasm"))]
#[cfg(test)]
mod tests {
    use super::*;
    use pb::echo_service_client::EchoServiceClient;

    #[tokio::test]
    async fn test_native_grpc_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            serve(listener).await.unwrap();
        });

        let endpoint = format!("http://{addr}");
        let mut client = EchoServiceClient::connect(endpoint).await.unwrap();
        let response = client
            .echo(Request::new(EchoRequest {
                message: "hello".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.message, "hello");

        server.abort();
    }
}
