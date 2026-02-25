# Rust Runtime Design Options (msg_ring-centric)

Goal: design a Rust async runtime inspired by `ourio`/`compio`/`glommio`/`monoio`, where `io_uring` `msg_ring` is the primary cross-ring signaling primitive.

## Baseline Invariants

- One ring owner per shard/thread (`!Send` local context).
- No direct cross-thread SQ submission.
- Cross-shard work enters a shard only via `msg_ring` completion.
- Cancellation and deadlines are first-class on every submitted op.
- Distinguish:
  - sender completion (`msg_ring` SQE done on source ring)
  - receiver delivery (message CQE observed on target ring)

## Option 1: Callback + Tagged Task (closest to ourio)

Surface:

- `ctx.submit(op, CallbackCtx { ptr, msg, cb }) -> TaskHandle`
- `ctx.msg_ring(target, target_task, send_ctx) -> TaskHandle`
- `TaskHandle::cancel()`, `TaskHandle::set_deadline(at)`

Pros:

- Very low overhead.
- Explicit and predictable control flow.
- Easy to map 1:1 to `io_uring` details.

Cons:

- Non-idiomatic Rust ergonomics.
- Harder composition versus `Future`.
- More user-level state machines.

Best for:

- Infrastructure code and advanced users optimizing latency tails.

## Option 2: Future-first Runtime with hidden msg_ring transport

Surface:

- `ctx.submit(op).await`
- `remote.send(msg).await`
- `channel::<T>().send_to(shard, value).await`

Pros:

- Familiar Rust API.
- Good composition with ecosystem futures.

Cons:

- Can hide shard crossing costs unless surfaced clearly.
- Harder to expose low-level completion semantics precisely.

Best for:

- Application teams that want productivity over maximal control.

## Option 3: Hybrid Two-Lane API (recommended)

Provide both:

- Lane A (raw): callback/tagged-task APIs for zero-allocation hot paths.
- Lane B (ergonomic): `Future` wrappers over the same core scheduler.

Sketch:

- `ShardCtx::submit_raw(op, tag, cb) -> OpToken`
- `ShardCtx::submit(op) -> OpFuture<T>`
- `RemoteShard::send_raw(tag, val) -> SendTicket`
- `RemoteShard::send<T: RingMsg>(msg) -> SendFuture`
- `msg::channel<T>()` implemented as payload queue + `msg_ring` doorbell

Pros:

- Preserves performance model and explicit semantics.
- Gives ergonomic API without sacrificing control.
- Clear migration path between "high-level" and "close to metal".

Cons:

- Larger API surface to maintain.

Best for:

- Runtime libraries intended for both systems and app users.

## Option 4: Actor-Only Runtime

Surface:

- Per-shard actor mailbox + typed messages.
- IO ops are actor methods, not general `submit(op)`.

Pros:

- Strong mental model.
- Eliminates many accidental cross-shard patterns.

Cons:

- Restrictive for low-level IO patterns.
- Less attractive for general-purpose runtime use.

Best for:

- Narrow domain runtimes with strict architecture constraints.

## Recommended Rust Interface

- `Runtime` with explicit shards.
- `ShardCtx` is `!Send`, acquired on a shard thread.
- `RemoteShard` is `Send + Sync` and only permits `msg_ring`-backed ingress.
- `OpHandle` supports:
  - `cancel()`
  - `set_deadline(Instant)`
  - `await`
- `Event` stream API for advanced users:
  - `Event::Io(...)`
  - `Event::RingMsg { from, tag, val }`

This keeps `ourio`'s design discipline while staying idiomatic in Rust.

## Can Rust Do Better?

Yes, in a few important ways:

- Stronger type safety:
  - typed message enums instead of ad-hoc integer tags
  - typed operation outputs (`Result<T, E>`) per op
- Better cancellation correctness:
  - structured `Drop` behavior on handles and futures
  - fewer accidental use-after-free patterns than raw pointer task passing
- Safer ownership for cross-ring payloads:
  - slab IDs / `Arc` envelopes / intrusive pools behind safe wrappers
- Better testing ergonomics:
  - deterministic mock backend with trait-based injection
- Better composability:
  - standard `Future`, `Stream`, and `AsyncRead/AsyncWrite` adapters

Where Rust is not automatically better:

- Allocation and abstraction overhead can regress latency if the API hides costs.
- Over-generalized async abstractions can obscure ring affinity and backpressure.

So the best Rust design is explicit about shard boundaries and keeps a raw fast path available.

## Practical Direction

Start with Option 3:

- Build raw core first (task records + msg_ring ingress + cancel/deadline).
- Add thin future wrappers second.
- Keep shard crossing explicit in types and function names.
