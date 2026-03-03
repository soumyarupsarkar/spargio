# Extending Spargio with Custom io_uring Opcodes

When the core crate does not expose a needed operation, use Spargio's native extension APIs.

## Extension Surface

Unsafe submission entrypoints:

- `UringNativeAny::submit_unsafe(...)`
- `UringNativeAny::submit_unsafe_on_shard(...)`

Safe wrapper example in core:

- `spargio::extension::fs::statx*`

## Build A Safe Wrapper (Authoring Pattern)

```rust
#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub async fn read_prefix(
    native: &spargio::UringNativeAny,
    fd: std::os::fd::RawFd,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    use io_uring::{opcode, types};

    struct ReadState {
        fd: std::os::fd::RawFd,
        buf: Vec<u8>,
    }

    let request_len = len.max(1);
    let request_len_u32 = u32::try_from(request_len)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "buffer too large"))?;

    let out = unsafe {
        native
            .submit_unsafe(
                ReadState {
                    fd,
                    buf: vec![0u8; request_len],
                },
                move |state| {
                    Ok(opcode::Read::new(types::Fd(state.fd), state.buf.as_mut_ptr(), request_len_u32).build())
                },
                |mut state, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    state.buf.truncate(cqe.result as usize);
                    Ok(state.buf)
                },
            )
            .await?
    };

    Ok(out)
}
```

What this does:

- defines a safe function (`read_prefix`) that extension users call.
- keeps all unsafe SQE/CQE handling private to the wrapper implementation.
- guarantees state/buffer lifetime by storing them in owned wrapper state.
- returns plain `io::Result<Vec<u8>>`, so callers do not touch unsafe APIs.

## Use The Safe Wrapper

```rust
#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    std::fs::write("/tmp/spargio-unsafe-read.txt", b"unsafe-path")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open("/tmp/spargio-unsafe-read.txt")?;
    let native = handle
        .uring_native_unbound()
        .expect("io_uring backend required");

    let out = read_prefix(&native, file.as_raw_fd(), 6).await?;

    assert_eq!(&out, b"unsafe");
    Ok(())
}
```

What this does: uses your safe wrapper from normal async code. Callers never use
`submit_unsafe*` directly.

## Existing Safe Wrapper In Spargio Core

Spargio already follows this pattern in `spargio::extension::fs::statx*`.
Example call site:

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let meta = spargio::extension::fs::statx_or_metadata(handle, "/tmp").await?;
    println!("size={} mode={:#o}", meta.size, meta.mode);
    Ok(())
}
```

## Safety Contract

Extension authors must ensure:

- all pointers in SQE payload remain valid until CQE completion
- state captured for completion is owned and lives long enough
- CQE interpretation is correct for each operation
- error paths do not leak resources

## Recommended Wrapper Pattern

1. Define a small owned operation state struct.
2. Build SQE from that state in one closure.
3. Parse CQE and transform to ergonomic Rust output in completion closure.
4. Expose a safe API that hides all unsafe internals.
5. Add tests for success, failure, and unsupported-kernel fallback paths.

## Design Guidance

- Keep unsafe blocks tiny and isolated.
- Prefer typed option structs rather than loose flags in public API.
- Add capability/fallback behavior explicitly rather than implicit panics.

See also: `docs/native_extension_cookbook.md`.
