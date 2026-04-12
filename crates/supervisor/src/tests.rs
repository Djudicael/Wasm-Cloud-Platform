use crate::instance::wait_for_ready;
use crate::network::LocalServiceRegistry;
use crate::port_alloc::PortAllocator;
use common::types::AppId;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::net::TcpListener;

#[test]
fn test_port_allocator_basic() {
    let addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let allocator = PortAllocator::new(addr, 10000, 10002); // 3 ports

    // allocate() returns a different port on each call
    let p1 = allocator.allocate().unwrap();
    let p2 = allocator.allocate().unwrap();
    let p3 = allocator.allocate().unwrap();

    assert_ne!(p1, p2);
    assert_ne!(p2, p3);
    assert_ne!(p1, p3);

    // allocate() returns Err when the pool is exhausted (not a panic)
    let err = allocator.allocate();
    assert!(err.is_err());

    // release(port) returns the port to the pool so it can be re-allocated
    allocator.release(p2);

    // A port released by one instance can be re-allocated to a new instance
    let p4 = allocator.allocate().unwrap();
    assert_eq!(p2, p4);
}

#[test]
fn test_port_allocator_threading() {
    let addr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let allocator = Arc::new(PortAllocator::new(addr, 20000, 20050)); // 51 ports

    // allocate() and release() are safe to call from multiple threads simultaneously
    let mut handles = vec![];
    for _ in 0..20 {
        let alloc_clone = allocator.clone();
        handles.push(thread::spawn(move || {
            let p = alloc_clone.allocate().unwrap();
            // simulate some work
            thread::sleep(Duration::from_millis(2));
            alloc_clone.release(p);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Ensure we can still allocate all 51 ports after the concurrent thrashing
    for _ in 0..51 {
        assert!(allocator.allocate().is_ok());
    }
    assert!(allocator.allocate().is_err());
}

#[tokio::test]
async fn test_local_service_registry() {
    let registry = LocalServiceRegistry::default();
    let app_id = AppId::new("test-app", "v1");
    let addr = "127.0.0.1:8080".parse().unwrap();

    // Resolving an unknown app returns None (not an error)
    assert!(registry.resolve(&app_id).await.is_none());

    // LocalServiceRegistry::register() stores an address for an app
    registry.register(&app_id, addr).await;

    // LocalServiceRegistry::resolve() returns the stored address
    assert_eq!(registry.resolve(&app_id).await, Some(addr));

    // LocalServiceRegistry::deregister() removes the address
    registry.deregister(&app_id, &addr).await;
    assert!(registry.resolve(&app_id).await.is_none());
}

#[tokio::test]
async fn test_wait_for_ready_success() {
    // Bind a listener so the port is open
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // wait_for_ready() returns Ok within 500ms when the port opens
    let result = wait_for_ready(addr, Duration::from_millis(500)).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_wait_for_ready_timeout() {
    // Bind and immediately drop to guarantee we have an unused port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // wait_for_ready() returns Err after the timeout when the port never opens
    let result = wait_for_ready(addr, Duration::from_millis(50)).await;
    assert!(result.is_err());

    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("did not become ready"));
}
