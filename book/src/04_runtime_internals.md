# Runtime Internals

This chapter gives a practical mental model of Spargio internals so placement
and tuning choices are easier to reason about.

## What A Shard Is

On the Linux `io_uring` backend, one shard is:

- one worker thread
- one local async scheduler/executor context
- one `io_uring` instance for that thread (its own submission/completion lanes)
- one set of queues for commands, payloads, and stealable tasks

So a runtime with `N` shards is `N` worker threads plus `N` rings, connected by
cross-shard messaging and queue-based dispatch.

## What Work-Stealing Means In Spargio

In Spargio, work-stealing is dispatch optimization for pending stealable tasks.
Work is queued first; shard-to-shard migration decisions happen while tasks are
still queued, before they are executed.

The goal is to avoid imbalance: one overloaded shard with deep queues while
other shards sit underutilized. That imbalance usually hurts throughput and
tail latency.

Migration is not free, so Spargio treats stealing as a cost/benefit decision.
As a rough CPU-latency reference, an L1 cache hit is often single-digit cycles
(roughly ~0.5-1.5 ns), while an L3 hit is often a few dozen cycles (roughly
~10-20 ns), depending on CPU and clock speed. L1 is core-local; L3 is shared
across cores. Cross-shard task migration can
incur multiple cache misses plus multiple synchronization steps (queue atomics,
locks, wake signaling), so migration is only used when it is expected to help.

Spargio uses two levers together:

1. Submission-time dispatch:
   `spawn_stealable_on(shard, ...)` pins the preferred start shard explicitly
   (`StealablePreferred`), while `spawn_stealable(...)` picks a preferred start
   shard via round-robin dispatch (`Stealable`).
2. Execution-time stealing:
   each shard drains its local stealable queue first, then (when local work is
   low) probes other shards' queues and steals tasks if migration is expected to
   pay off.

At a high level, stealing is gated by:

`estimated_time_saved > estimated_migration_cost + locality_margin`

### How Submission Works

- `spawn_stealable_on(shard, ...)` (`StealablePreferred`) queues work with an
  explicit preferred shard.
- `spawn_stealable(...)` (`Stealable`) also queues stealable work, but its
  initial preferred shard is chosen by runtime round-robin dispatch at
  submission time.
- both forms enqueue into per-shard stealable userspace queues (inboxes), not
  directly into the target shard's run queue.
- after enqueue, Spargio relies on `msg_ring` doorbells to wake target shards
  promptly when needed.

This is the first lever: choosing the initial queue that receives the task.

### How Wakeups Work (`msg_ring` Doorbells)

Who is polling:

- shard worker threads poll/drain userspace queues in the runtime loop
- the kernel processes `IORING_OP_MSG_RING` submissions and emits CQEs (and if
  `SQPOLL` is enabled, a kernel SQPOLL thread polls the ring submission queue)

Userspace queue polling alone is not enough here because queued payload/work is
in userspace memory and is not itself a kernel completion event. Doorbells turn
"queue became non-empty" into a targeted CQE for the destination shard.

After queueing stealable work, Spargio wakes the target shard:

- wake intent is coalesced (duplicate wakes are suppressed)
- a doorbell is sent through `IORING_OP_MSG_RING` to the target shard
- target shard's runtime loop polls its ring CQ and observes a doorbell CQE
  (`MSG_RING_CQE_FLAG` + doorbell tag), then runs a drain pass on queued work

So `msg_ring` is the wake/signal path, while task payload stays in userspace
queues. This is cheaper than constantly sweeping remote queues across shards,
because one targeted doorbell CQE avoids repeated cross-shard lock/atomic
traffic from blind polling.

### How Queue Polling and Stealing Works

Each worker shard repeatedly runs a drain loop:

- drain local queued stealable tasks first (locality-first)
- if local queued work is insufficient and steal budget remains, probe candidate
  victim shards
- estimate whether stealing is worth it
- steal a batch from a victim queue only when the gate passes
- apply backoff/cooldown after repeated failed steal scans

This is the second lever: dynamic rebalance by polling queue state at runtime.

For the default deque-backed stealable queue, local draining pops from the front
and stealing pops from the back of the victim queue. This split helps the victim
keep making forward progress on its local front, while thieves take tail work.
Tail work is usually less likely to be in the victim core's hottest L1 working
set, so stealing from the tail reduces disruption to victim-local cache
locality.

Backend contrast:

- `StealableQueueBackend::Mutex` (default) uses `Mutex<VecDeque<_>>`, so local
  and stolen paths can intentionally use front/back pops.
- `StealableQueueBackend::SegQueueExperimental` uses `SegQueue`, which has one
  shared `pop()` path; local and stolen dequeues therefore follow the same pop
  behavior (no strict front/back split), trading that locality control for a
  lock-free queue shape.
- Tokio uses a more specialized scheduler queue stack (custom local run queue,
  global inject queue, and per-worker fast-path/LIFO behavior). That design is
  likely better-optimized than Spargio's current generic stealable queue
  backends for many production workloads, and we will evaluate similar queue
  specialization later.

### Simple Walkthrough

```rust
use spargio::{RuntimeError, RuntimeHandle, ShardCtx};

#[spargio::main]
async fn main(handle: RuntimeHandle) -> Result<(), RuntimeError> {
    let h = handle.clone();
    let demo = handle.spawn_pinned(0, async move {
        let current = ShardCtx::current().expect("shard").shard_id();

        // Locality-first stealable task (preferred current shard).
        let preferred = h
            .spawn_stealable_on(current, async { "preferred" })
            .expect("spawn");

        // Plain stealable task (runtime picks preferred shard via round-robin).
        let rr_stealable = h.spawn_stealable(async { "rr_stealable" }).expect("spawn");

        let _ = preferred.await.expect("join");
        let _ = rr_stealable.await.expect("join");
    })?;

    demo.await?;
    Ok(())
}
```

What happens here:

1. The pinned dispatcher on shard `0` submits two stealable tasks.
2. `spawn_stealable_on(current, ...)` queues work to shard `0` as its preferred
   start shard.
3. `spawn_stealable(...)` queues work with a preferred shard selected by the
   runtime's round-robin submission cursor.
4. For each target shard, Spargio coalesces wake signals and sends a `msg_ring`
   doorbell if needed.
5. Each shard drains local stealable queued work first.
6. If a shard has spare budget and local queued work is low, it probes other
   shards and steals only when the locality-vs-migration gate says stealing is
   worth it (from the victim queue tail on the default deque backend).

That is why `StealablePreferred` is usually the default: locality first, with
stealing only when imbalance justifies migration cost.

## Cross-Shard Paths: `msg_ring` + Userspace Queues

### 1) Direct `msg_ring` messages

Used for immediate cross-shard signaling where each message is submitted
directly as `IORING_OP_MSG_RING` (for example direct raw sends).

- lowest indirection
- good for low-volume, latency-sensitive control traffic
- more ring traffic if used for high-volume payload delivery

### 2) Userspace payload queues + `msg_ring` doorbells

Used for nowait/batch paths.

- sender enqueues payloads into a userspace queue keyed by source/target shard
- sender sends one `msg_ring` doorbell when queue transitions to non-empty
- receiver gets the doorbell CQE, then drains many queued payloads in one pass

This reduces ring submission pressure and improves batching under bursty load.

### 3) Stealable task inboxes + coalesced wakes

`spawn_stealable(...)` and `spawn_stealable_on(...)` enqueue pending tasks into
per-shard stealable inboxes (userspace queues), then trigger wakeups (coalesced
when possible).

- local shard drains its own stealable inbox first
- other shards can steal from victim inboxes when heuristics allow
- migration happens for queued stealable tasks, not pinned tasks

## Where Command Channels Fit

Not all cross-shard actions go through `msg_ring`. Control-plane commands (for
example, spawn requests from outside shard context) can enter through runtime
command channels. Once on shard threads, local command draining typically uses
the queue + `msg_ring` paths above for fast wake/dispatch.

## What To Observe While Tuning

```rust
#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) -> Result<(), spargio::RuntimeError> {
    let stats = handle.stats_snapshot();
    println!(
        "local_hit_ratio={:.2} steal_success_rate={:.2} ring_msgs_submitted={} ring_msgs_backpressure={}",
        stats.local_hit_ratio(),
        stats.steal_success_rate(),
        stats.ring_msgs_submitted,
        stats.ring_msgs_backpressure,
    );
    Ok(())
}
```

What this does:

- reads locality and steal-success effectiveness.
- shows how much ring messaging traffic/backpressure is happening.
- gives a quick signal for whether placement or steal knobs need adjustment.
