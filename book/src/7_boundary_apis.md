# Boundary APIs

Use boundary APIs when you need explicit async request-reply behavior across components or runtime contexts.

## Which Boundary Call To Use

| Goal | Client side | Server side |
| --- | --- | --- |
| Default request/reply with bounded wait | `call_with_timeout(...)` | `recv_timeout(...)` |
| Internal path where timeout is managed elsewhere | `call(...)` | `recv(...)` |
| Add a second-stage reply deadline after enqueue | `BoundaryTicket::wait_timeout(...)` | `respond(...)` |

Practical default for user-facing paths:

1. `call_with_timeout(...)`
2. `recv_timeout(...)`
3. `wait_timeout(...)` if reply computation can outlive queue wait budget

## Example: Request/Reply With Explicit Timeouts

```rust
use spargio::{boundary, RuntimeHandle};
use std::time::Duration;

#[spargio::main]
async fn main(handle: RuntimeHandle) {
    let (client, server) = boundary::channel::<u64, u64>(64);

    let worker = handle
        .spawn_stealable(async move {
            loop {
                match server.recv_timeout(Duration::from_millis(200)).await {
                    Ok(req) => {
                        let value = *req.request();
                        let _ = req.respond(value * 2);
                    }
                    Err(boundary::BoundaryError::Timeout) => continue,
                    Err(boundary::BoundaryError::Closed) => break,
                    Err(boundary::BoundaryError::Canceled) => break,
                    Err(boundary::BoundaryError::Overloaded) => continue,
                }
            }
        })
        .expect("spawn worker");

    let ticket = client
        .call_with_timeout(21, Duration::from_secs(1))
        .await
        .expect("enqueue");
    let reply = ticket
        .wait_timeout(Duration::from_secs(1))
        .await
        .expect("reply");
    assert_eq!(reply, 42);

    drop(client);
    worker.await.expect("join worker");
}
```

What this does:

- `call_with_timeout` bounds enqueue + queue-wait time for the caller.
- `recv_timeout` lets the server keep a heartbeat loop instead of waiting forever.
- `wait_timeout` adds a response deadline after enqueue succeeds.
- `req.respond(...)` completes the ticket and returns the response to the caller.

## When Boundary Helps

- decoupling producers and handlers with explicit queueing
- surfacing overload and timeout as first-class outcomes
- integrating mixed-mode services where not all work sits in one task tree

## Operational Guidance

- treat timeout values as API contract, not arbitrary constants
- monitor overload counters and timeout counters in runtime stats
- use `call_with_timeout` by default for user-facing request paths

## Failure Behavior

Design for:

- timeout
- overload/backpressure
- cancellation before handler completion

Boundary APIs are strongest when these paths are intentional and tested.
