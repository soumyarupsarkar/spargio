# Implementation Log

## Snapshot (2026-02-25)

Repository state at start of this log:

- Git repo initialized in `/workspace/spargio`
- Initial implementation committed as:
  - `59d0b34` (`Implement sharded msg-ring-style runtime with TDD tests and benchmarks`)

## Completed So Far

### Design docs

- Added runtime design options:
  - `DESIGN_OPTIONS.md`

### Runtime crate

- Created crate:
  - `msg_ring_runtime`
- Implemented a sharded runtime with:
  - `RuntimeBuilder`, `Runtime`, `ShardCtx`, `RemoteShard`
  - `spawn_on` and `spawn_local`
  - `send_raw` and typed `send` via `RingMsg`
  - `next_event` event stream (`Event::RingMsg`)
  - sender completion tickets (`SendTicket`)

Current backend in this snapshot:

- In-process queue-based message transport (useful as baseline/fallback and for comparative benchmarking).

### TDD tests

- Added API/behavior tests in `tests/runtime_tdd.rs`:
  - local spawn runs on shard
  - raw send delivers to target with sender shard id
  - typed send round-trips through event path

Workflow used:

- Red: tests failed on placeholder API
- Green: implemented runtime until tests passed

### Benchmarks

- Added Criterion benchmark:
  - `benches/ping_pong.rs`
- Includes:
  - runtime ping-pong
  - simple Tokio baseline
  - simple Glommio baseline (feature-gated)

Feature:

- `glommio-bench` enables Glommio benchmark code path on Linux.

## Validation Results

Executed and passing:

- `cargo test`
- `cargo bench --no-run`
- `cargo bench --no-run --features glommio-bench`

Short benchmark sample run completed:

- `msg_ring_runtime`: ~1.62 ms (sample config)
- `tokio_unbounded_channel`: ~1.53 ms (sample config)
- `glommio_simple`: ~3.77–4.47 ms (with `glommio-bench`)

Note:

- These are quick smoke numbers, not stable performance conclusions.

## Next Work (Requested)

- Add a Linux `io_uring` backend that uses `msg_ring` for cross-shard delivery.
- Keep current queue backend for comparative benchmarks and fallback behavior.
- Preserve existing API so both backends can be measured under similar workloads.

## Update: Linux io_uring Backend Added

Implemented after the snapshot above:

- Added runtime backend selector:
  - `BackendKind::Queue`
  - `BackendKind::IoUring`
- Added builder controls:
  - `RuntimeBuilder::backend(BackendKind)`
  - `RuntimeBuilder::ring_entries(u32)`
- Default backend remains:
  - `BackendKind::Queue`

### Backend behavior

- Queue backend:
  - existing in-process message transport path retained.
- io_uring backend (Linux):
  - each shard owns an `IoUring` instance.
  - `send_raw` issued from a shard thread is routed through the source shard ring using:
    - `IORING_OP_MSG_RING` (`opcode::MsgRingData` via `io-uring` crate)
  - target shard receives an event via ring completion and emits:
    - `Event::RingMsg { from, tag, val }`
  - sender ticket completion is tied to sender-ring completion CQE.
- External/non-shard callers:
  - still supported using queue injection fallback (kept intentionally for safety and portability).

### Runtime loop adjustments

- Added backend-aware loop behavior:
  - queue backend keeps timeout-driven idle wait.
  - io_uring backend prefers busy polling (`yield_now`) to avoid artificial millisecond latency.

### Tests

- Existing tests still pass.
- Added Linux-only backend test:
  - `io_uring_backend_delivers_message`
- Full test status:
  - `cargo test` passes.

### Benchmarks updated

- `benches/ping_pong.rs` now benchmarks:
  - `msg_ring_runtime_queue`
  - `msg_ring_runtime_io_uring` (only when backend init succeeds)
  - `tokio_unbounded_channel`
  - `glommio_simple` (with `glommio-bench` feature)

Validation:

- `cargo bench --no-run` passes
- `cargo bench --no-run --features glommio-bench` passes

Quick benchmark sample (short run config):

- `msg_ring_runtime_queue`: ~1.66-1.70 ms
- `msg_ring_runtime_io_uring`: ~0.60-0.72 ms
- `tokio_unbounded_channel`: ~1.49-1.58 ms
- `glommio_simple`: ~4.05-4.85 ms

## Update: Stricter Benchmark Suite

Implemented to improve comparability and isolate what is being measured:

- Switched to persistent harnesses for steady-state measurements.
- Added matched two-worker topology for baselines:
  - Tokio: dedicated runtime thread, two-worker message loop.
  - Glommio (`glommio-bench`): two executor threads with message channels.
- Added explicit benchmark groups:
  - `steady_ping_pong_rtt`
  - `steady_one_way_send_drain`
  - `cold_start_ping_pong`

### Metric definitions

- `steady_ping_pong_rtt`:
  - per-round request/ack round-trip latency over persistent workers.
- `steady_one_way_send_drain`:
  - repeated one-way sends followed by a flush barrier ack.
  - for `msg_ring_runtime`, this now uses a bounded send-ticket window (`SEND_WINDOW=64`) to avoid fully serial per-send awaiting while preserving backpressure.
  - for Tokio/Glommio channel sends, send completion is synchronous enqueue.
- `cold_start_ping_pong`:
  - includes harness/runtime construction and teardown each iteration.

### Safety constraints observed

- No machine-level or persistent system tuning performed.
- No CPU governor/turbo/IRQ/process-affinity changes applied.
- Benchmarks are runnable on standard developer machines.

### Validation

- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.
- Sample full run completed for non-Glommio path.
- Sample targeted run completed for Glommio path.

### Notes from latest tuning pass

- Updated runtime one-way harness from strict per-send await to windowed in-flight tickets.
- Targeted one-way io_uring sample improved from roughly `~1.44 ms` to `~1.17 ms` under short Criterion settings.
