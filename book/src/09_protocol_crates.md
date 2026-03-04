# Companion Protocol Crates (TLS/WS/QUIC/Process/Signal)

Spargio keeps protocol stacks outside core runtime and provides companion adapters.

## Companion Crates

- `spargio-tls`: `rustls` + `futures-rustls` bridge.
- `spargio-ws`: `async-tungstenite` bridge.
- `spargio-quic`: QUIC support with backend modes.
- `spargio-process`: process execution utilities.
- `spargio-signal`: signal handling utilities.
- `spargio-protocols`: legacy compatibility bridge helpers.

## Which Crate Should You Pick?

| If you need... | Use | Notes |
| --- | --- | --- |
| TLS over Spargio TCP | `spargio-tls` | handshake timeout options, rustls-based |
| WebSocket over Spargio TCP | `spargio-ws` | ws client/server handshake helpers |
| QUIC endpoints/streams | `spargio-quic` | native backend by default, bridge fallback |
| Async process launch/wait/output | `spargio-process` | wraps blocking process APIs behind async interface |
| Async signal subscription | `spargio-signal` | useful for shutdown hooks and process control |
| Compatibility bridge for legacy integration | `spargio-protocols` | explicit blocking bridge path |

If you need the newest upstream protocol feature immediately, use upstream crates directly and integrate with Spargio sockets/runtime primitives.

## `spargio-tls` Example

```rust
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use spargio_tls::{HandshakeOptions, TlsConnector};
use std::sync::Arc;
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let config = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth(),
    );
    let connector = TlsConnector::new(config)
        .with_options(HandshakeOptions::default().with_timeout(Duration::from_secs(2)));

    let addr: std::net::SocketAddr = "127.0.0.1:4433".parse().expect("addr");
    let server_name = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let _tls = connector.connect_socket_addr(handle, addr, server_name).await?;
    Ok(())
}
```

What this does: constructs a TLS connector with handshake timeout policy and dials a concrete socket address through Spargio TCP.

## `spargio-ws` Example

```rust
use spargio_ws::{WsOptions, connect_socket_addr_with_options};
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let addr: std::net::SocketAddr = "127.0.0.1:9001".parse().expect("addr");
    let (_ws, response) = connect_socket_addr_with_options(
        handle,
        addr,
        "/chat",
        WsOptions::default().with_timeout(Duration::from_secs(2)),
    )
    .await?;

    println!("ws handshake status={}", response.status());
    Ok(())
}
```

What this does: performs WebSocket client handshake with timeout policy using Spargio socket setup.

## `spargio-quic` Example

```rust
use spargio_quic::quinn;
use spargio_quic::{QuicEndpoint, QuicEndpointOptions};
use std::sync::Arc;
use std::time::Duration;

fn empty_root_client_config() -> quinn::ClientConfig {
    let roots = rustls::RootCertStore::empty();
    quinn::ClientConfig::with_root_certificates(Arc::new(roots)).expect("client config")
}

async fn quic_client() -> std::io::Result<()> {
    let options = QuicEndpointOptions::default()
        .with_connect_timeout(Duration::from_secs(2))
        .with_operation_timeout(Duration::from_secs(2));
    let mut endpoint = QuicEndpoint::client_with_options("127.0.0.1:0".parse().expect("addr"), options)
        .expect("endpoint");
    endpoint.set_default_client_config(empty_root_client_config());

    let conn = endpoint.connect("127.0.0.1:4433".parse().expect("addr"), "localhost").await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"ping").await?;
    send.finish()?;
    let _reply = recv.read_to_end(64 * 1024).await?;
    Ok(())
}
```

What this does: creates a QUIC endpoint, applies connect/operation budgets, opens a bidirectional stream, writes a request, and reads the full reply.

For long-lived framed protocols, prefer incremental stream reads and owned-byte writes:

```rust
use bytes::Bytes;

async fn stream_loop(mut conn: spargio_quic::QuicConnection) -> std::io::Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;

    let mut out = Bytes::from_static(b"frame-1frame-2");
    while !out.is_empty() {
        let wrote = send.write_bytes(out.clone()).await?;
        if wrote == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "quic write_bytes returned zero",
            ));
        }
        out = out.slice(wrote.min(out.len())..);
    }
    send.finish()?;

    while let Some(chunk) = recv.read_chunk(16 * 1024).await? {
        println!("received {} bytes", chunk.len());
        // decode frames incrementally here instead of waiting for read_to_end
    }
    Ok(())
}
```

What this does: writes owned byte chunks without rebuilding `Vec<u8>` payloads and consumes incoming data incrementally with `read_chunk`, which is a better fit for long-lived framed control/data streams.

## `spargio-process` Example

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let status = spargio_process::CommandBuilder::new("sh")
        .args(["-c", "echo spargio-process-ok >/tmp/spargio-process-demo.txt"])
        .status(&handle)
        .await?;
    assert!(status.success());
    Ok(())
}
```

What this does: runs a subprocess through the async process bridge and waits for exit status without blocking runtime lanes.

## `spargio-signal` Example

```rust
use std::time::Duration;

#[spargio::main]
async fn main(_handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let stream = spargio_signal::ctrl_c()?;
    match stream.recv_timeout(Duration::from_secs(30)).await? {
        Some(sig) => println!("received signal {sig}"),
        None => println!("no signal within timeout"),
    }
    Ok(())
}
```

What this does: subscribes to Ctrl+C and awaits one signal with a bounded timeout.

## `spargio-protocols` Bridge Example

```rust
use spargio_protocols::BlockingOptions;
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let out = spargio_protocols::tls_blocking_with_options(
        &handle,
        BlockingOptions::default().with_timeout(Duration::from_secs(1)),
        || Ok::<_, std::io::Error>("legacy adapter path"),
    )
    .await?;
    assert_eq!(out, "legacy adapter path");
    Ok(())
}
```

What this does: runs a protocol operation on the explicit blocking bridge with timeout control for legacy integrations.

## QUIC Backend Modes

`QuicBackend` supports:

- `Native` (default dispatch)
- `Bridge` (compatibility fallback)

Current native path uses `quinn-proto` driver integration with Spargio-native pump/timer orchestration.

## Direct Upstream vs Companion Adapter

Use direct upstream crate when:

- you need newest upstream APIs immediately
- you already maintain runtime-neutral integration glue

Use `spargio-*` adapter when:

- you want Spargio-aligned timeout/cancel behavior
- you want one project-standard integration path
- you want in-repo examples and migration guidance
