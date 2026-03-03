# I/O API Layers and Selection

Spargio exposes layered I/O APIs. Choose the highest layer that fits your use case.

## Layer 1: Ergonomic fs/net APIs

High-level APIs:

- `spargio::fs::{OpenOptions, File}` plus path helpers (`create_dir*`, `rename`, `remove_*`, metadata/link helpers, `read`, `write`).
- `spargio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream, UnixDatagram}`.
- `spargio::io::{AsyncRead, AsyncWrite, split, copy_to_vec, BufReader, BufWriter}` and framed helpers.

Use this layer first for most services.

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    spargio::fs::write(&handle, "/tmp/spargio-io-layer1.txt", b"hello").await?;
    let bytes = spargio::fs::read(&handle, "/tmp/spargio-io-layer1.txt").await?;
    assert_eq!(&bytes, b"hello");

    let listener = spargio::net::TcpListener::bind(handle.clone(), "127.0.0.1:7002").await?;
    let (stream, _) = listener.accept_round_robin().await?;
    let payload = stream.recv(4096).await?;
    stream.write_all(&payload).await?;
    Ok(())
}
```

What this does:

- writes and reads a file through the high-level fs helpers.
- accepts one TCP connection through the high-level net helper.
- reads a payload and echoes it back with stream helpers.

```rust
use spargio::io::{AsyncWrite, BufReader, BufWriter, copy_to_vec, split};

async fn copy_and_ack(stream: spargio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let (reader, writer) = split(stream);
    let reader = BufReader::with_capacity(reader, 16 * 1024);
    let mut all_bytes = Vec::new();
    let _copied = copy_to_vec(&reader, &mut all_bytes, 8 * 1024).await?;

    let buffered = BufWriter::new(writer);
    let _ = buffered.write_owned(b"ok\n".to_vec()).await?;
    buffered.flush().await?;
    Ok(all_bytes)
}
```

What this does:

- splits one stream into read/write halves with `split`.
- buffers reads with `BufReader` and copies bytes with `copy_to_vec`.
- writes a buffered acknowledgement via `BufWriter`.

## Layer 2: Native unbound API

For direct operation control and shard-aware routing controls:

- `RuntimeHandle::uring_native_unbound() -> UringNativeAny`
- `UringNativeAny::with_preferred_shard(...)`
- `UringNativeAny::select_shard(...)`

This is appropriate for advanced pipelines and extension authors.

What "unbound" means here:

- the API handle is not permanently tied to one shard
- each submitted operation is routed at submission time by Spargio's selector
- you can provide routing hints (preferred shard), but routing is still per-op

This differs from session-bound stream APIs (for example a `TcpStream` session shard),
where operations are intentionally kept on that stream's session shard.

```rust
#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let native_any = handle
        .uring_native_unbound()
        .expect("io_uring backend required");
    let preferred = 0 as spargio::ShardId;
    // Configure a default preferred shard hint on this unbound handle.
    let native = native_any
        .with_preferred_shard(preferred)
        .expect("valid preferred shard");
    // `None` means "no per-call override": use the handle's configured preferred
    // shard hint (if any), then let the selector make the final routing choice.
    let selected = native.select_shard(None).expect("select shard");
    println!("native preferred_shard={preferred} selected_shard={selected}");

    std::fs::write("/tmp/spargio-native-any.txt", b"native")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open("/tmp/spargio-native-any.txt")?;

    let out = native.read_at(file.as_raw_fd(), 0, 6).await?;
    assert_eq!(&out, b"native");
    Ok(())
}
```

What this does:

- acquires `UringNativeAny` from the runtime handle.
- sets an explicit preferred shard for native routing.
- lets you inspect the selector's chosen shard before submitting operations.
- submits a direct `read_at` against a raw fd.
- gets completion as a regular async result without helper threads.

`with_preferred_shard(...)` is a preferred-routing control. For strict per-op
shard routing, use `submit_unsafe_on_shard(...)` in Layer 3.

## Layer 3: Custom io_uring Opcodes via Unsafe Extension API

For custom SQE/CQE workflows not in core APIs:

- `UringNativeAny::submit_unsafe`
- `UringNativeAny::submit_unsafe_on_shard`

Use safe wrappers around unsafe calls. See [Extending Spargio with Custom io_uring Opcodes](native_extensions.md).

```rust
#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    use io_uring::opcode;

    let native = handle
        .uring_native_unbound()
        .expect("io_uring backend required");
    let shard = native.select_shard(Some(0)).expect("select shard");

    let cqe_result = unsafe {
        native
            .submit_unsafe_on_shard(
                shard,
                (),
                |_| Ok(opcode::Nop::new().build()),
                |(), cqe| Ok::<i32, std::io::Error>(cqe.result),
            )
            .await?
    };

    assert_eq!(cqe_result, 0);
    Ok(())
}
```

What this does:

- selects an explicit shard for this low-level operation.
- submits a custom SQE (`NOP`) not wrapped by a high-level helper.
- decodes the CQE in a completion closure.
- keeps unsafe isolated to one call site, which is the expected extension pattern.

## DNS Caveat

Hostname `ToSocketAddrs` paths can still block for resolution. For strict non-DNS data-plane behavior, use explicit `SocketAddr` APIs:

- `connect_socket_addr*`
- `bind_socket_addr`

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("socket addr");
    let _stream = spargio::net::TcpStream::connect_socket_addr(handle, addr).await?;
    Ok(())
}
```

What this does: bypasses hostname lookup entirely and connects on a concrete `SocketAddr`.

## Filesystem Caveat

Some fs helpers are still compatibility blocking paths (`canonicalize`, `metadata`, `symlink_metadata`, `set_permissions`) while higher-value helpers are native-first.

## Directory Traversal and DU

Available now:

- `spargio::fs::read_dir(...)`
- `spargio::fs::du(...)` with `DuOptions` and `DuSummary`
- low-level `spargio::extension::fs::read_dir_entries(...)`

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let entries = spargio::fs::read_dir(&handle, "/var/log").await?;
    let du = spargio::fs::du(&handle, "/var/log", spargio::fs::DuOptions::default()).await?;
    let raw = spargio::extension::fs::read_dir_entries(handle.clone(), "/var/log").await?;

    println!(
        "entries={} raw_entries={} total_bytes={}",
        entries.len(),
        raw.len(),
        du.total_bytes
    );
    Ok(())
}
```

What this does:

- uses high-level `read_dir` for ergonomic traversal.
- uses `du` to summarize recursive disk usage with policy knobs.
- uses low-level `read_dir_entries` when you need raw extension-layer entry data.

Current limitation (as of 2026-03-03): fully in-ring directory traversal via `IORING_OP_GETDENTS` is not yet available in upstream stable ABI surfaces, so traversal currently uses a compatibility helper lane.
