# messaging

NATS-based pub/sub message bus for the Wasm Cloud Platform.

## Overview

The `messaging` crate provides a NATS-backed publish/subscribe message bus that serves as the central nervous system for platform event distribution. It wraps the `async_nats` client with higher-level abstractions for connection management, event publishing, subscription handling, and JetStream integration.

## Architecture

The crate is organized around several core components:

- **NatsBus** — The primary interface wrapping the `async_nats` client. Manages connections, publishes events, creates subscriptions, and configures JetStream streams and consumers.
- **Event** — An enum representing all platform events, with a `subject()` method that maps each variant to its NATS subject string.
- **NatsHealth / NatsHealthWatcher** — Connection state tracking and health reporting. `NatsHealthWatcher` periodically requests JetStream account information and records probe success or failure.
- **run_publisher** — A background task that drains events from a channel and publishes them to NATS, decoupling event production from network I/O.

Event flow:
1. Producers send `Event` variants through a channel.
2. `run_publisher` drains the channel and calls `NatsBus::publish()`.
3. Subscribers receive events via `subscribe()` or `subscribe_durable()` (JetStream).
4. `NatsHealthWatcher` probes the server and updates connected/degraded state.

## Public API

| Type | Description |
|------|-------------|
| `NatsBus` | NATS client wrapper: plaintext/local `connect()`, credentials `connect_secure()`, explicit CA/mTLS `connect_with_tls()`, publish/subscribe, and JetStream setup |
| `Event` | Enum of all platform events with `subject() -> String` mapping |
| `NatsHealth` | Connection state holder (connected flag, last update timestamp) |
| `NatsHealthWatcher` | Background task that performs a bounded server-response health probe |
| `run_publisher` | Spawns a background task to drain and publish events from a channel |
| `MessageEnvelope` | Wrapper for envelope-wrapped message serialization/deserialization |

## Known Issues & Improvements

### JetStream & Reliability

- **No way to unsubscribe or cancel subscriptions** — Subscriber tasks run indefinitely with no cancellation mechanism.
- **No JoinHandle stored for subscriber tasks** — Subscriber task failures are invisible to the caller.

### Publisher

- **No backpressure, retry, or dead-letter** — If publishing fails, the event is silently dropped. There is no retry logic or dead-letter queue.
- **`run_publisher` stops when all channel senders are dropped, but has no explicit cancellation token** — Callers must own and close the channel as part of shutdown.

### Health Monitoring

- **Health is probe-based rather than driven by connection callbacks** — `NatsHealthWatcher` requests JetStream account information and marks the dependency disconnected on failure. Detection time is bounded by the configured polling interval and the two-second probe timeout.


### Serialization & Protocol

- **`Event::subject()` allocates `String` on every call** — Static subjects could return `&'static str` to avoid allocation.

### Configuration

- **`node_id` initially defaults to `"unknown"`** — Node startup calls `set_node_id()` before publishing. Other consumers must do the same to avoid misattributed envelopes.

## Security Considerations

- `connect_with_tls()` supports a private CA, credentials, and a client
  certificate/key pair. Node production admission separately requires a
  `tls://` URL and credentials; the generic crate keeps plaintext `connect()`
  for local test fixtures.
- **Server-side subject authorization remains required** — NATS accounts and
  permissions must restrict each component to its required subjects. Client TLS
  proves transport identity but does not define those permissions.
- **No message integrity verification** — Events are not signed or authenticated, allowing any NATS client on the network to inject forged events.
- **JetStream data at rest** — No encryption is applied to persisted JetStream messages. Sensitive event payloads may be stored in plaintext on the NATS server.
- **Subscriber task isolation** — Subscriber tasks share the same Tokio runtime with no resource limits, potentially allowing a misbehaving subscriber to impact other components.
