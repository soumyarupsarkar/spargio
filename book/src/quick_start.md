# Quick Start

This chapter gives you one end-to-end Spargio-only example first, then a
Tokio embedding example.

## Prerequisites

- Linux environment.
- Rust toolchain (`cargo`, `rustc`).
- Kernel/runtime support for your selected backend.

## Add Dependencies

```toml
[dependencies]
spargio = { version = "0.5", features = ["uring-native", "macros"] }
```

This enables the io_uring-backed runtime path and the `#[spargio::main]`
entry macro used below.

## A Tiny Ingestion Flow

```rust
use std::{
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use spargio::{net::TcpListener, RuntimeHandle};

#[spargio::main]
async fn main(handle: RuntimeHandle) -> io::Result<()> {
    let mut out = std::env::temp_dir();
    out.push(format!(
        "spargio-ingest-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    let listener = TcpListener::bind(handle.clone(), "127.0.0.1:7001").await?;
    eprintln!("listening on {}", listener.local_addr()?);
    eprintln!("writing first received event to {}", out.display());

    let writer_runtime = handle.clone();
    let out_for_server = out.clone();
    let (stream, _) = listener.accept_round_robin().await?;
    let worker = handle
        .spawn_stealable(async move {
            let payload = stream.recv(64 * 1024).await.expect("recv");
            if payload.is_empty() {
                return;
            }

            spargio::fs::write(&writer_runtime, &out_for_server, &payload)
                .await
                .expect("write file");

            let ack = format!("stored {} bytes", payload.len());
            stream.write_all(ack.as_bytes()).await.expect("reply");
        })
        .expect("spawn_stealable");

    worker.await.expect("join");
    eprintln!("done");
    Ok(())
}
```

What this does:

- starts a TCP listener on `127.0.0.1:7001`
- accepts one connection
- spawns a stealable task to read one payload and persist it
- writes the payload to a temp file
- replies to the client with `stored N bytes`

Try it from another terminal:

```bash
printf 'event=user.signup,user_id=42' | nc 127.0.0.1 7001
```

You will see the output file path in the server terminal:
`writing first received event to /tmp/spargio-ingest-<timestamp>.txt`
(exact temp directory comes from `std::env::temp_dir()` on your machine).

## Embed Spargio In Tokio

If your app is Tokio-first, call Spargio from a Tokio async context.

```toml
[dependencies]
spargio = { version = "0.5", features = ["uring-native"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Use this when Tokio remains your outer runtime and you only want to run selected
paths inside Spargio.

```rust
#[tokio::main]
async fn main() -> Result<(), spargio::RuntimeError> {
    spargio::run(|handle| async move {
        let task = handle.spawn_stealable(async { 7usize }).expect("spawn");
        assert_eq!(task.await.expect("join"), 7);
    })
    .await
}
```

This snippet keeps Tokio as the application entry runtime and calls
`spargio::run(...)` for a scoped Spargio execution block.

## What To Read Next

- Choose placement and local execution: [Task Placement and !Send Execution](runtime_model.md)
- Understand APIs and caveats: [I/O API Layers and Selection](io_apis.md)
- Production workflow: [Performance Guide](performance_guide.md)
