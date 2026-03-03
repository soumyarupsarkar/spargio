# Task Placement and !Send Execution

Spargio is easiest to use when you treat it as a shard-oriented runtime with explicit placement decisions.

## Core Concepts

- `Shard`: a runtime execution lane with its own scheduling context.
- Placement identifiers: `Pinned(shard)`, `RoundRobin`, `Sticky(key)`,
  `Stealable`, `StealablePreferred(shard)`.
- `Session-local task`: local `!Send` execution pinned to a shard context.
- `Boundary`: a structured cross-runtime/cross-component request-reply channel.

## Default Recommendation: `StealablePreferred`

As a default, use stealable locality-first placement:

- use `spawn_stealable_on(shard, ...)` (equivalent to `StealablePreferred(shard)`)
- default preferred shard: the shard you are currently spawning from
- choose a different preferred shard only when you intentionally route by ownership/session key

Why this is the default:

- you get thread locality for cache-friendly hot paths
- other shards can still steal work when imbalance makes it worthwhile
- migration happens only when Spargio's runtime heuristics decide the expected
  benefit is worth the task migration cost

```rust
use spargio::{RuntimeError, RuntimeHandle, ShardCtx};

#[spargio::main]
async fn main(handle: RuntimeHandle) -> Result<(), RuntimeError> {
    let h = handle.clone();
    let demo = handle.spawn_stealable(async move {
        let current = ShardCtx::current().expect("shard context").shard_id();

        // Default: prefer the shard we are currently spawning from.
        let local_first = h
            .spawn_stealable_on(current, async { "local_first" })
            .expect("spawn");

        // Intentional routing: map an ownership key to a shard.
        let owner_key = 42u64;
        let owner_shard = (owner_key as usize % h.shard_count()) as spargio::ShardId;
        let routed = h
            .spawn_stealable_on(owner_shard, async { "ownership_routed" })
            .expect("spawn");

        let _ = local_first.await.expect("join");
        let _ = routed.await.expect("join");
    })?;

    demo.await?;
    Ok(())
}
```

What this does:

- uses the current spawning shard as the default preferred shard.
- shows routing to another shard only when ownership/session mapping is intentional.

## Running !Send Work: `run_local_on` and `spawn_local_on`

Use local APIs when your future captures non-`Send` state (for example `Rc`,
`RefCell`, or non-thread-safe FFI handles).

```rust
use std::rc::Rc;

async fn local_entrypoint_demo() -> Result<(), spargio::RuntimeError> {
    let out = spargio::run_local_on(
        spargio::Runtime::builder().shards(2),
        0,
        |_ctx| async move {
            let local_only = Rc::new(String::from("local-state"));
            local_only.len()
        },
    )
    .await?;

    assert_eq!(out, "local-state".len());
    Ok(())
}
```

What this does:

- runs a top-level `!Send` future on one explicit shard.
- allows local-only values (`Rc`) that cannot cross threads.
- keeps the future pinned to that shard for its entire lifetime.

```rust
use spargio::{RuntimeError, RuntimeHandle};
use std::rc::Rc;

#[spargio::main]
async fn main(handle: RuntimeHandle) -> Result<(), RuntimeError> {
    let join = handle.spawn_local_on(0, |_ctx| async move {
        let local_numbers = Rc::new(vec![1u64, 2, 3, 4]);
        local_numbers.iter().sum::<u64>()
    })?;

    assert_eq!(join.await?, 10);
    Ok(())
}
```

What this does:

- spawns one shard-local `!Send` task from an existing runtime handle.
- keeps execution pinned to shard `0` with no migration.
- returns a normal join handle so local work still composes with regular async flows.

## Execution Model

1. You choose an entrypoint (`run`, `run_with`, or `#[spargio::main]`).
2. You choose placement when spawning (for example `spawn_pinned`,
   `spawn_stealable_on`, or `spawn_with_placement(...)`).
3. Task execution location follows placement:
   - `Pinned(shard)`: always runs on the shard you specify.
   - `Sticky(key)`: key is deterministically mapped to one shard; same key stays
     on that shard.
   - `RoundRobin` is spawned on the next shard in a round-robin rotation and
     stays there.
   - `Stealable`: starts as migratable work with no preferred shard.
   - `StealablePreferred(shard)`: starts with a preferred shard for locality, but
     can still migrate if steal heuristics decide migration is worth the cost.
4. I/O can run through high-level fs/net APIs or lower-level native extension
   paths.

## Two Scheduling Goals

- Keep locality when locality is valuable (cache-friendly shard-local state).
- Allow migration when queue imbalance is the bigger cost.

This tradeoff is why placement policy and scheduler tuning both matter.

## Practical Rule of Thumb

- Use `Pinned` when you already know the owning shard.
- Use `Sticky` when you have a stable key (session/user/partition) but do not
  want to manually map shards.
- Use `RoundRobin` for straightforward spread at submission time.
- Default for migratable work: use `spawn_stealable_on(shard, ...)` (equivalent
  to `StealablePreferred(shard)`), usually on the current spawning shard.
- Route to a different preferred shard only when you intentionally align with
  ownership/session locality.
- Use `Stealable` when no meaningful preferred shard exists.
- Tune only after measuring with representative workload shapes.

## Placement APIs in Code

```rust
use spargio::{RuntimeError, RuntimeHandle, ShardCtx, TaskPlacement};

#[spargio::main]
async fn main(handle: RuntimeHandle) -> Result<(), RuntimeError> {
    // Run this demo from inside a shard context.
    let h = handle.clone();
    let demo = handle.spawn_stealable_on(0, async move {
        let current = ShardCtx::current().expect("shard context").shard_id();
        let other = ((usize::from(current) + 1) % h.shard_count()) as spargio::ShardId;

        // Pinned to the current shard.
        let pinned_here = h.spawn_pinned(current, async { "pinned_here" }).expect("spawn");
        // Pinned to a different shard.
        let pinned_other = h.spawn_pinned(other, async { "pinned_other" }).expect("spawn");

        // Spawn on next shard in round-robin rotation.
        let rr = h
            .spawn_with_placement(TaskPlacement::RoundRobin, async { "round_robin" })
            .expect("spawn");
        // Deterministic key-based shard routing.
        let sticky = h
            .spawn_with_placement(TaskPlacement::Sticky(42), async { "sticky" })
            .expect("spawn");
        // Locality-first migratable task (recommended default).
        let preferred_default = h
            .spawn_stealable_on(current, async { "stealable_preferred_default" })
            .expect("spawn");
        // Equivalent explicit placement API form.
        let preferred_explicit = h
            .spawn_with_placement(
                TaskPlacement::StealablePreferred(other),
                async { "stealable_preferred_explicit" },
            )
            .expect("spawn");
        // Migratable with no preferred start shard (fallback when no ownership hint exists).
        let stealable_any = h
            .spawn_with_placement(TaskPlacement::Stealable, async { "stealable_any" })
            .expect("spawn");

        let _ = pinned_here.await.expect("join");
        let _ = pinned_other.await.expect("join");
        let _ = rr.await.expect("join");
        let _ = sticky.await.expect("join");
        let _ = preferred_default.await.expect("join");
        let _ = preferred_explicit.await.expect("join");
        let _ = stealable_any.await.expect("join");
    })?;

    demo.await?;
    Ok(())
}
```

This snippet shows locality-first `StealablePreferred` as the default migratable
choice (via `spawn_stealable_on(...)` and explicit placement), with plain
`Stealable` as the no-preference fallback.
