# Migration Guide

## From Tokio

Typical mapping:

- `tokio::spawn` -> explicit placement (`spawn_pinned`, `spawn_stealable`, etc.)
- `tokio::task::spawn_local` -> `run_local_on` / `spawn_local_on`
- ad-hoc timeout wrappers -> `timeout` / `timeout_at` and boundary timeout APIs

```rust
use spargio::{TaskPlacement, boundary};
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) {
    let (client, server) = boundary::channel::<u64, u64>(32);

    let worker = handle
        .spawn_with_placement(TaskPlacement::Sticky(42), async move {
            let req = server.recv().await.expect("recv");
            req.respond(*req.request() + 1).expect("respond");
        })
        .expect("spawn worker");

    let ticket = client
        .call_with_timeout(7, Duration::from_millis(250))
        .await
        .expect("enqueue");
    let out = ticket.await.expect("reply");
    assert_eq!(out, 8);

    worker.await.expect("join");
}
```

What this does: replaces implicit global spawn with explicit placement, and replaces ad-hoc timeouts with boundary timeout APIs.

Recommended migration order:

1. migrate entrypoint (`run`/`run_with`)
2. migrate hot-path spawning with explicit placement
3. migrate fs/net calls to Spargio APIs where beneficial
4. capture benchmark baselines before tuning

## From Compio or monoio

Start from API surface parity, then optimize placement and boundaries.

Recommended order:

1. port fs/net happy paths using ergonomic Spargio APIs
2. port runtime placement semantics
3. map protocol integration to companion crates (or direct upstream where needed)
4. use native extension APIs for operations not exposed in core

```rust
#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let native = handle
        .uring_native_unbound()
        .expect("io_uring backend required");
    let shard = native.select_shard(None).expect("select shard");
    let _ = unsafe {
        native
            .submit_unsafe_on_shard(
                shard,
                (),
                |_| Ok(io_uring::opcode::Nop::new().build()),
                |(), cqe| Ok::<i32, std::io::Error>(cqe.result),
            )
            .await?
    };
    Ok(())
}
```

What this does: shows the migration escape hatch when you need an operation that is not yet in Spargio's safe core surface.

## Compatibility Notes

- Hostname resolution may still block on some paths; prefer explicit `SocketAddr` APIs for strict non-DNS data planes.
- Some fs helpers intentionally remain compatibility blocking paths where io_uring migration is not yet a clear net win.
