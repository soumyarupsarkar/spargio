# Time and Cancellation

Spargio ships common runtime primitives for deadline-driven services.

## Time Primitives

- `sleep`, `sleep_until`
- `timeout`, `timeout_at`
- `interval`, `interval_at`
- `Sleep` (resettable timer)

Use `timeout` for request budgets and `interval` for periodic workers.

## Cancellation Primitives

- `CancellationToken`
- `TaskGroup`

Recommended pattern:

1. create one token per subsystem
2. pass clones to workers
3. signal cancel during shutdown
4. await task group completion

## Example Pattern

```rust
use std::time::Duration;

async fn bounded_work() {
    let op = async {
        spargio::sleep(Duration::from_millis(25)).await;
        1usize
    };
    let out = spargio::timeout(Duration::from_millis(50), op).await;
    assert!(out.is_ok());
}
```

This snippet wraps work with a deadline budget: the operation sleeps for
25ms and is bounded by a 50ms timeout.

## Cancellation Token and Task Group Example

```rust
use spargio::{CancellationToken, TaskGroup, TaskPlacement};
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) {
    let token = CancellationToken::new();
    let token_waiter = handle
        .spawn_stealable({
            let token = token.clone();
            async move {
                token.cancelled().await;
                "token canceled"
            }
        })
        .expect("spawn token waiter");

    let group = TaskGroup::new(handle.clone());
    let grouped = group
        .spawn_with_placement(TaskPlacement::Stealable, async {
            loop {
                spargio::sleep(Duration::from_millis(10)).await;
            }
        })
        .expect("spawn grouped worker");

    token.cancel();
    assert_eq!(token_waiter.await.expect("join"), "token canceled");

    group.cancel();
    assert!(grouped.await.expect("join").is_none());
}
```

What this does:

- uses `CancellationToken` for direct one-to-many cancellation signaling.
- uses `TaskGroup` to attach cancellation behavior to spawned work.
- shows grouped tasks returning `None` when canceled before completion.

## Common Mistakes

- hard-coded long timeouts with no service-level budget.
- per-call ad-hoc cancellation instead of shared subsystem token.
- no bounded waits during teardown.
