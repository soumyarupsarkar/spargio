# Runtime Entry and Builder Configuration

## Entrypoints

Preferred entrypoints:

- `spargio::run(...)`
- `spargio::run_with(builder, ...)`
- optional `#[spargio::main(...)]` (`macros` feature)

Use `run(...)` for defaults. Use `run_with(...)` when you need explicit builder controls.

## Builder Controls

Common controls include:

- shard count
- queue capacity controls
- steal controls (`steal_*` knobs)
- thread affinity

For steal-control definitions and profiling workflow, see
[Performance Guide](12_performance_guide.md#work-stealing-controls-and-profiling).

Example:

```rust
let builder = spargio::Runtime::builder()
    .shards(4)
    .steal_budget(64)
    .steal_locality_margin(2)
    .thread_affinity(Some(vec![0, 1, 2, 3]));

spargio::run_with(builder, |handle| async move {
    let task = handle.spawn_stealable(async { 7usize }).expect("spawn");
    assert_eq!(task.await.expect("join"), 7);
})
.await?;
```

This snippet configures shard count, two steal controls, and CPU affinity, then
runs a small stealable task to validate the runtime is wired correctly.

## Handle Usage

`RuntimeHandle` is your control plane for:

- spawning tasks
- blocking bridge work (`spawn_blocking`)
- stats snapshots for tuning
- obtaining unbound native access (`uring_native_unbound`) when needed

`unbound` means native operations are not fixed to one shard up front; routing
is decided per operation (with optional preferred-shard hints).

```rust
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    let blocking = handle
        .spawn_blocking(|| std::fs::read_to_string("/etc/hostname"))
        .expect("spawn_blocking");
    let _hostname = blocking.await.expect("join blocking")?;

    let stats = handle.stats_snapshot();
    println!(
        "steal_success_rate={:.2} local_hit_ratio={:.2}",
        stats.steal_success_rate(),
        stats.local_hit_ratio()
    );

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    {
        let native = handle
            .uring_native_unbound()
            .expect("io_uring backend required");
        native.sleep(Duration::from_millis(1)).await?;
    }

    Ok(())
}
```

What this does:

- offloads a blocking filesystem read with `spawn_blocking`.
- reads runtime scheduler counters with `stats_snapshot`.
- touches unbound native API (`uring_native_unbound`) when the io_uring backend is active.

## Shutdown Expectations

Treat runtime shutdown as a lifecycle boundary:

- avoid leaking long-running background work you still depend on
- wire cancellation and task groups for cooperative teardown
- use boundary timeouts and cancellation so shutdown does not hang
