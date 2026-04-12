use axum::{routing::get, routing::post, Router};
use std::sync::Arc;
use tokio::sync::Notify;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Create a shutdown notifier shared between the shutdown endpoint and the server
    let shutdown = Arc::new(Notify::new());
    let shutdown_for_endpoint = shutdown.clone();
    let shutdown_for_signal = shutdown.clone();

    let app = Router::new()
        .route("/", get(|| async { "Hello from Wasm!" }))
        // Platform shutdown endpoint - called by the Supervisor for graceful shutdown
        .route(
            "/_platform/shutdown",
            post(move || {
                let s = shutdown_for_endpoint.clone();
                async move {
                    println!("Graceful shutdown requested via /_platform/shutdown");
                    s.notify_one();
                    "shutting down gracefully"
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();

    println!("Hello-axum server listening on 0.0.0.0:8080");

    // Run Axum with graceful shutdown support
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_for_signal.notified().await;
            println!("Shutdown signal received - draining connections");
        })
        .await
        .unwrap();

    println!("Server shut down cleanly");
}
