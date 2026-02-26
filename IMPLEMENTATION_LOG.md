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
  - `spargio`
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

- `spargio`: ~1.62 ms (sample config)
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
  - `spargio_queue`
  - `spargio_io_uring` (only when backend init succeeds)
  - `tokio_unbounded_channel`
  - `glommio_simple` (with `glommio-bench` feature)

Validation:

- `cargo bench --no-run` passes
- `cargo bench --no-run --features glommio-bench` passes

Quick benchmark sample (short run config):

- `spargio_queue`: ~1.66-1.70 ms
- `spargio_io_uring`: ~0.60-0.72 ms
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
  - for `spargio`, this now uses a bounded send-ticket window (`SEND_WINDOW=64`) to avoid fully serial per-send awaiting while preserving backpressure.
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

## Update: Send Path Optimizations (Proceed Phase)

Implemented next optimization wave:

- Added no-ticket send APIs:
  - `RemoteShard::send_raw_nowait(tag, val)`
  - `RemoteShard::send_nowait(msg)`
  - `ShardCtx::send_raw_nowait(target, tag, val)`
- Added shard-local fast path:
  - local sends now enqueue into a local per-shard queue (`LocalCommand`) and no longer bounce through the shard command channel.
- Added io_uring batching:
  - deferred `ring.submit()` with batched flush (`IOURING_SUBMIT_BATCH=64`)
  - flush on poll/reap and on SQ pressure.
- Added io_uring no-ticket CQE suppression:
  - uses `IORING_MSG_RING_CQE_SKIP` flag value for no-ticket `msg_ring` sends to avoid sender-CQ flooding.

### Benchmark harness alignment updates

- Runtime one-way benchmark now uses `send_raw_nowait` for fire-and-drain semantics.
- io_uring steady one-way harness uses larger ring entries (`4096`) to avoid CQ overflow in high-burst synthetic load.
- Cold-start io_uring path kept at default ring sizing to keep init broadly reliable on dev machines.

### Additional test coverage

- Added test:
  - `send_raw_nowait_delivers_event`

### Current quick sample numbers (50ms warmup/50ms measure)

- `steady_ping_pong_rtt/spargio_queue`: ~`1.47-1.51 ms`
- `steady_ping_pong_rtt/spargio_io_uring`: ~`336-348 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.21-1.34 ms`
- `steady_one_way_send_drain/spargio_queue`: ~`1.25-1.27 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`232-234 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`69-71 us`

## Update: Fast-Path Checklist Pass (Current)

Requested optimization checklist from the prior analysis and status:

- Doorbell + payload queue batching for io_uring no-ticket sends:
  - Implemented.
  - No-ticket sends now enqueue payloads into per `(target, source)` shared queues and only emit a `msg_ring` doorbell when transitioning empty -> non-empty.
- `send_many_nowait` API:
  - Implemented.
  - Added:
    - `RemoteShard::send_many_raw_nowait`
    - `RemoteShard::send_many_nowait`
    - `ShardCtx::send_many_raw_nowait`
    - `ShardCtx::send_many_nowait`
- Explicit flush API:
  - Implemented.
  - Added:
    - `ShardCtx::flush() -> SendTicket`
    - `RemoteShard::flush() -> SendTicket` (no-op success outside shard context)
  - io_uring implementation flushes pending submissions and uses a `NOP` completion barrier.
- Send waiter structure (`HashMap -> slab`):
  - Implemented.
  - Waiters are now stored in `Slab`, with completion `user_data` carrying slab index.
- Optional io_uring setup knobs (SQPOLL path):
  - Implemented on Linux builder:
    - `io_uring_sqpoll(Option<u32>)`
    - `io_uring_sqpoll_cpu(Option<u32>)`
    - `io_uring_single_issuer(bool)`
    - `io_uring_coop_taskrun(bool)`
- EventState lock removal (`Mutex -> RefCell`):
  - Not applied.
  - Reason: current `spawn_on` API requires `Send` futures; making event state shard-local `Rc<RefCell<...>>` makes `NextEvent` non-`Send`, which breaks valid `spawn_on` usage.

### Correctness note on CQE suppression

- Previous pass used `IORING_MSG_RING_CQE_SKIP` under the assumption it only removed sender-side completions.
- This pass corrected no-ticket suppression to use SQE `SKIP_SUCCESS` for source CQE suppression while preserving receiver delivery.

### Additional tests added

- `send_many_raw_nowait_delivers_in_order`
- `flush_completes_without_messages`
- `io_uring_send_many_nowait_delivers_messages`

### Validation

- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

### Latest quick benchmark sample (50ms warmup/50ms measure)

- `steady_ping_pong_rtt/spargio_queue`: ~`1.36-1.39 ms`
- `steady_ping_pong_rtt/spargio_io_uring`: ~`365-370 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.23-1.31 ms`
- `steady_one_way_send_drain/spargio_queue`: ~`1.23-1.25 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`62.8-64.5 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`69.0-72.7 us`
- `cold_start_ping_pong/spargio_queue`: ~`2.39-2.40 ms`
- `cold_start_ping_pong/spargio_io_uring`: ~`255-276 us`
- `cold_start_ping_pong/tokio_two_worker`: ~`453-484 us`

## Update: Tokio Batched One-Way Controls

To make the one-way comparison fairer, added additional Tokio benchmarks that batch payloads before crossing threads:

- `steady_one_way_send_drain/tokio_two_worker_batched_64`
- `steady_one_way_send_drain/tokio_two_worker_batched_all`

Implementation notes:

- Added `TokioWire::OneWayBatch(Vec<u32>)`.
- Added `TokioCmd::OneWayBatched { rounds, batch, reply }`.
- Existing `tokio_two_worker` remains unchanged as the per-message baseline.

Quick sample (50ms warmup/50ms measure):

- `steady_one_way_send_drain/spargio_io_uring`: ~`64.2-65.4 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`83.7-96.0 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_64`: ~`23.3-25.3 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_all`: ~`14.9-15.7 us`

Interpretation:

- The previous Tokio gap was largely due to per-send cross-thread signaling overhead, not an inherent runtime scheduler limit.
- With batching, Tokio is substantially faster on this one-way synthetic workload.

## Update: Disk IO Benchmark (4K Read RTT)

Added a dedicated disk benchmark:

- New bench target:
  - `benches/disk_io.rs`
- Cargo bench config:
  - `[[bench]] name = "disk_io" harness = false`

### Benchmark shape

- Persistent fixture file:
  - 16 MiB (`4096 * 4 KiB`) temp file under system temp dir.
- Metric:
  - `disk_read_rtt_4k` (per-iteration round-trip for `256` 4 KiB reads).
- Compared paths:
  - `tokio_two_worker_pread`
    - two-worker Tokio runtime
    - request/ack over Tokio unbounded channels
    - worker performs `pread` (`FileExt::read_at`)
  - `io_uring_msg_ring_two_ring_pread` (Linux)
    - two rings (`client` + `worker`)
    - request/ack over `IORING_OP_MSG_RING`
    - worker performs `IORING_OP_READ` and replies via `msg_ring`

### Quick sample (50ms warmup/50ms measure)

- `disk_read_rtt_4k/tokio_two_worker_pread`: ~`1.71-1.91 ms`
- `disk_read_rtt_4k/io_uring_msg_ring_two_ring_pread`: ~`2.64-3.09 ms`

### Notes

- This first disk RTT harness is not yet optimized for io_uring throughput; it is currently request/ack serialized and favors simplicity/debuggability.
- VFS work is still present for both paths; `io_uring` changes submission/completion mechanics, not filesystem lookup/permission/page-cache semantics.

## Update: Tokio Interop API Slice (TDD)

Started implementation toward the ADR with a first interop slice focused on submission APIs that can be called from Tokio tasks.

### Red phase

Added failing tests in `tests/tokio_compat_tdd.rs` for:

- `Runtime::handle()` availability.
- `RuntimeHandle::spawn_pinned(shard, fut)` execution on requested shard.
- `RuntimeHandle::spawn_stealable(fut)` round-robin placement.
- `RuntimeHandle` usage from Tokio tasks, including remote send + ticket await.
- `RuntimeHandle` cloneability and `Send + Sync`.

### Green phase

Implemented in `src/lib.rs`:

- New public `RuntimeHandle` (`Clone`, `Send + Sync`).
- `Runtime::handle() -> RuntimeHandle`.
- `RuntimeHandle` APIs:
  - `backend()`
  - `shard_count()`
  - `remote(shard)`
  - `spawn_pinned(shard, fut)`
  - `spawn_stealable(fut)` (round-robin via `AtomicUsize`)
- Refactored spawn logic into shared helper:
  - `spawn_on_shared(...)`

Validation:

- `cargo test` passes (including new `tokio_compat_tdd` tests).
- `cargo bench --no-run` passes.

## Update: Tokio-Compat POLL_ADD Reactor Scaffold (TDD)

Implemented the first compatibility-reactor scaffold behind feature gating.

### Red phase

Added failing tests in `tests/tokio_poll_reactor_tdd.rs` (`cfg(all(feature = "tokio-compat", target_os = "linux"))`) for:

- `PollReactor::register(..., PollInterest::Readable)` receives readable event.
- `PollReactor::deregister(token)` returns `NotFound` on second deregister.
- Token uniqueness across registrations.

### Green phase

Implemented new module in `src/lib.rs`:

- `tokio_compat` (Linux + feature gated):
  - `PollReactor`
  - `PollInterest`
  - `PollToken`
  - `PollEvent`
  - `PollReactorError`
- Uses `IORING_OP_POLL_ADD` for registration and `IORING_OP_POLL_REMOVE` for deregistration.
- Includes minimal completion routing and internal completion tagging for deterministic deregister behavior.

Cargo feature updates (`Cargo.toml`):

- Added features:
  - `tokio-compat`
  - `uring-native`
- Added Linux dependency:
  - `libc`

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Current Status: Tokio-Uring Alternative Scope

Snapshot of what is implemented vs remaining for the target architecture (`msg_ring` + poll-compat + work-stealing + native fast lane):

### Implemented

- Core `msg_ring` runtime and Linux `io_uring` backend.
- Tokio interop handle APIs:
  - `Runtime::handle()`
  - `spawn_pinned(...)`
  - `spawn_stealable(...)` (current policy: round-robin placement).
- `tokio-compat` lane scaffold:
  - `PollReactor` (`IORING_OP_POLL_ADD` / `IORING_OP_POLL_REMOVE`)
  - async `TokioPollReactor`
  - `TokioCompatLane` via `RuntimeHandle::tokio_compat_lane(...)`
  - lane readiness helpers: `wait_readable(fd)`, `wait_writable(fd)`.
- Cancellation cleanup and active-token tracking for poll registrations.
- TDD coverage for all above in:
  - `tokio_compat_tdd.rs`
  - `tokio_poll_reactor_tdd.rs`
  - `tokio_poll_async_tdd.rs`
  - `tokio_runtime_lane_tdd.rs`
  - `tokio_runtime_wait_tdd.rs`

### Remaining

- True work-stealing scheduler:
  - per-worker deque + global injector + steal loop (not implemented yet).
- Submission-time stealing/placement policy for native I/O work (not implemented yet).
- Poll-compat path integrated into shard driver with `msg_ring` doorbells:
  - current poll path uses dedicated reactor worker thread + command channel.
- `uring-native` fast lane:
  - feature flag exists, but native async API surface is not implemented yet.
- Tokio-like compatibility wrappers (`AsyncRead`/`AsyncWrite`) are not implemented yet.
- Full stress/race suite for rearm/cancel/drop edge cases under load is not complete yet.
- Compat-vs-native and mixed-load stealing benchmark suite is not complete yet.

## Proposed Sequence: Functional Slices First

Priority order to ship usable slices earlier:

1. Compat ergonomics slice:
   - stabilize `tokio-compat` lane ergonomics and add simple compatibility wrappers.
2. Native fast-lane MVP:
   - add first `uring-native` read/write APIs with pinned submission.
3. Mixed-mode app slice:
   - make compat and native lanes easy to combine in one app.
4. Submission-time placement policies:
   - add `round_robin`, `sticky`, and explicit shard placement options.
5. True work-stealing scheduler:
   - introduce per-worker deque + global injector + steal loop for stealable tasks.
6. Poll path re-home to shard driver:
   - move poll processing into shard driver path with `msg_ring` wakeups.
7. Hardening and benchmark gate slice:
   - race stress tests + mixed-load benchmark gates.

User stories unlocked after each slice:

1. After compat ergonomics:
   - migrate Tokio readiness-style code with minimal rewrites.
2. After native fast-lane MVP:
   - move only hot I/O paths to native `io_uring` APIs.
3. After mixed-mode:
   - run compatibility code and native ops side by side.
4. After placement policies:
   - control locality/load-balance at submission time.
5. After true work-stealing:
   - auto-balance CPU/control tasks while keeping I/O ring-affine.
6. After poll re-home:
   - reduce poll-path overhead without API changes.
7. After hardening/bench gates:
   - rely on correctness/perf regression protection in CI.

## User Stories Already Possible

With current implementation, users can already:

1. Build and run a sharded runtime with queue or Linux `io_uring` backend.
2. Send typed/raw shard-to-shard messages and await sender tickets.
3. Use no-ticket batched message sends and explicit flush barriers.
4. Spawn pinned or round-robin stealable tasks from Tokio tasks via `RuntimeHandle`.
5. Create a `tokio-compat` lane and use poll registration (`POLL_ADD`/`POLL_REMOVE`) through:
   - direct poll API (`register`, `wait_one`, `deregister`)
   - lane helpers (`wait_readable`, `wait_writable`).
6. Cancel readiness waits without leaking poll registrations (covered by tests).
7. Benchmark message RTT/one-way/cold-start and run a first disk I/O RTT comparison harness.

## Update: Compat Ergonomics Slice (TDD)

Implemented the next functional slice aimed at easier migration ergonomics for readiness-style code.

### Red phase

Added failing tests in `tests/tokio_compat_fd_tdd.rs` (`cfg(all(feature = "tokio-compat", target_os = "linux"))`) for:

- lane-scoped compatibility FD wrapper creation.
- wrapper `writable().await` and `readable().await` behavior.
- wrapper cloneability and FD identity access.

### Green phase

Implemented in `src/lib.rs`:

- New `CompatFd` type (`Clone`) under `tokio-compat`:
  - stores `TokioCompatLane` + `RawFd`.
- New lane factory:
  - `TokioCompatLane::compat_fd(fd) -> CompatFd`
- Wrapper methods:
  - `fd()`
  - `readable().await`
  - `writable().await`

This reuses the lane's cancellation-safe wait logic and poll token cleanup.

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Update: Async Tokio Poll Wrapper (TDD)

Added a Tokio-usable async wrapper over the `POLL_ADD` scaffold to allow direct use from Tokio tasks.

### Red phase

Added failing tests in `tests/tokio_poll_async_tdd.rs` (`cfg(all(feature = "tokio-compat", target_os = "linux"))`) for:

- async `wait_one()` returning readable events.
- async `deregister()` reporting `NotFound` on second remove.

### Green phase

Implemented in `src/lib.rs` (`tokio_compat` module):

- `TokioPollReactor` (`Clone`) wrapping `PollReactor` in `Arc<Mutex<_>>`.
- Methods:
  - `new(entries)`
  - `register(fd, interest)`
  - `wait_one().await`
  - `deregister(token).await`
- Async methods use `tokio::task::spawn_blocking` to execute blocking ring wait/remove logic safely off async worker threads.

Feature/dependency update:

- `tokio-compat` now enables optional Tokio dependency (`dep:tokio`).

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Update: Tokio Compat Lane via RuntimeHandle (TDD)

Integrated poll-compat usage into a runtime-lane API so Tokio tasks can use a single handle for both runtime operations and readiness waiting.

### Red phase

Added failing tests in `tests/tokio_runtime_lane_tdd.rs` (`cfg(all(feature = "tokio-compat", target_os = "linux"))`) for:

- `RuntimeHandle::tokio_compat_lane(entries)` creation.
- Combined lane behavior:
  - `spawn_pinned`
  - `remote(...).send_raw(...).await`
  - event receive path
- Poll API through lane:
  - `register`
  - async `wait_one`

### Green phase

Implemented in `src/lib.rs`:

- `RuntimeHandle::tokio_compat_lane(entries) -> Result<TokioCompatLane, PollReactorError>`
- New `TokioCompatLane` (`Clone`) with delegated runtime APIs:
  - `backend`
  - `shard_count`
  - `remote`
  - `spawn_pinned`
  - `spawn_stealable`
- Lane poll APIs:
  - `register`
  - async `wait_one`
  - async `deregister`

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Update: Lane Readiness Futures + Cancellation Cleanup (TDD)

Implemented lane-scoped readiness waits and fixed cancellation behavior.

### Red phase

Added failing tests in `tests/tokio_runtime_wait_tdd.rs` (`cfg(all(feature = "tokio-compat", target_os = "linux"))`) for:

- `wait_writable(fd)` and `wait_readable(fd)` APIs through `TokioCompatLane`.
- cancellation cleanup:
  - aborting `wait_readable` should not leak poll registrations.

### Green phase

Implemented in `src/lib.rs`:

- `TokioCompatLane` readiness methods:
  - `wait_readable(fd).await`
  - `wait_writable(fd).await`
- Drop cleanup guard for wait futures:
  - best-effort deregistration on cancellation.
- Debug helper for validation:
  - `debug_poll_registered_count()`.

Important fix during this slice:

- Reworked `TokioPollReactor` implementation from `spawn_blocking + Mutex<PollReactor>` to a dedicated worker-thread command loop.
- Reason:
  - prior design could deadlock cleanup when aborted tasks left blocking waits holding the mutex.
- New design:
  - command channel (`register` / `wait_one` / `deregister`)
  - non-blocking waiter pump (`try_wait_one`) to keep deregistration responsive.

Additional reactor hardening:

- Track active poll tokens in `PollReactor`.
- Ignore stale completions for inactive tokens.
- Fast `NotFound` on deregister for unknown token.

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Recap: Requested Slice Sequence and Status (2026-02-26)

Per the requested "functional slices first" plan, the sequence and current status are:

1. Compat ergonomics slice: `completed`.
2. Native fast-lane MVP slice: `completed` (this update).
3. Mixed-mode app slice: `partially completed` (compat + native lanes both exist; additional app-level helpers still pending).
4. Submission-time placement policies: `not started`.
5. True work-stealing scheduler: `not started`.
6. Poll path re-home to shard driver + `msg_ring` wakeups: `not started`.
7. Hardening + benchmark gate slice: `in progress` (coverage exists, full stress/benchmark gates pending).

## Update: Compat Stream Wrappers (TDD)

Extended compat ergonomics with Tokio `AsyncRead`/`AsyncWrite` wrappers for easier migration from socket-like code.

### Red phase

Added failing tests:

- `tests/tokio_compat_stream_tdd.rs`
  - `compat_stream_fd_reads_and_writes`
  - `compat_stream_fd_pending_read_wakes_on_write`
- `tests/tokio_compat_stream_hardening_tdd.rs`
  - `compat_fd_into_stream_reads_bytes`
  - `compat_stream_reads_eof_as_zero`
  - `lane_compat_stream_helper_wraps_asrawfd`

### Green phase

Implemented in `src/lib.rs` (Linux + `tokio-compat`):

- `CompatStreamFd` wrapper.
- `TokioCompatLane::compat_stream_fd(fd)`.
- `TokioCompatLane::compat_stream<T: AsRawFd>(&T)`.
- `CompatFd::into_stream()`.
- `AsyncRead`/`AsyncWrite` impls for `CompatStreamFd` using:
  - nonblocking `libc::read`/`libc::write`
  - lane readiness waits (`wait_readable`/`wait_writable`) on `WouldBlock`.
- helper utilities:
  - `set_nonblocking(fd)`
  - poll-error -> `std::io::Error` mapping.

Validation:

- `cargo test --features tokio-compat` passes.
- `cargo test` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Update: `uring-native` Fast-Lane MVP (TDD)

Implemented first native lane API for direct `io_uring` read/write-at operations with pinned shard submission.

### Red phase

Added failing tests in `tests/uring_native_tdd.rs` (`cfg(all(feature = "uring-native", target_os = "linux"))`):

- `uring_native_lane_requires_io_uring_backend`
- `uring_native_lane_reads_file_at_offset`
- `uring_native_lane_writes_file_at_offset`

### Green phase

Implemented in `src/lib.rs` (Linux + `uring-native`):

- `RuntimeHandle::uring_native_lane(shard) -> Result<UringNativeLane, RuntimeError>`.
- `UringNativeLane` API:
  - `read_at(fd, offset, len).await -> io::Result<Vec<u8>>`
  - `write_at(fd, offset, buf).await -> io::Result<usize>`
  - `shard()`.
- `TokioCompatLane::uring_native_lane(shard)` bridge (when both `tokio-compat` and `uring-native` features are enabled).
- Native op command plumbing from shard tasks to backend.
- `IoUringDriver` native op tracking/completion with `IORING_OP_READ` and `IORING_OP_WRITE`.
- Completion demuxing for native op user-data and cleanup on shutdown/error paths.

Notes:

- Native lane currently uses pinned submission through shard-local command flow.
- Queue backend intentionally returns `UnsupportedBackend` for native lane creation.

Validation:

- `cargo test` passes.
- `cargo test --features tokio-compat` passes.
- `cargo test --features uring-native` passes.
- `cargo test --features "tokio-compat uring-native"` passes.
- `cargo bench --no-run` passes.
- `cargo bench --no-run --features glommio-bench` passes.

## Revised Task List: Value Proposition Execution (2026-02-26)

Revised priority list aligned to the current project premise.

Update:

- reordered for faster proof generation.
- benchmark evidence is moved near the front so we validate value earlier.

- Core premise:
  - deliver a differentiated `io_uring` runtime centered on `msg_ring`-based cross-shard coordination and work-stealing.
- Not the core premise:
  - broad Tokio drop-in compatibility across dependency internals.

### Slice 1: Compatibility De-Scoping

Goal:

- remove or deprecate `tokio-compat` paths as active project focus.
- retain only interop boundaries needed for mixed-mode deployment.

Done criteria:

- code/docs/feature flags no longer present `tokio-compat` as strategic direction.
- README + ADRs + crate feature docs reflect runtime-first focus.

Validation gate:

- `cargo test`
- `cargo test --features uring-native`
- `cargo bench --no-run`

### Slice 2: Benchmark MVP Harness (Early Proof)

Goal:

- add the first coordination-heavy benchmark harness early:
  - intra-request fan-out/fan-in
  - shard-skew scenarios
  - mixed control/CPU + ring-affine I/O path.

Done criteria:

- reproducible harness exists and can run quickly on dev machines.
- first p50/p95/p99 + throughput-at-SLO snapshots are recorded.

Validation gate:

- benchmark smoke run in local workflow.
- `cargo bench --no-run` remains green.

### Slice 3: Placement Policy MVP

Goal:

- implement policy-driven submission placement needed by the benchmark:
  - explicit shard
  - sticky-key routing
  - policy round-robin.

Done criteria:

- public APIs expose placement policy selection.
- deterministic tests verify routing behavior.

Validation gate:

- placement policy tests + no regression in existing send/flush tests.

### Slice 4: True Work-Stealing MVP

Goal:

- replace spawn-time round-robin with true stealing mechanics:
  - per-worker deque
  - global injector
  - steal loop with cooperative budgeting.

Done criteria:

- stealable tasks move under load/skew.
- pinned/ring-affine tasks remain protected.

Validation gate:

- scheduler TDD for steal/no-steal invariants and skew behavior.

### Slice 5: Ring-Affine Native I/O Enforcement

Goal:

- make ring-affinity guarantees explicit in runtime state transitions.

Done criteria:

- in-flight native I/O cannot migrate across shards.
- cancellation and completion paths preserve ownership invariants.

Validation gate:

- race/cancel/drop tests for native I/O ownership safety.

### Slice 6: `msg_ring` Transport Hardening

Goal:

- harden coordination path under load:
  - batching behavior
  - doorbell policy
  - SQ/CQ pressure handling.

Done criteria:

- overload behavior is well-defined and tested.
- transport metrics (drops/retries/backpressure) are surfaced.

Validation gate:

- stress tests with bounded memory and deterministic failure semantics.

### Slice 7: Mixed-Runtime Boundary API Hardening

Goal:

- define robust communication contracts between `spargio` and host runtimes (Tokio or others):
  - bounded request/reply channels
  - backpressure semantics
  - cancellation and deadline propagation.

Done criteria:

- boundary API is explicit and documented.
- tests cover cancellation, timeout, and overload behavior.

Validation gate:

- boundary TDD suite (correctness + cancellation + overload).
- existing core tests remain green.

### Slice 8: Observability and Operator Signals

Goal:

- expose metrics and debug hooks needed for production tuning.

Candidate signals:

- per-shard queue depth
- steal rate
- doorbell rate
- pending native ops
- timeout/cancel counters.

Done criteria:

- metrics API and/or tracing events documented and test-covered.

Validation gate:

- instrumentation tests + low-overhead checks in benchmark runs.

### Slice 9: CI Regression Gates

Goal:

- lock in correctness and performance trajectory.

Done criteria:

- mandatory correctness suites for scheduler/transport/native I/O invariants.
- perf guardrails for critical benchmark scenarios.

Validation gate:

- CI blocks regressions on defined thresholds.

### Slice 10: Reference Mixed-Mode Service + Benchmark Expansion

Goal:

- provide a small reference app showing Tokio + `spargio` mixed-runtime usage:
  - request fan-out into `spargio`
  - aggregation and response path
  - explicit cancellation/backpressure boundary.
- expand benchmark suite from MVP to release-grade scenarios and reporting.

Done criteria:

- runnable example with docs and benchmark entry point.
- linked from README as adoption blueprint.
- expanded benchmark scenarios tracked in log and docs.

Validation gate:

- example integration test + benchmark smoke pass.

## Update: Benchmark Review and Suite Refocus (2026-02-26)

Reviewed benchmark outputs against current value proposition (`io_uring` + `msg_ring` coordination + work-stealing trajectory), then refocused the suite.

### Latest quick benchmark sample (Criterion 50ms warmup / 50ms measure / 20 samples)

From `ping_pong`:

- `steady_ping_pong_rtt/spargio_io_uring`: ~`340-360 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.33-1.45 ms`
- `steady_ping_pong_rtt/spargio_queue`: ~`1.38-1.52 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`63-65 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`84-97 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_64`: ~`23-25 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_all`: ~`13-15 us`
- `cold_start_ping_pong/spargio_io_uring`: ~`255-288 us`
- `cold_start_ping_pong/tokio_two_worker`: ~`505-593 us`

From `disk_io`:

- `disk_read_rtt_4k/tokio_two_worker_pread`: ~`1.81-2.01 ms`
- `disk_read_rtt_4k/io_uring_msg_ring_two_ring_pread`: ~`2.54-2.83 ms`

### Interpretation

- Current value is strongest in control-path/message-path microbenchmarks for `io_uring` backend (`steady_ping_pong_rtt`, unbatched `steady_one_way_send_drain`).
- Batched Tokio one-way is still faster in that synthetic path, so batching-sensitive comparisons remain context, not headline.
- Current serialized disk RTT harness does not yet demonstrate `spargio` advantage.

### Benchmark taxonomy update

Primary KPI direction (to add/expand next):

- coordination-heavy fan-out/fan-in benchmarks with skew and tail-latency focus.

Context / microbench (kept):

- `steady_ping_pong_rtt`
- `steady_one_way_send_drain`

De-emphasized for value-prop claims:

- `cold_start_ping_pong`
- `tokio_two_worker_batched_*` (useful context, not primary proof)
- current `disk_read_rtt_4k` harness (until reworked beyond strict serialized request/ack)

### Glommio benchmark removal decision

Decision:

- remove Glommio comparison path for now.

Reason:

- not currently aligned with primary proof objective and adds maintenance noise.
- current harness shape is not the target benchmark niche for `spargio`.

Changes applied:

- removed Glommio benchmark harness/code from `benches/ping_pong.rs`.
- removed `glommio` dependency and `glommio-bench` feature from `Cargo.toml`.
- removed `glommio-bench` mention from README feature list.

Validation:

- `cargo test` passes.
- `cargo test --features uring-native` passes.
- `cargo bench --no-run` passes.

## Update: Tokio-Compat Removal + Fanout/Fan-in Benchmark MVP (2026-02-26)

Applied the scope change to fully de-emphasize drop-in Tokio emulation and move proof work to coordination-heavy fan-out/fan-in benchmarks.

### Tokio-compat removal (code + tests)

Changes:

- removed `tokio-compat` feature flag from `Cargo.toml`.
- removed optional non-dev Tokio dependency from `[dependencies]`.
- removed all `tokio-compat` lane and poll-emulation code from `src/lib.rs`:
  - deleted `tokio_compat` module.
  - deleted `RuntimeHandle::tokio_compat_lane(...)`.
  - deleted `TokioCompatLane`, `CompatFd`, `CompatStreamFd`, and associated helpers.
- removed compat-only TDD files:
  - `tests/tokio_compat_fd_tdd.rs`
  - `tests/tokio_compat_stream_tdd.rs`
  - `tests/tokio_compat_stream_hardening_tdd.rs`
  - `tests/tokio_poll_reactor_tdd.rs`
  - `tests/tokio_poll_async_tdd.rs`
  - `tests/tokio_runtime_lane_tdd.rs`
  - `tests/tokio_runtime_wait_tdd.rs`
- renamed remaining Tokio interoperability coverage from `tests/tokio_compat_tdd.rs` to `tests/tokio_interop_tdd.rs` for clearer intent.

### New benchmark: fan-out/fan-in with skew

Added `benches/fanout_fanin.rs` and registered it in `Cargo.toml`.

Harness design:

- Same worker width on both runtimes (`4` threads/shards).
- Same workload model on both runtimes:
  - per-request spawn fan-out (`16` branches), then fan-in on join.
  - deterministic synthetic compute per branch.
- Two scenarios:
  - `fanout_fanin_balanced`: all branches equal work.
  - `fanout_fanin_skewed`: one hot branch per request has much heavier work.
- Bench variants:
  - `tokio_mt_4`
  - `spargio_queue`
  - `spargio_io_uring` (Linux)

### Quick MVP benchmark sample

Command:

- `cargo bench --bench fanout_fanin -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

Observed ranges:

- `fanout_fanin_balanced/tokio_mt_4`: ~`1.41-1.51 ms`
- `fanout_fanin_balanced/spargio_queue`: ~`10.7-18.1 ms`
- `fanout_fanin_balanced/spargio_io_uring`: ~`0.782-0.813 ms`
- `fanout_fanin_skewed/tokio_mt_4`: ~`2.34-2.40 ms`
- `fanout_fanin_skewed/spargio_queue`: ~`54.0-54.4 ms`
- `fanout_fanin_skewed/spargio_io_uring`: ~`1.882-1.889 ms`

### Validation

- `cargo fmt` passes.
- `cargo test` passes.
- `cargo test --features uring-native` passes.
- `cargo bench --no-run` passes (includes `fanout_fanin`).

## Direction Note: Full io_uring Runtime Scope (2026-02-26)

Long-term direction:

- evolve `spargio` toward a fuller `io_uring` runtime surface (disk + network I/O), comparable in scope to specialized runtimes.

Near-term priority remains unchanged:

- prove differentiated value first in `msg_ring`-coordinated cross-shard scheduling, placement, and work-stealing benchmarks.

Implication for sequencing:

- full disk/network API breadth is explicitly treated as a later expansion track after current scheduler/coordination milestones are validated.

## Update: Slice Execution MVP (Placement, Stealing, Boundary, CI, Reference App) (2026-02-26)

Executed the remaining planned slices in MVP form with red/green TDD coverage.

### Red-phase tests added

New failing suites introduced first:

- `tests/slices_tdd.rs`
  - placement policy routing (`Pinned`, `Sticky`)
  - stealable execution on non-preferred shard under load
  - runtime stats snapshot counters/shape
- `tests/boundary_tdd.rs`
  - bounded overload behavior (`Overloaded`)
  - blocking timeout behavior (`Timeout`)
  - cancellation-safe reply path (`Canceled`)
  - deadline metadata propagation

Then implementation was iterated until all tests passed.

### Slice 3: Placement policy MVP

Implemented:

- `TaskPlacement` enum:
  - `Pinned(ShardId)`
  - `RoundRobin`
  - `Sticky(u64)`
  - `Stealable`
  - `StealablePreferred(ShardId)`
- `RuntimeHandle::spawn_with_placement(...)`
- `RuntimeHandle::spawn_stealable_on(preferred_shard, ...)`

Notes:

- sticky placement uses stable key hashing to shard index.

### Slice 4: True work-stealing MVP

Implemented:

- global stealable injector channel (`StealableTask`) shared across shard workers.
- shard workers opportunistically drain stealable tasks and execute locally.
- preferred-shard hint tracking with `stealable_stolen` counter when execution shard differs from preferred shard.

Validation:

- `stealable_preferred_tasks_can_run_on_another_shard_under_load` now passes.

### Slice 5: Ring-affine native I/O enforcement

Implemented:

- native local commands now carry `origin_shard`.
- backend validates `origin_shard == current_shard` before submitting native ops.
- affinity violations increment `native_affinity_violations` and fail the operation.
- pending native-op gauge (`pending_native_ops`) is tracked.

### Slice 6: `msg_ring` transport hardening

Implemented:

- configurable `msg_ring_queue_capacity` on `RuntimeBuilder`.
- io_uring payload queues enforce bounded capacity.
- overload now reports `SendError::Backpressure` for saturated payload queues.
- backpressure counter surfaced via `ring_msgs_backpressure`.

### Slice 7: Mixed-runtime boundary API hardening

Implemented `spargio::boundary` module:

- bounded channel construction via `boundary::channel(capacity)`.
- client API:
  - `call(...)`
  - `try_call(...)`
  - `call_with_timeout(...)`
- server API:
  - `recv()`
  - `recv_timeout(...)`
- request API:
  - `request()`
  - `deadline()`
  - `respond(...)` (cancellation-safe)
- ticket API:
  - `Future` implementation
  - `wait_timeout_blocking(...)`

Error model:

- `BoundaryError::{Closed, Overloaded, Timeout, Canceled}`.

### Slice 8: Observability and operator signals

Implemented snapshot API:

- `RuntimeHandle::stats_snapshot() -> RuntimeStats`

Current signals:

- per-shard command depth (`shard_command_depths`)
- submitted pinned / stealable spawn counts
- stealable executed / stolen counts
- ring message submitted / completed / failed / backpressure counts
- native affinity violation count
- pending native-op gauge

### Slice 9: CI regression gates

Added:

- `.github/workflows/ci.yml` with gates for:
  - format check
  - tests
  - `uring-native` tests
  - `cargo bench --no-run`
  - fan-out benchmark smoke + guardrail scripts

Added scripts:

- `scripts/bench_fanout_smoke.sh`
- `scripts/bench_fanout_guardrail.sh`

### Slice 10: Reference mixed-mode service + benchmark expansion

Added:

- `examples/mixed_mode_service.rs`
  - Tokio-hosted request fan-out to `spargio` via boundary channel
  - stealable placement usage + aggregation response path
  - timeout-aware boundary call path

Benchmark update:

- `benches/fanout_fanin.rs` now records throughput units per group (`Throughput::Elements`).

### Validation

- `cargo test` passes.
- `cargo test --features uring-native` passes.
- `cargo bench --no-run` remains green.

## Update: Full Benchmark Snapshot Refresh (2026-02-26)

Captured a fresh baseline across all active benchmark suites after slice MVP implementation.

### Command profile

- `cargo bench --bench ping_pong -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench fanout_fanin -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench disk_io -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Observed ranges

From `ping_pong`:

- `steady_ping_pong_rtt/spargio_queue`: ~`1.37-1.42 ms`
- `steady_ping_pong_rtt/spargio_io_uring`: ~`353-380 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.41-1.51 ms`
- `steady_one_way_send_drain/spargio_queue`: ~`1.31-1.35 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`66.9-69.1 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`87.2-91.1 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_64`: ~`22.4-23.4 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_all`: ~`13.7-14.7 us`
- `cold_start_ping_pong/spargio_queue`: ~`2.43-2.44 ms`
- `cold_start_ping_pong/spargio_io_uring`: ~`242-264 us`
- `cold_start_ping_pong/tokio_two_worker`: ~`511-560 us`

From `fanout_fanin`:

- `fanout_fanin_balanced/tokio_mt_4`: ~`1.35-1.38 ms`
- `fanout_fanin_balanced/spargio_queue`: ~`3.80-4.10 ms`
- `fanout_fanin_balanced/spargio_io_uring`: ~`1.61-1.65 ms`
- `fanout_fanin_skewed/tokio_mt_4`: ~`2.39-2.59 ms`
- `fanout_fanin_skewed/spargio_queue`: ~`3.44-3.73 ms`
- `fanout_fanin_skewed/spargio_io_uring`: ~`1.99-2.00 ms`

From `disk_io`:

- `disk_read_rtt_4k/tokio_two_worker_pread`: ~`1.80-1.95 ms`
- `disk_read_rtt_4k/io_uring_msg_ring_two_ring_pread`: ~`2.61-2.78 ms`

### Readout

- `spargio_io_uring` is strongest in control-path RTT and cold-start latency.
- one-way unbatched send/drain favors `spargio_io_uring`, but batched Tokio remains significantly faster.
- skewed fan-out/fan-in currently favors `spargio_io_uring`.
- balanced fan-out/fan-in currently favors Tokio.
- current disk RTT harness remains a loss for the io_uring+msg_ring path.

## Update: msg_ring Stealable Dispatch + Benchmark Refresh (2026-02-26)

Implemented work-stealing data-path changes to align with project premise:

- replaced global stealable injector channel with per-shard stealable inboxes.
- changed stealable submit path to:
  1. choose target shard by inbox depth (submission-time decision),
  2. enqueue task into target inbox,
  3. wake target via `msg_ring` doorbell on `IoUring` backend.
- added wake plumbing:
  - `LocalCommand::SubmitStealableWake`
  - `Command::StealableWake`
  - backend `submit_stealable_wake(...)` path.
- kept queue-backend fallback wake semantics for non-io_uring runs.

TDD additions:

- added Linux io_uring slice test proving stealable dispatch submits ring wake traffic:
  - `tests/slices_tdd.rs::io_uring_stealable_dispatch_uses_msg_ring_wake`.

Validation:

- `cargo fmt`
- `cargo test`
- `cargo test --features uring-native`

Benchmark profile:

- `cargo bench --bench ping_pong -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench fanout_fanin -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench disk_io -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `./scripts/bench_fanout_guardrail.sh`

Observed ranges:

From `ping_pong`:

- `steady_ping_pong_rtt/spargio_io_uring`: ~`352-370 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.30-1.42 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`66.6-68.3 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`84.0-90.6 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_64`: ~`24.2-26.1 us`
- `steady_one_way_send_drain/tokio_two_worker_batched_all`: ~`14.4-15.7 us`
- `cold_start_ping_pong/spargio_io_uring`: ~`248-305 us`
- `cold_start_ping_pong/tokio_two_worker`: ~`500-555 us`

From `fanout_fanin`:

- `fanout_fanin_balanced/tokio_mt_4`: ~`1.43-1.51 ms`
- `fanout_fanin_balanced/spargio_io_uring`: ~`982-989 us`
- `fanout_fanin_skewed/tokio_mt_4`: ~`2.35-2.42 ms`
- `fanout_fanin_skewed/spargio_io_uring`: ~`1.92-1.93 ms`

From `disk_io`:

- `disk_read_rtt_4k/tokio_two_worker_pread`: ~`1.82-2.00 ms`
- `disk_read_rtt_4k/io_uring_msg_ring_two_ring_pread`: ~`2.52-2.74 ms`

Interpretation:

- value proposition now shows up directly in coordination-heavy fan-out/fan-in:
  - balanced and skewed scenarios both favor `spargio_io_uring`.
- compared with earlier same-day snapshot, `fanout_fanin_balanced` flipped from loss to win after the stealable dispatch changes.
- batched Tokio one-way throughput remains a known gap.
- disk RTT benchmark remains a known gap.
