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

## Roadmap: Toward Full Runtime Scope

Objective:

- evolve `spargio` into a fuller async runtime in the class of `glommio` / `monoio` / `compio`, while preserving the current differentiator (`msg_ring`-coordinated cross-shard scheduling + stealing).

Priority roadmap:

1. Lock the differentiator with stable KPI gates.
2. Build scheduler v2 (true per-worker deque stealing + fairness controls).
3. Complete core runtime primitives (timers, cancellation, task groups, backpressure semantics).
4. Deliver native network I/O MVP (TCP/UDP) on io_uring.
5. Deliver native filesystem I/O MVP with clear FD/buffer ownership and affinity rules.
6. Harden reliability and observability (stress/soak, failure injection, per-shard metrics and tracing).
7. Keep sidecar interop first-class; treat broad Tokio-compat readiness emulation as an optional long-term lane.

Immediate milestone sequence:

1. Deque-based stealing + fairness/budgeting.
2. Timer + timeout + cancellation primitives.
3. TCP MVP + dedicated latency/throughput/tail benchmarks.

## Update: Roadmap Tasks 1-5 MVP Implementation (TDD) (2026-02-26)

Implemented the first pass for roadmap tasks 1-5 with red/green TDD, then validated with tests and benchmark guardrails.

### 1) KPI gates for value proposition

Added benchmark guardrails/scripts:

- `scripts/bench_ping_guardrail.sh`
  - checks `steady_ping_pong_rtt`, unbatched `steady_one_way_send_drain`, and `cold_start_ping_pong` against Tokio ratio thresholds.
- `scripts/bench_kpi_guardrail.sh`
  - runs ping + fanout guardrails together.
- existing `scripts/bench_fanout_guardrail.sh` retained.

CI update:

- `.github/workflows/ci.yml` now runs:
  - fanout smoke
  - ping perf guardrail
  - fanout perf guardrail

### 2) Scheduler v2 (per-worker deque stealing + fairness controls)

Runtime changes:

- added `RuntimeBuilder::stealable_queue_capacity(...)`.
- added `RuntimeBuilder::steal_budget(...)`.
- changed stealable submission path:
  - submit to preferred shard deque (`StealablePreferred`) with bounded capacity.
  - return `RuntimeError::Overloaded` on enqueue backpressure.
- worker execution loop now:
  - drains local deque first up to budget.
  - attempts bounded victim steals via rotating cursor when local queue has room.

New stats signals:

- `stealable_backpressure`
- `steal_attempts`
- `steal_success`

### 3) Core runtime primitives (timer/cancellation/task groups/backpressure semantics)

Added:

- `sleep(Duration) -> impl Future<Output = ()>`
- `timeout(Duration, fut) -> Result<T, TimeoutError>`
- `CancellationToken` with:
  - `new()`
  - `cancel()`
  - `is_canceled()`
  - `cancelled() -> Future`
- `TaskGroup` with cooperative cancellation:
  - `TaskGroup::new(handle)`
  - `spawn_with_placement(...) -> TaskGroupJoinHandle<T>`
  - `cancel()`
  - `token()`

Backpressure semantics now include stealable task-queue overload via `RuntimeError::Overloaded`.

### 4) Native network I/O MVP (io_uring lane)

Extended `UringNativeLane` with:

- `recv(fd, len)`
- `send(fd, buf)`

Implemented via native io_uring ops:

- `IORING_OP_RECV`
- `IORING_OP_SEND`

### 5) Native filesystem I/O MVP (ownership + affinity surface)

Added:

- `UringNativeLane::fsync(fd)` (`IORING_OP_FSYNC`)
- `UringBoundFd` ownership wrapper bound to a lane/shard with methods:
  - `read_at`, `write_at`, `recv`, `send`, `fsync`
- binding helpers:
  - `bind_owned_fd`
  - `bind_file`
  - `bind_tcp_stream`
  - `bind_udp_socket`

This gives an explicit ownership + shard-affinity API surface for FD-driven native ops.

### Red/green tests added

- `tests/primitives_tdd.rs`
  - sleep timing
  - timeout success/failure
  - cancellation token notification
  - task-group cancellation and completion semantics
- `tests/slices_tdd.rs` additions
  - stealable queue backpressure -> `RuntimeError::Overloaded`
  - steal attempts/success stats under blocked-owner load
- `tests/uring_native_tdd.rs` additions
  - bound file write/read/fsync
  - bound TCP send/recv
  - bound UDP send/recv

### Validation

- `cargo fmt`
- `cargo test`
- `cargo test --features uring-native`
- `./scripts/bench_ping_guardrail.sh`
- `./scripts/bench_fanout_guardrail.sh`
- `cargo bench --bench disk_io -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Benchmark readout (latest local run profile)

From ping guardrail run:

- `steady_ping_pong_rtt/spargio_io_uring`: ~`363-380 us`
- `steady_ping_pong_rtt/tokio_two_worker`: ~`1.37-1.48 ms`
- `steady_one_way_send_drain/spargio_io_uring`: ~`73.0-75.3 us`
- `steady_one_way_send_drain/tokio_two_worker`: ~`104.6-115.8 us`
- `cold_start_ping_pong/spargio_io_uring`: ~`260-297 us`
- `cold_start_ping_pong/tokio_two_worker`: ~`463-511 us`

From fanout guardrail run:

- `fanout_fanin_balanced/tokio_mt_4`: ~`1.42-1.50 ms`
- `fanout_fanin_balanced/spargio_io_uring`: ~`1.33-1.35 ms`
- `fanout_fanin_skewed/tokio_mt_4`: ~`2.42-2.53 ms`
- `fanout_fanin_skewed/spargio_io_uring`: ~`2.03-2.04 ms`

From disk benchmark run:

- `disk_read_rtt_4k/tokio_two_worker_pread`: ~`1.79-1.93 ms`
- `disk_read_rtt_4k/io_uring_msg_ring_two_ring_pread`: ~`2.65-2.80 ms`

## Benchmark suite update: FS/Net API coverage and legacy disk bench removal

Implemented benchmark suite changes to align with current runtime API surface:

- removed legacy disk RTT benchmark harness:
  - deleted `benches/disk_io.rs`
  - removed `[[bench]] name = "disk_io"` from `Cargo.toml`
- added filesystem API benchmark suite:
  - `benches/fs_api.rs`
  - `fs_read_rtt_4k`:
    - `tokio_spawn_blocking_pread_qd1`
    - `spargio_uring_bound_file_qd1`
  - `fs_read_throughput_4k_qd32`:
    - `tokio_spawn_blocking_pread_qd32`
    - `spargio_uring_bound_file_qd32`
- added network API benchmark suite:
  - `benches/net_api.rs`
  - `net_echo_rtt_256b`:
    - `tokio_tcp_echo_qd1`
    - `spargio_uring_bound_tcp_qd1`
  - `net_stream_throughput_4k_window32`:
    - `tokio_tcp_echo_window32`
    - `spargio_uring_bound_tcp_window32`
- updated `Cargo.toml` benchmark targets:
  - `ping_pong`
  - `fanout_fanin`
  - `fs_api`
  - `net_api`

Validation run:

- `cargo fmt --all`
- `cargo bench --no-run`
- `cargo bench --no-run --features uring-native`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

Latest benchmark readout (short smoke profile):

From `fs_api`:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.59-1.68 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`1.98-2.11 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`7.66-7.76 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`7.51-8.23 ms`

From `net_api`:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`8.17-8.54 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`6.89-6.97 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`11.12-11.42 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`29.33-30.01 ms`

## Net benchmark tuning pass: reduce `net_stream_throughput_4k_window32` gap

Goal:

- reduce overhead in the `uring-native` TCP path and re-run `net_api` to improve `net_stream_throughput_4k_window32`.

Implemented runtime/API changes (`src/lib.rs`):

- added owned-buffer native APIs:
  - `UringNativeLane::recv_owned(fd, Vec<u8>) -> io::Result<(usize, Vec<u8>)>`
  - `UringNativeLane::send_owned(fd, Vec<u8>) -> io::Result<(usize, Vec<u8>)>`
  - `UringBoundFd::recv_owned(Vec<u8>) -> io::Result<(usize, Vec<u8>)>`
  - `UringBoundFd::send_owned(Vec<u8>) -> io::Result<(usize, Vec<u8>)>`
- kept existing convenience APIs by adapting through owned-buffer path:
  - `recv(fd, len)` now uses `recv_owned` + truncate
  - `send(fd, &[u8])` now uses `send_owned`
- added same-shard fast path in `recv_owned`/`send_owned`:
  - if called from matching runtime/shard context, enqueue native op directly to local command queue instead of spawning a new pinned task.
- wired owned-buffer request/response shapes through local command + backend + io_uring native op completion path.

TDD coverage:

- added `uring_bound_tcp_stream_supports_owned_send_and_recv_buffers` in `tests/uring_native_tdd.rs`.

Benchmark harness tuning (`benches/net_api.rs`):

- moved Spargio net workload execution into a pinned runtime worker task (command-driven harness), instead of issuing all ops from outside the runtime.
- switched throughput receive path to stream-byte draining with a reusable scratch buffer (`64 KiB`) for both Tokio and Spargio:
  - reduces per-op overhead and keeps the workload apples-to-apples as stream throughput.
- switched Spargio send path to owned-buffer reuse (`send_owned`) with fallback for partial sends.

Validation:

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

Result delta from this tuning pass:

- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: improved from ~`6.89-6.97 ms` to ~`5.46-5.70 ms`.
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: improved from ~`29.33-30.01 ms` to ~`12.96-13.16 ms`.

Current comparison (same run):

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.62-8.10 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`5.46-5.70 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.47-11.01 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`12.96-13.16 ms`

Interpretation:

- RTT is now clearly in Spargio’s favor for this harness.
- Stream throughput gap versus Tokio is substantially reduced (from ~2.6x slower to ~1.2x slower), but still present.

## Next optimization batch (committed plan before implementation)

Based on current net throughput gap, the next batch is:

1. Introduce provided-buffer multishot receive path (`IORING_OP_RECV_MULTISHOT` + `IORING_OP_PROVIDE_BUFFERS`) for stream receive-heavy benchmarks.
2. Expand reusable-buffer APIs (`recv_into`/owned-buffer reuse) so stream loops avoid per-op allocation churn.
3. Add batch-oriented stream APIs (`send_batch`, `recv_batch`/multishot helpers) to reduce per-message control overhead.
4. Increase pipelining depth in throughput paths by issuing batched/native operations with configurable in-flight windows.
5. Add an io_uring throughput preset (`single_issuer`, `coop_taskrun`, optional `sqpoll`) and use it in benchmark harnesses with fallback when unsupported.

Execution approach remains red/green TDD: add failing tests for each new API/behavior, then implement minimal passing behavior, then re-benchmark.

## Implementation: proposal batch (multishot/batching/tuning) completed

Implemented all items from the prior optimization proposal set.

### 1) Provided-buffer multishot receive path

Runtime additions (`src/lib.rs`):

- new local command: `SubmitNativeRecvMultishot`
- new native op state: `NativeIoOp::RecvMulti` (buffer group, target bytes, collected chunks)
- new driver path:
  - `submit_native_recv_multishot(...)`
  - submits `IORING_OP_PROVIDE_BUFFERS` + `IORING_OP_RECV_MULTISHOT`
  - collects CQEs until target bytes reached or stream ends
  - issues `IORING_OP_ASYNC_CANCEL` when target reached while CQE `MORE` continues
  - removes provided buffers via `IORING_OP_REMOVE_BUFFERS` on completion/failure
- completion path updated to process multishot/native housekeeping CQEs safely.

### 2) Reusable-buffer API expansion

Added:

- `UringNativeLane::recv_into(fd, Vec<u8>)`
- `UringBoundFd::recv_into(Vec<u8>)`

These preserve caller-owned buffers and avoid per-op allocation churn.

### 3) Batch-oriented stream APIs

Added:

- `UringNativeLane::send_batch(fd, Vec<Vec<u8>>, window)`
- `UringNativeLane::recv_batch_into(fd, Vec<Vec<u8>>, window)`
- `UringBoundFd::send_batch(...)`
- `UringBoundFd::recv_batch_into(...)`
- `UringNativeLane::recv_multishot(...)`
- `UringBoundFd::recv_multishot(...)`

### 4) Pipelining depth in throughput path

Benchmark harness updates (`benches/net_api.rs`):

- throughput send path now uses `send_batch` with reusable buffer pool.
- throughput receive path attempts `recv_multishot` first, then falls back to `recv_owned` if unsupported.
- this increases in-flight native work while keeping a fallback for older kernels.

### 5) io_uring throughput preset + harness usage

Runtime builder addition:

- `RuntimeBuilder::io_uring_throughput_mode(sqpoll_idle_ms)`
  - enables `coop_taskrun`
  - optional sqpoll setting through argument

Harness usage:

- `benches/fs_api.rs` and `benches/net_api.rs` now try throughput mode and fall back to plain io_uring runtime build if unavailable.

### Additional hardening done while implementing

- `flush_submissions()` now treats transient submit errors (`EAGAIN`/`EBUSY`/`Interrupted`) as retry/defer instead of immediate fatal teardown.
- this removed runtime cancellation failures seen under benchmark pressure.

### TDD additions

`tests/uring_native_tdd.rs` now includes:

- `uring_bound_tcp_stream_supports_recv_into_and_send_batch`
- `uring_bound_tcp_stream_supports_recv_multishot` (with unsupported-kernel fallback)

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest benchmark readout after this implementation batch

From `fs_api`:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.64-1.71 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`1.98-2.28 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`8.57-8.97 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`6.73-7.42 ms`

From `net_api`:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.92-8.35 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`5.57-5.88 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.93-11.85 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`11.92-12.28 ms`

Interpretation:

- proposal batch is functionally implemented end-to-end (APIs + runtime + tests + benches).
- stream throughput gap versus Tokio narrowed further while preserving RTT advantage.

## Next optimization batch: close net throughput gap vs Tokio

Goal:

- improve `net_stream_throughput_4k_window32` by reducing per-frame control-path overhead in Spargio’s native TCP path.

Planned items (to implement with red/green TDD):

1. True native send batching:
   - add a single-command native submit path for multiple sends (`send_batch_native`) instead of `join_all(send_owned(...))` fanout.
   - aggregate completions in-driver and reply once per batch.
2. Persistent multishot provided-buffer groups:
   - keep a reusable provided-buffer pool per fd/lane for throughput loops.
   - avoid `ProvideBuffers`/`RemoveBuffers` on every throughput batch.
3. Zero-copy-ish multishot completion path cleanup:
   - remove `chunks.clone()` completion duplication.
   - finish by moving accumulated chunks once.
4. Capability caching in benchmark/harness:
   - probe multishot support once and stop retrying unsupported ops each batch.
5. Stronger throughput semantics:
   - add `send_all_batch` behavior (or equivalent) so batch send handles partial writes without leaking throughput accounting.

## Implementation: net throughput optimization batch completed

Implemented all five planned items.

### 1) True native send batching

Runtime changes (`src/lib.rs`):

- new API:
  - `UringNativeLane::send_all_batch(fd, bufs, window)`
  - `UringBoundFd::send_all_batch(bufs, window)`
- `send_batch(...)` now delegates to `send_all_batch(...)`.
- new local command:
  - `SubmitNativeSendBatchOwned`
- new backend + driver path:
  - `ShardBackend::submit_native_send_batch(...)`
  - `IoUringDriver::submit_native_send_batch(...)`
- batch state and CQE handling:
  - `NativeSendBatch`
  - `NativeSendBatchPart`
  - `native_send_batches` + `native_send_parts`
  - `complete_native_send_batch_part(...)`
  - single batch reply channel per batch (not per send op).

### 2) Persistent multishot provided-buffer groups

Runtime changes (`src/lib.rs`):

- `NativeIoOp::RecvMulti` now references a pool key rather than owning temporary storage.
- new pool model:
  - `NativeRecvPoolKey`
  - `NativeRecvPool`
  - `native_recv_pools: HashMap<...>`
- multishot flow now:
  - registers provided buffers once per pool (`registered`).
  - reuses pool storage/group across calls.
  - reprovides consumed bids via `reprovide_multishot_buffers(...)`.
  - marks pool free via `mark_recv_pool_free(...)`.
  - removes all registered groups on driver shutdown.

### 3) Multishot completion path copy cleanup

- removed `chunks.clone()` completion duplication in `complete_native_op(...)`.
- completion now moves collected chunks with `std::mem::take(...)` when finishing multishot ops.

### 4) Capability caching in benchmark path

Benchmark changes (`benches/net_api.rs`):

- `spargio_echo_windowed(...)` now caches multishot support in-loop:
  - if `recv_multishot` returns `EINVAL` / `ENOSYS` / `EOPNOTSUPP`, disable further multishot attempts for the rest of the run.

### 5) Stronger send semantics (`send_all_batch`)

- `send_all_batch` tracks per-buffer progress and retries partial writes until each buffer is fully sent or an error occurs.
- benchmark throughput sender now uses `send_all_batch(...)` (full-send semantics).

### Red/Green TDD additions

Added tests first in `tests/uring_native_tdd.rs`, then implemented runtime until green:

- `uring_bound_tcp_stream_supports_send_all_batch`
- `uring_bound_tcp_stream_reuses_recv_multishot_path_across_calls`

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest benchmark readout after this batch

From `net_api`:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.62-8.07 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`5.26-5.70 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.42-10.73 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`11.02-11.16 ms`

From `fs_api`:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.60-1.75 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`1.85-1.92 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`7.51-7.62 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`6.40-6.96 ms`

Interpretation:

- net throughput gap vs Tokio narrowed again (roughly from ~1.1x slower to ~1.05x slower in this short-run harness).
- net RTT lead remains.
- fs throughput lead remains.

## Implementation: follow-up net throughput optimizations (session + segment path + reprovide coalescing)

Applied the next optimization set aimed at reducing remaining `net_stream_throughput_4k_window32` overhead.

### 1) Persistent session in benchmark worker

`benches/net_api.rs`:

- added `SpargioWindowedSession` that persists across `EchoWindowed` benchmark commands.
- session retains:
  - reusable tx buffer pool,
  - reusable recv scratch buffer,
  - cached multishot capability state.
- worker now reuses this session for matching `(payload, window)` rather than rebuilding per invocation.

### 2) Segment-based multishot API (avoid `Vec<Vec<u8>>` materialization in hot path)

`src/lib.rs`:

- new public types:
  - `UringRecvSegment { offset, len }`
  - `UringRecvMultishotSegments { buffer, segments }`
- new APIs:
  - `UringNativeLane::recv_multishot_segments(...)`
  - `UringBoundFd::recv_multishot_segments(...)`
- `recv_multishot(...)` remains for compatibility and now adapts from segment output.
- `NativeIoOp::RecvMulti` now accumulates into one flat output buffer + segment metadata rather than `Vec<Vec<u8>>`.

### 3) Reprovide coalescing (reduce housekeeping SQEs)

`src/lib.rs`:

- `reprovide_multishot_buffers(...)` now:
  - sorts + deduplicates consumed bids,
  - coalesces contiguous bids into runs,
  - submits one `ProvideBuffers` SQE per contiguous run (instead of one per bid).

### TDD updates

- added test:
  - `uring_bound_tcp_stream_supports_recv_multishot_segments`
- preserved existing multishot compatibility tests; full `--features uring-native` test suite remains green.

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest net benchmark snapshot after this follow-up

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.58-7.90 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`5.25-5.35 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.51-10.85 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`10.84-10.95 ms`

Interpretation:

- stream-throughput gap narrowed further and is now close to parity in this short-run harness.
- RTT lead for Spargio remains.

## Implementation: fs RTT (`qd=1`) optimization batch (items 1-3)

Implemented the requested three-item set for `fs_read_rtt_4k`.

### 1) Run Spargio FS loops inside pinned runtime worker

`benches/fs_api.rs`:

- replaced external `block_on` Spargio loop with a pinned worker command loop (`SpargioFsCmd`).
- `ReadRtt` and `ReadQd` now execute on shard `1` in the runtime task itself.
- benchmark caller uses std mpsc request/reply to drive the worker, mirroring Tokio harness structure more closely.

### 2) Reusable read buffer API (`read_at_into`)

`src/lib.rs`:

- added:
  - `UringNativeLane::read_at_into(fd, offset, buf)`
  - `UringBoundFd::read_at_into(offset, buf)`
- `read_at(...)` now adapts through `read_at_into(...)`.
- added native read-owned command path:
  - `LocalCommand::SubmitNativeReadOwned`
  - backend routing `submit_native_read_owned(...)`
  - driver submission `submit_native_read_owned(...)`
  - native op state `NativeIoOp::ReadOwned`
- completion and failure handling updated for `ReadOwned`.

### 3) Persistent file session API (actor-style)

`src/lib.rs`:

- added `UringFileSession`:
  - `read_at_into(...)`
  - `read_at(...)`
  - `shutdown(...)`
  - `shard()`
- new constructor on bound fd:
  - `UringBoundFd::start_file_session()`
- session is implemented as a pinned shard task with command channel (`UringFileSessionCmd`), keeping repeated file operations on one shard.

### Red/Green TDD

Added failing tests first, then implemented until green:

- `uring_bound_file_supports_read_at_into_reuse`
- `uring_bound_file_session_supports_repeated_reads`

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest FS benchmark snapshot after this batch

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.62-1.73 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`0.99-1.01 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`7.59-7.75 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`5.74-6.27 ms`

Interpretation:

- `qd=1` RTT moved from slower-than-Tokio to faster-than-Tokio in this short-run harness.
- throughput lead at `qd=32` remains.

## Proposal: unbound submission-time steering for all native ops

Goal:

- allow stealable tasks to issue native ops without pre-pinning a lane, while selecting target shard at submission time.

Design slices:

1. Unbound native entrypoint:
   - add `RuntimeHandle::uring_native_unbound() -> UringNativeAny`.
   - expose all native ops (`read/write/fsync`, `send/recv`, batch, multishot) on `UringNativeAny`.
2. Lane selector:
   - introduce `NativeLaneSelector` using per-shard pending native-op depth + round-robin tie-break.
   - support optional locality hints (`preferred_shard`).
3. FD affinity lease table:
   - add `FdAffinityTable` (`fd -> shard`) with TTL/release on idle.
   - use weak leases for file ops, stronger leases for stream/socket ops, hard affinity for multishot lifetime.
4. Generic native command envelope:
   - add `SubmitNativeAny { op, reply }` and route to selected shard.
   - preserve local fast path when selected shard == current shard.
5. Op-family behavior:
   - file single-shot ops steerable per op,
   - stream single-shot ops steerable with lease-aware ordering,
   - batch ops single-lane per batch,
   - multishot fixed-lane for op lifetime (token/stream tied to owning lane).
6. Cancellation/timeouts:
   - add global `op_id -> shard` tracking for correct cancel routing.
   - keep resource cleanup on owning lane.
7. TDD rollout:
   - slice A: unbound file ops + selector correctness/distribution tests.
   - slice B: unbound stream single-shot + batch ordering tests.
   - slice C: unbound multishot lifecycle/cancel/cleanup tests.
   - slice D: benchmark variants (`*_unbound_*`) vs pinned/session APIs.

Recommendation:

- yes, this is worth doing, but as a phased effort.
- rationale:
  - it preserves explicit pinned/session fast paths while adding flexible scheduler-friendly mode for stealable compute tasks.
  - it unlocks broader ergonomics without forcing users to choose one affinity model globally.
- risk:
  - correctness complexity is non-trivial (lease ownership, cancellation routing, multishot lifetime rules), so TDD slice gating is required.

## Implementation: unbound submission-time steering (slices A-D)

Implemented the full unbound slice set in this pass.

### Slice A: unbound entrypoint + selector + file ops

`src/lib.rs`:

- added `RuntimeHandle::uring_native_unbound() -> UringNativeAny`.
- added `NativeLaneSelector`:
  - selection by per-shard pending native-op depth (`pending_native_ops_by_shard`) with round-robin tie-break.
  - optional preferred-shard hinting.
- added `UringNativeAny` API surface for native ops:
  - `read_at`, `read_at_into`, `write_at`, `fsync`
  - plus stream/batch/multishot methods (below).
- added FD affinity lease table (`FdAffinityTable`):
  - weak lease for file-family ops,
  - strong lease for stream single-shot/batch,
  - hard lease for multishot lifetime.
- added unbound op-route tracking:
  - global `NativeOpId` allocation and `op_id -> shard` map.
  - `active_native_op_count()` / `active_native_op_shard(...)` observability.

Stats:

- `RuntimeStats` now includes `pending_native_ops_by_shard`.
- io_uring driver now updates both global pending-native count and per-shard pending-native depth.

### Slice B: stream single-shot + batch behavior

`UringNativeAny` now supports:

- `recv`, `recv_owned`, `recv_into`
- `send`, `send_owned`
- `send_batch`, `send_all_batch`
- `recv_batch_into`

Behavior:

- stream ops are lease-aware (`strong` lease), preserving lane-local ordering tendencies for repeated ops on the same FD.
- batch ops run single-lane per batch.

### Slice C: multishot lifecycle + cleanup

`UringNativeAny` now supports:

- `recv_multishot`
- `recv_multishot_segments`

Behavior:

- multishot uses `hard` FD affinity for operation lifetime.
- affinity is released when multishot completes.
- op-route map entries are added/removed around each unbound op, preserving ownership tracking.

### Slice D: benchmark variants (`*_unbound_*`)

`benches/fs_api.rs`:

- added `SpargioFsUnboundHarness`.
- added benchmark cases:
  - `spargio_uring_unbound_file_qd1`
  - `spargio_uring_unbound_file_qd32`

`benches/net_api.rs`:

- added `SpargioNetUnboundHarness`.
- added benchmark cases:
  - `spargio_uring_unbound_tcp_qd1`
  - `spargio_uring_unbound_tcp_window32`

### Red/Green TDD

Added failing tests first in `tests/uring_native_tdd.rs`, then implemented to green:

- `uring_native_unbound_requires_io_uring_backend`
- `uring_native_unbound_selector_distributes_when_depths_equal`
- `uring_native_unbound_file_ops_work`
- `uring_native_unbound_stream_ops_preserve_affinity_and_order`
- `uring_native_unbound_multishot_releases_hard_affinity_after_completion`
- `uring_native_unbound_tracks_active_op_routes_for_inflight_work`

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest short-run benchmark snapshot

FS:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.55-1.68 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`1.03-1.07 ms`
- `fs_read_rtt_4k/spargio_uring_unbound_file_qd1`: ~`1.01-1.03 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`8.55-8.70 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`5.93-6.68 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_unbound_file_qd32`: ~`6.57-7.38 ms`

Net:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.74-7.97 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`5.48-5.75 ms`
- `net_echo_rtt_256b/spargio_uring_unbound_tcp_qd1`: ~`7.64-8.04 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.69-11.17 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`11.09-11.33 ms`
- `net_stream_throughput_4k_window32/spargio_uring_unbound_tcp_window32`: ~`10.83-10.99 ms`

## Implementation: direct unbound command-envelope optimization (`SubmitNativeAny`)

Implemented the previously planned unbound-path optimization to remove per-op pinned-spawn overhead.

### What changed

`src/lib.rs`:

- added direct native command envelope:
  - `Command::SubmitNativeAny { op: NativeAnyCommand }`
  - `NativeAnyCommand` variants for read/write/fsync, send/recv, batch, multishot.
- `UringNativeAny` now dispatches native ops via:
  - same-shard local fast path: enqueue `LocalCommand` directly.
  - cross-shard envelope path: send `SubmitNativeAny` command to selected shard.
- preserved existing affinity/route semantics:
  - `NativeLaneSelector` selection.
  - FD lease table (`weak`/`strong`/`hard`).
  - `op_id -> shard` tracking and cleanup.

### New observability

`RuntimeStats` now includes:

- `native_any_envelope_submitted`
- `native_any_local_fastpath_submitted`

### Red/Green TDD

Added failing tests first, then implemented to green:

- `uring_native_unbound_records_command_envelope_submission`
- `uring_native_unbound_records_local_fast_path_submission`

### Validation

- `cargo fmt --all`
- `cargo test -q`
- `cargo test -q --features uring-native`
- `cargo bench --no-run --features uring-native`
- `cargo bench --bench fs_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --bench net_api --features uring-native -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

### Latest short-run snapshot after optimization

FS:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: ~`1.754-1.867 ms`
- `fs_read_rtt_4k/spargio_uring_bound_file_qd1`: ~`1.013-1.062 ms`
- `fs_read_rtt_4k/spargio_uring_unbound_file_qd1`: ~`1.003-1.028 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: ~`8.732-9.015 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_bound_file_qd32`: ~`5.967-6.988 ms`
- `fs_read_throughput_4k_qd32/spargio_uring_unbound_file_qd32`: ~`6.085-6.866 ms`

Net:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: ~`7.918-8.187 ms`
- `net_echo_rtt_256b/spargio_uring_bound_tcp_qd1`: ~`6.840-8.632 ms`
- `net_echo_rtt_256b/spargio_uring_unbound_tcp_qd1`: ~`5.539-5.812 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: ~`10.544-10.656 ms`
- `net_stream_throughput_4k_window32/spargio_uring_bound_tcp_window32`: ~`11.073-11.449 ms`
- `net_stream_throughput_4k_window32/spargio_uring_unbound_tcp_window32`: ~`10.996-11.408 ms`

Interpretation:

- unbound `net_echo_rtt_256b` improved materially after removing per-op spawn overhead.
- unbound fs remains competitive and generally close to bound.

## Roadmap Revision: ergonomics-first sequence (requested)

No implementation in this update; this section revises priority order only.

### New priority order

1. Scope simplification first: remove bound APIs to keep the codebase manageable.
   - deprecate/remove `UringNativeLane`/`UringBoundFd`-centric public paths in favor of unbound-first APIs.
   - remove bound-only benchmark variants and docs references once replacement coverage exists.
2. Ergonomics project (highest priority after simplification):
   - deliver a high-level API layer targeting parity with Compio-style filesystem and network ergonomics.
   - target outcome: common file/network flows can be written without manual lane/FD plumbing boilerplate.
3. After ergonomics parity milestone is complete:
   - add benchmark suites against Compio for filesystem and network APIs, with matched workload shapes.
   - prioritize broader native I/O surface expansion.
4. Then continue with remaining milestones:
   - production-grade work-stealing policy (fairness/starvation/adaptive heuristics),
   - tail-latency perf program (longer windows + p95/p99 gates),
   - production hardening (stress/soak/failure injection/observability),
   - optional Tokio-compat readiness shim as a separate large-investment track.

### Ergonomics parity target (Compio-like)

At completion of the ergonomics project, Spargio should provide equivalent day-to-day usability for core filesystem/network tasks:

- filesystem:
  - high-level async file open/create/read/write helpers,
  - convenience methods equivalent to common `read_to_end_at`/buffer-reuse workflows.
- network:
  - high-level async TCP/UDP connect/accept/send/recv helpers,
  - convenience traits/wrappers for common read/write loops and batching patterns.
- runtime entry ergonomics:
  - straightforward app entry patterns (macro or helper-based) with minimal setup boilerplate.

### Notes

- This roadmap change intentionally favors API usability and adoption surface before deeper policy/perf-hardening tracks.
- Bound APIs are treated as temporary complexity and are planned for removal ahead of the ergonomics phase.
- Post-ergonomics benchmarking will include explicit Spargio-vs-Compio fs/net comparisons.

## Update: scope simplification + ergonomics APIs + Compio benchmark lane

Completed the requested implementation batch in three slices:

### 1) Scope simplification (bound API removal)

Removed bound-centric native public APIs from `src/lib.rs`:

- removed `RuntimeHandle::uring_native_lane(...)`
- removed `UringNativeLane`
- removed `UringBoundFd`
- removed `UringFileSession`

Native public surface is now unbound-first:

- `RuntimeHandle::uring_native_unbound() -> UringNativeAny`

Also removed bound-oriented TDD/bench usage and migrated coverage to unbound equivalents.

### 2) Ergonomics project (Compio-like API shape)

Added high-level wrappers over unbound native ops in `src/lib.rs`:

- `spargio::fs`
  - `OpenOptions`
  - `File`
    - `open`, `create`, `from_std`
    - `read_at`, `read_at_into`, `read_to_end_at`
    - `write_at`, `write_all_at`, `fsync`
- `spargio::net`
  - `TcpStream`
    - `connect`, `from_std`
    - `send`, `recv`, `send_owned`, `recv_owned`
    - `send_all_batch`, `recv_multishot_segments`
    - `write_all`, `read_exact`
  - `TcpListener`
    - `bind`, `from_std`, `local_addr`, `accept`

Added red/green tests:

- new `tests/ergonomics_tdd.rs`
  - `fs_open_read_to_end_and_write_at`
  - `net_tcp_stream_connect_supports_read_write_all`
  - `net_tcp_listener_bind_accepts_and_wraps_stream`
- rewrote `tests/uring_native_tdd.rs` to unbound-only coverage.

### 3) Benchmark refresh + Compio comparisons

Added Compio to Linux dev-dependencies:

- `Cargo.toml`:
  - `[target.'cfg(target_os = "linux")'.dev-dependencies]`
  - `compio = { version = "0.18.0", default-features = false, features = ["runtime", "io-uring", "fs", "net", "io"] }`

Rewrote benchmark harnesses:

- `benches/fs_api.rs`
  - compares:
    - `tokio_spawn_blocking_pread_qd1`
    - `spargio_fs_read_at_qd1`
    - `compio_fs_read_at_qd1`
    - `tokio_spawn_blocking_pread_qd32`
    - `spargio_fs_read_at_qd32`
    - `compio_fs_read_at_qd32`
- `benches/net_api.rs`
  - compares:
    - `tokio_tcp_echo_qd1`
    - `spargio_tcp_echo_qd1`
    - `compio_tcp_echo_qd1`
    - `tokio_tcp_echo_window32`
    - `spargio_tcp_echo_window32`
    - `compio_tcp_echo_window32`

### Validation

- `cargo fmt`
- `cargo test --features uring-native --tests`
- `cargo bench --features uring-native --no-run`
- `cargo bench --features uring-native --bench fs_api -- --sample-size 20`
- `cargo bench --features uring-native --bench net_api -- --sample-size 20`

### Latest benchmark snapshot (sample-size 20)

FS:

- `fs_read_rtt_4k/tokio_spawn_blocking_pread_qd1`: `1.601-1.641 ms`
- `fs_read_rtt_4k/spargio_fs_read_at_qd1`: `1.012-1.026 ms`
- `fs_read_rtt_4k/compio_fs_read_at_qd1`: `1.388-1.421 ms`
- `fs_read_throughput_4k_qd32/tokio_spawn_blocking_pread_qd32`: `7.680-7.767 ms`
- `fs_read_throughput_4k_qd32/spargio_fs_read_at_qd32`: `5.971-6.054 ms`
- `fs_read_throughput_4k_qd32/compio_fs_read_at_qd32`: `5.983-6.119 ms`

Net:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: `7.913-8.056 ms`
- `net_echo_rtt_256b/spargio_tcp_echo_qd1`: `5.542-5.606 ms`
- `net_echo_rtt_256b/compio_tcp_echo_qd1`: `6.530-6.646 ms`
- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: `11.306-11.511 ms`
- `net_stream_throughput_4k_window32/spargio_tcp_echo_window32`: `16.903-17.082 ms`
- `net_stream_throughput_4k_window32/compio_tcp_echo_window32`: `6.928-7.091 ms`

### Notes

- This completes the requested simplification + ergonomics + Compio benchmark scope.
- Current ergonomic `fs::OpenOptions::open`, `net::TcpListener::bind/accept`, and `net::TcpStream::connect` are async wrappers using blocking helper threads for setup operations; native io_uring open/accept/connect op coverage remains future work.

## Update: net throughput optimization pass (owned buffers + batch/multishot receive)

Focused on `net_stream_throughput_4k_window32`, where Spargio remained behind Tokio/Compio after the ergonomics migration.

### Red/Green TDD

Added failing ergonomics test first:

- `tests/ergonomics_tdd.rs`
  - `net_tcp_stream_owned_buffers_support_read_write_all`

Then implemented the API and benchmark-path changes to green.

### API changes (`spargio::net::TcpStream`)

`src/lib.rs`:

- added `write_all_owned(Vec<u8>) -> io::Result<Vec<u8>>`
- added `read_exact_owned(Vec<u8>) -> io::Result<Vec<u8>>`
- optimized `read_exact(&mut [u8])` to reuse a scratch receive buffer rather than allocating per recv loop.

These allow high-frequency send/recv loops to reuse caller-owned buffers and avoid repeated allocation churn.

### Benchmark harness changes

`benches/net_api.rs`:

- `spargio_echo_rtt` now uses owned-buffer helpers:
  - `write_all_owned`
  - `read_exact_owned`
- `spargio_echo_windowed` now uses a throughput-oriented native path:
  - prebuild frame batch from reusable tx pool
  - `send_all_batch(...)`
  - `recv_multishot_segments(...)` with kernel capability fallback (`EINVAL/ENOSYS/EOPNOTSUPP`)
  - fallback receive path uses `read_exact_owned` with reusable buffer

### Validation

- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo bench --features uring-native --bench net_api --no-run`
- `cargo bench --features uring-native --bench net_api -- --sample-size 20`

### Latest `net_api` snapshot after optimization

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: `7.878-8.032 ms`
- `net_echo_rtt_256b/spargio_tcp_echo_qd1`: `5.516-5.613 ms`
- `net_echo_rtt_256b/compio_tcp_echo_qd1`: `6.555-6.715 ms`

- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: `11.147-11.318 ms`
- `net_stream_throughput_4k_window32/spargio_tcp_echo_window32`: `10.889-10.974 ms`
- `net_stream_throughput_4k_window32/compio_tcp_echo_window32`: `7.090-7.225 ms`

Result: Spargio throughput moved from clearly behind Tokio to slightly ahead in this harness run, while remaining behind Compio in sustained stream throughput.

## Update: local stream-session fast path + pool-backed multishot snapshot

Follow-up optimization work after the prior net-throughput pass.

### What was implemented

1) Local stream-session fast path (submission without unbound route tracking)

`src/lib.rs` (`UringNativeAny` + `spargio::net::TcpStream`):

- added direct-to-shard submit helper in `UringNativeAny`:
  - bypasses `op_routes` + FD-affinity lock bookkeeping for stream-session calls.
- added stream-session methods on `UringNativeAny`:
  - `select_stream_session_shard`
  - `recv_owned_on_shard`
  - `send_owned_on_shard`
  - `send_all_batch_on_shard`
  - `recv_multishot_segments_on_shard`
- `spargio::net::TcpStream` now selects a session shard at construction and routes stream ops through these methods.

2) Multishot receive copy-path change

`src/lib.rs` (`IoUringDriver::complete_native_op`):

- removed per-CQE compaction copy (`out.extend_from_slice(...)`) for multishot segments.
- now records segment offsets directly against buffer-pool layout (`bid * buffer_len`).
- returns a pool-backed snapshot buffer (`pool.storage.to_vec()`) with segment metadata.

Note: this is a safe pool-backed snapshot path (no per-segment compaction copy), not a full ownership-transfer zero-copy path. A first ownership-transfer attempt caused unsafe kernel buffer-registration interactions and was not kept.

### Red/Green TDD additions

Added failing tests first, then implemented to green:

- `tests/ergonomics_tdd.rs`
  - `net_tcp_stream_session_path_does_not_track_unbound_op_routes`
- `tests/uring_native_tdd.rs`
  - `uring_native_unbound_multishot_segments_expose_pool_backing_without_compaction_copy`

### Validation

- `cargo test --features uring-native --tests`
- `cargo bench --features uring-native --bench net_api -- --sample-size 20`

### Latest `net_api` snapshot after this pass

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: `7.923-8.118 ms`
- `net_echo_rtt_256b/spargio_tcp_echo_qd1`: `5.410-5.516 ms`
- `net_echo_rtt_256b/compio_tcp_echo_qd1`: `6.447-6.530 ms`

- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: `10.902-11.155 ms`
- `net_stream_throughput_4k_window32/spargio_tcp_echo_window32`: `11.225-11.441 ms`
- `net_stream_throughput_4k_window32/compio_tcp_echo_window32`: `7.007-7.118 ms`

Interpretation:

- stream RTT improved further on Spargio.
- throughput remains near Tokio (within a few percent in this run) and behind Compio on sustained stream throughput.

## Update: imbalanced net-stream benchmark (hot/cold skew)

Added a third `net_api` benchmark to measure skewed stream load across multiple concurrent TCP connections.

### What changed

- `benches/net_api.rs`:
  - refactored echo server fixture to support N accepted client connections per harness (`spawn_echo_server_with_clients`).
  - extended Tokio/Spargio/Compio harness command sets with `EchoImbalanced`.
  - each harness now creates `IMBALANCED_STREAMS=8` persistent streams.
  - existing RTT/windowed benchmarks continue to use the primary stream.
  - new benchmark group: `net_stream_imbalanced_4k_hot1_light7`.

### Imbalanced workload definition

- Streams: `8`
- Payload: `4096` bytes
- Window: `32`
- Heavy stream (`idx=0`): `2048` frames
- Light streams (`idx=1..7`): `128` frames each
- Total per iteration: `11,468,800` bytes

### Validation

- `cargo check --features uring-native --bench net_api`
- `cargo bench --features uring-native --bench net_api -- --sample-size 20`

### Latest results (`--sample-size 20`)

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: `7.903-8.093 ms`
- `net_echo_rtt_256b/spargio_tcp_echo_qd1`: `5.405-5.474 ms`
- `net_echo_rtt_256b/compio_tcp_echo_qd1`: `6.472-6.593 ms`

- `net_stream_throughput_4k_window32/tokio_tcp_echo_window32`: `11.157-11.203 ms`
- `net_stream_throughput_4k_window32/spargio_tcp_echo_window32`: `11.085-11.166 ms`
- `net_stream_throughput_4k_window32/compio_tcp_echo_window32`: `7.136-7.277 ms`

- `net_stream_imbalanced_4k_hot1_light7/tokio_tcp_8streams_hotcold`: `13.595-13.853 ms` (`830-846 MiB/s`)
- `net_stream_imbalanced_4k_hot1_light7/spargio_tcp_8streams_hotcold`: `16.335-16.502 ms` (`697-704 MiB/s`)
- `net_stream_imbalanced_4k_hot1_light7/compio_tcp_8streams_hotcold`: `12.089-12.215 ms` (`942-951 MiB/s`)

### Notes

- The new skew benchmark is stable and repeatable.
- In the current implementation, Spargio is behind Tokio and Compio on this hot/cold multi-stream workload.

## Update: hypotheses and A/B plan for imbalanced net-stream slowdown

This captures why `net_stream_imbalanced_4k_hot1_light7` is currently slower on Spargio and what we should test next before changing core runtime behavior.

### Hypotheses

1. Workload shape is dominated by one serialized hot stream.
- In hot1/light7, one stream carries most bytes; single-stream TCP ordering limits parallelism and reduces benefits from work stealing.

2. Session-shard concentration reduces lane spread.
- Streams are created from one worker context; `TcpStream` picks `session_shard` at construction.
- With preferred-shard bias in selector, many streams may end up on the same shard.

3. Cross-shard submit overhead in imbalanced path.
- Imbalanced benchmark spawns stealable tasks per stream, but stream I/O still routes to stream `session_shard`.
- If task executes off-session-shard, each op pays envelope/command/oneshot overhead.

4. Multishot receive path still performs heavy copying.
- Current multishot completion returns a pool snapshot via `pool.storage.to_vec()`.
- This copies the full pool per batch and can dominate throughput in hot stream workloads.

### Quick A/B plan to prove each cause

A/B-1: workload-shape sensitivity (hot-stream serialization)
- A: current `hot1/light7` profile.
- B: balanced profile with same total bytes spread evenly across streams.
- Success signal: if Spargio narrows/erases gap on balanced profile, shape serialization is a primary contributor.

A/B-2: stream session-shard distribution
- A: current stream construction path.
- B: instrument and enforce explicit spread (round-robin stream creation context or per-stream target shard) and record distribution.
- Success signal: if better spread improves imbalanced throughput, lane concentration is a contributor.

A/B-3: task placement vs. stream session shard
- A: current `spawn_stealable` for stream workers.
- B: run stream workers pinned/preferred to each stream `session_shard`.
- Success signal: if B improves latency/throughput, cross-shard submit overhead is material.

A/B-4: multishot copy cost
- A: current `take_recv_pool_storage -> to_vec()` behavior.
- B: copy only touched segment ranges (or temporarily force non-multishot read path as control).
- Success signal: lower time and reduced CPU/memory pressure confirms copy-path dominance.

### Copy-reduction and related optimization options

1) Copy only touched bytes from multishot segments (low risk).
- Replace full-pool clone with segment-aware gather into a compact output buffer.
- Expected effect: materially lower copy volume on partial-pool consumption.

2) Segment-fold API to avoid materializing receive buffers (medium risk).
- Add API that processes multishot segments in-place and returns folded result (checksum/parser state/etc.).
- Expected effect: near-zero extra copy for many streaming workloads.

3) Pool lease API for true zero-copy receive view (higher complexity).
- Return a lease object that references registered pool storage + segment metadata.
- Reclaim buffers on lease drop, with double-buffered pool strategy to keep pipeline full.

4) Placement alignment for stream workers (complementary).
- Run per-stream tasks on their `session_shard` by default in throughput-oriented paths.
- Expected effect: remove cross-shard submit + response overhead from hot I/O loops.

### Priority suggestion

- First: A/B-4 (copy path) and A/B-3 (placement alignment).
- Then: A/B-2 (distribution), A/B-1 (shape sensitivity) for explanatory confidence and benchmark positioning.

## Update: A/B results for imbalanced net-stream hypotheses

Ran targeted A/B matrix in `benches/net_api.rs` via benchmark group:
- `net_stream_imbalanced_ab_4k`

Command used:
- `cargo bench --features uring-native --bench net_api -- net_stream_imbalanced_ab_4k --sample-size 12`

### Key results (time ranges)

- `tokio_hotcold`: `13.547-13.682 ms`
- `tokio_balanced_total_bytes`: `8.046-8.174 ms`

- `spargio_hotcold_stealable_multishot`: `16.337-16.454 ms`
- `spargio_hotcold_pinned_multishot`: `16.358-16.512 ms`
- `spargio_hotcold_stealable_readexact`: `17.902-17.970 ms`
- `spargio_hotcold_pinned_readexact`: `17.742-17.896 ms`

- `spargio_balanced_stealable_multishot` (single-context stream init): `16.861-16.986 ms`

- `spargio_hotcold_stealable_multishot_distributed_connect`: `13.534-13.684 ms`
- `spargio_hotcold_pinned_multishot_distributed_connect`: `13.300-13.360 ms`
- `spargio_balanced_stealable_multishot_distributed_connect`: `9.080-9.172 ms`

### Hypothesis outcomes

1) Workload shape (hot-stream serialization) matters: **confirmed**.
- Tokio hotcold vs balanced shows a large swing.
- Spargio shows the same swing once stream session distribution is fixed (`13.6 ms` hotcold vs `9.1 ms` balanced in distributed-connect mode).

2) Session-shard concentration / stream distribution: **strongly confirmed (primary factor)**.
- Spargio hotcold improves from ~`16.4 ms` to ~`13.6 ms` by only changing stream init to distributed-connect.
- This is the biggest single improvement in the A/B set.

3) Placement alignment (stealable vs pinned-to-session): **secondary effect**.
- In single-context mode, pinned vs stealable is effectively flat.
- In distributed-connect mode, pinned gives a modest gain (~2%).

4) Multishot copy-path concern: **not primary in this workload**.
- `read_exact` variants are slower than multishot by ~8-10%.
- Conclusion: reducing full-pool clone may still help, but it is not the top bottleneck for this benchmark shape.

### Re-evaluated optimization priorities

1. Make stream session-shard distribution explicit/default for multi-stream workloads.
- Add runtime/net API controls for connect-time lane selection (e.g., round-robin shard hinting).

2. Add stream-task placement helpers that align execution with stream session shard.
- Keep work-stealable default, but provide an easy pinned/session-aligned fast path for throughput loops.

3. Keep multishot as default receive path for throughput profiles.
- Do not switch to read_exact-only path for this workload class.

4. Move copy-reduction work to medium priority.
- Touched-range copy and lease-based zero-copy remain worthwhile, but after (1) and (2).

5. Add follow-up benchmark scenarios to validate generality.
- skewed + distributed under larger windows, mixed payload sizes, and parser-like downstream processing.

## Update: implemented optimization priorities from imbalanced A/B findings

Implemented the re-prioritized optimization set focused on multi-stream distribution, session-aligned execution ergonomics, and receive-copy reduction.

### 1) Stream distribution controls (runtime API)

`src/lib.rs` (`spargio::net`):

- added `StreamSessionPolicy`:
  - `ContextPreferred`
  - `RoundRobin`
  - `Fixed(ShardId)`
- added session-policy connect APIs on `TcpStream`:
  - `connect_with_session_policy(...)`
  - `connect_round_robin(...)`
  - `connect_many_with_session_policy(...)`
  - `connect_many_round_robin(...)`
- added session-policy wrap API:
  - `from_std_with_session_policy(...)`
- kept existing `connect(...)` / `from_std(...)` behavior via `ContextPreferred`.
- added session-policy accept APIs on `TcpListener`:
  - `accept_with_session_policy(...)`
  - `accept_round_robin(...)`

This makes multi-stream session placement explicit and gives a first-class round-robin path without requiring benchmark-specific task orchestration.

### 2) Session-shard-aligned execution helpers

`src/lib.rs` (`spargio::net::TcpStream`):

- added `spawn_on_session(&RuntimeHandle, fut)`
- added `spawn_stealable_on_session(&RuntimeHandle, fut)`

This removes boilerplate for session-aligned throughput loops and enables straightforward pinned-to-session execution from stream handles.

### 3) Keep multishot as default throughput receive path

`benches/net_api.rs`:

- throughput/imbalanced hot paths continue to default to multishot receive mode.
- read-exact is kept only as A/B comparison lane.

### 4) Copy reduction for multishot completion

`src/lib.rs` (io_uring driver):

- replaced full pool clone in multishot completion path with compact touched-range copy:
  - old: full `pool.storage.to_vec()` clone
  - new: copy only segment-covered ranges and rewrite segment offsets to compact buffer coordinates

This reduces receive-copy volume when only a subset of the registered pool is used per operation.

### Benchmark harness updates

`benches/net_api.rs`:

- `SpargioStreamInitMode::DistributedConnect` now uses runtime API (`connect_many_round_robin`) instead of benchmark-local pinned-connect orchestration.
- `bench_net_stream_imbalanced_4k_hot1_light7` uses distributed-connect Spargio harness (optimized multi-stream path).
- A/B matrix retained (`net_stream_imbalanced_ab_4k`) and updated to use the new helpers.

### Red/Green TDD

Added failing tests first, then implemented to green:

- `tests/ergonomics_tdd.rs`
  - `net_tcp_stream_connect_round_robin_distributes_session_shards`
  - `net_tcp_stream_spawn_on_session_runs_on_stream_session_shard`
- `tests/uring_native_tdd.rs`
  - updated multishot-copy expectation:
    - `uring_native_unbound_multishot_segments_use_compact_buffer_copy`

Validation:

- `cargo test --features uring-native --tests`
- `cargo check --features uring-native --bench net_api`
- `cargo bench --features uring-native --bench net_api -- net_stream_imbalanced_ab_4k --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- net_stream_imbalanced_4k_hot1_light7 --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- net_echo_rtt_256b --sample-size 12`

### Post-change benchmark snapshot (latest runs)

Imbalanced target benchmark:

- `net_stream_imbalanced_4k_hot1_light7/tokio_tcp_8streams_hotcold`: `14.058-14.331 ms`
- `net_stream_imbalanced_4k_hot1_light7/spargio_tcp_8streams_hotcold`: `13.300-13.734 ms`
- `net_stream_imbalanced_4k_hot1_light7/compio_tcp_8streams_hotcold`: `12.174-12.499 ms`

A/B confirmation:

- `spargio_hotcold_stealable_multishot_distributed_connect`: `13.410-13.639 ms`
- `spargio_hotcold_pinned_multishot_distributed_connect`: `13.050-13.144 ms`
- `spargio_balanced_stealable_multishot_distributed_connect`: `8.886-8.942 ms`

RTT sanity after harness adjustment:

- `net_echo_rtt_256b/tokio_tcp_echo_qd1`: `7.988-8.128 ms`
- `net_echo_rtt_256b/spargio_tcp_echo_qd1`: `5.625-5.793 ms`
- `net_echo_rtt_256b/compio_tcp_echo_qd1`: `6.599-6.704 ms`

### Interpretation

- Primary bottleneck identified earlier (session concentration) is now addressed via runtime API and benchmark-path adoption.
- Session-aligned helpers are in place and show modest additional gains in distributed mode.
- Compact multishot copy reduced copy overhead and improved several A/B lanes, while multishot remains better than read-exact for these workloads.

## Update: separated net A/B scenarios into experimental benchmark target

To keep long-running benchmark reporting focused and stable, imbalanced A/B diagnostic scenarios were moved out of the main net benchmark target.

### What changed

- Added new bench target in `Cargo.toml`:
  - `[[bench]] name = "net_experiments"`
- Main benchmark target `benches/net_api.rs` now includes only product-facing groups:
  - `net_echo_rtt_256b`
  - `net_stream_throughput_4k_window32`
  - `net_stream_imbalanced_4k_hot1_light7`
- Experimental A/B matrix moved to `benches/net_experiments.rs`.
- Experimental group renamed for clarity:
  - `exp_net_stream_imbalanced_ab_4k`

### Usage

- Product-facing benchmark suite:
  - `cargo bench --features uring-native --bench net_api`
- Experimental diagnostic suite:
  - `cargo bench --features uring-native --bench net_experiments`

### Validation

- `cargo check --features uring-native --bench net_api --bench net_experiments`
- Verified no A/B group is exposed from `net_api` target.
- Verified `net_experiments` runs `exp_net_stream_imbalanced_ab_4k` as intended.

## Update: dynamic-imbalance benchmark backlog + pipeline-hotspot implementation

Captured additional benchmark shapes (posterity/backlog) to better probe the `msg_ring` + work-stealing value proposition under dynamic skew:

1. `net_stream_hotspot_rotation`
- rotating hot stream without explicit CPU stage.
2. `net_stream_bursty_tenants`
- many streams with bursty ON/OFF activity and skewed arrivals.
3. `net_pipeline_imbalanced_io_cpu`
- per-frame recv/CPU/send pipeline with rotating hotspot.
4. `fanout_fanin_hotkey_rotation`
- fanout/fanin with moving hot key pressure across shards.
5. `accept_connect_churn_skewed`
- skewed short-lived connection churn including setup path.

Implemented now:

- Added new benchmark group in `benches/net_api.rs`:
  - `net_pipeline_hotspot_rotation_4k_window32`
- Added runtime lanes in the existing Tokio/Spargio/Compio net harness commands:
  - `*_pipeline_hotspot` command + execution path per runtime.
- Workload shape:
  - 8 streams, 4 KiB frames, window 32.
  - hotspot rotates every 64 frames.
  - per-frame CPU stage after echo receive (`heavy` for current hotspot stream, `light` for others).
- Added a shared deterministic CPU stage helper used by all three runtimes to keep the comparison shape aligned.

Validation:

- `cargo fmt`
- `cargo check --features uring-native --bench net_api`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`

Quick snapshot (`sample-size 10`):

- `net_pipeline_hotspot_rotation_4k_window32/tokio_tcp_pipeline_hotspot`: `26.075-26.308 ms`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`: `32.686-33.156 ms`
- `net_pipeline_hotspot_rotation_4k_window32/compio_tcp_pipeline_hotspot`: `50.496-51.812 ms`

## Update: added `net_stream_hotspot_rotation_4k` (I/O-only rotating hotspot)

Implemented the follow-up benchmark shape requested to isolate dynamic skew effects without an explicit CPU stage.

What was added:

- New benchmark group in `benches/net_api.rs`:
  - `net_stream_hotspot_rotation_4k`
- New runtime command lane across Tokio/Spargio/Compio harnesses:
  - `EchoHotspotRotation`
- Workload definition:
  - 8 streams
  - 4 KiB frames
  - hotspot rotates each step (`step % stream_count`)
  - per-step frame budget:
    - hotspot stream: `32` frames
    - non-hot streams: `2` frames
  - `64` steps total
  - window `32`

Validation:

- `cargo fmt`
- `cargo check --features uring-native --bench net_api`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`

Quick snapshot (`sample-size 10`):

- `net_stream_hotspot_rotation_4k/tokio_tcp_8streams_rotating_hotspot`: `8.7249-8.7700 ms`
- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`: `11.499-11.600 ms`
- `net_stream_hotspot_rotation_4k/compio_tcp_8streams_rotating_hotspot`: `16.637-16.766 ms`

## Roadmap update: runtime entry ergonomics moved to the front

To reduce first-use friction, runtime entry ergonomics is now the first item in the upcoming roadmap.

Updated upcoming order:

1. Runtime entry ergonomics:
   - add a simple helper entrypoint (for example `spargio::run(...)`).
   - add optional `#[spargio::main]` proc-macro sugar in a companion proc-macro crate.
   - ensure feature-gated behavior and clear fallback/error messaging on unsupported platforms.
2. Remove blocking APIs from the public runtime surface.
   - replace helper-thread `run_blocking` paths in `fs::OpenOptions::open`, `net::TcpStream::connect`, and `net::TcpListener::bind/accept`.
   - require native/non-blocking paths for these setup operations.
3. Continue ergonomic parity work for fs/net API discoverability and docs.
4. Continue dynamic-imbalance benchmark expansion and optimization loops.
5. Proceed with broader native I/O surface + hardening milestones.

## Update: runtime entry ergonomics slice (helpers + `#[spargio::main]`)

Completed the next runtime-entry ergonomics slice with red/green TDD.

### Red phase

- Added new integration tests in `tests/entry_macro_tdd.rs`:
  - `main_macro_executes_async_body`
  - `main_macro_applies_builder_overrides`
  - `main_macro_panics_on_runtime_build_failure`
- Ran:
  - `cargo test --features macros --test entry_macro_tdd`
- Expected failure observed:
  - package did not yet expose a `macros` feature.

### Green phase

- Added companion proc-macro crate:
  - `spargio-macros/Cargo.toml`
  - `spargio-macros/src/lib.rs`
- Implemented `#[spargio::main]` attribute macro:
  - supports async no-arg function entry wrappers;
  - supports options: `shards = ...`, `backend = "queue" | "io_uring"`;
  - validates unsupported signatures/options at compile time.
- Wired feature-gated export in main crate:
  - `Cargo.toml`: added optional dependency + `macros` feature.
  - `src/lib.rs`: `#[cfg(feature = "macros")] pub use spargio_macros::main;`
- Existing helper entry APIs (`spargio::run`, `spargio::run_with`) remain the non-macro path.

### Validation

- `cargo test --features macros --test entry_macro_tdd`
- `cargo test --test runtime_tdd`
- `cargo test --features macros --tests`
- `cargo fmt`

### Status

- Runtime entry ergonomics roadmap item is now covered by:
  - helper entry (`run`, `run_with`) and
  - optional attribute macro entry (`#[spargio::main]`).
- Next planned item remains removing blocking setup APIs from the public fs/net surface.

## Update: removed blocking setup helpers from fs/net public APIs (Red/Green TDD)

Goal completed:

- Removed helper-thread `run_blocking` setup paths from:
  - `spargio::fs::OpenOptions::open`
  - `spargio::net::TcpStream::connect*`
  - `spargio::net::TcpListener::bind/accept*`

### Red phase

Added/expanded failing tests in `tests/ergonomics_tdd.rs` to lock behavior before implementation:

- `net_tcp_stream_connect_supports_read_write_all` now asserts returned stream fd is nonblocking.
- `net_tcp_listener_bind_accepts_and_wraps_stream` now asserts accepted stream fd is nonblocking.
- Added fs option-compat tests:
  - `fs_open_options_create_new_reports_already_exists`
  - `fs_open_options_append_and_truncate_is_invalid`

Observed red failure before implementation:

- connected/accepted stream nonblocking assertions failed with existing helper-thread setup path.

### Green phase

Implemented native setup operations in the io_uring command pipeline:

- Added new native command flow variants (`NativeAnyCommand`, `LocalCommand`, backend dispatch, driver submission/completion):
  - `OpenAt`
  - `Connect`
  - `Accept`
- Added `UringNativeAny` helpers:
  - `open_at(...)`
  - `connect_on_shard(...)`
  - `accept_on_shard(...)`
- Added driver-side completion handling for new `NativeIoOp` variants.

Public API behavior changes:

- `fs::OpenOptions::open` now uses native `IORING_OP_OPENAT` instead of helper threads.
- `net::TcpStream::connect*` now creates nonblocking sockets and completes with native `IORING_OP_CONNECT` on the chosen shard.
- `net::TcpListener::accept*` now uses native `IORING_OP_ACCEPT` (nonblocking + cloexec accepted sockets).
- `net::TcpListener::bind` now creates/binds/listens via nonblocking socket syscalls (no helper thread).
- `TcpStream::from_std_with_session_policy` now enforces nonblocking mode.

Notes:

- Added sockaddr encode/decode helpers for IPv4/IPv6 setup/completion paths.
- `fs::OpenOptions` flag mapping now validates invalid combinations in-process and uses `openat` flags/mode directly.

### Validation

Executed:

- `cargo fmt`
- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo test --features uring-native --test uring_native_tdd`
- `cargo test --features uring-native`

Result:

- All tests pass.

## Update: benchmark refresh after native setup-path changes

Re-ran the monitored benchmark suites and refreshed README tables.

Command profile used for all runs:

- `--warm-up-time 0.05`
- `--measurement-time 0.05`
- `--sample-size 20`

Commands executed:

- `cargo bench --features uring-native --bench ping_pong -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --features uring-native --bench fanout_fanin -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --features uring-native --bench fs_api -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`
- `cargo bench --features uring-native --bench net_api -- --warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

Highlights from refreshed results:

- Coordination:
  - `steady_ping_pong_rtt`: Tokio `1.4509-1.4888 ms`, Spargio `357.27-378.34 us`.
  - `steady_one_way_send_drain`: Tokio `70.972-75.645 us`, Spargio `66.006-66.811 us`.
  - `cold_start_ping_pong`: Tokio `535.65-601.90 us`, Spargio `262.24-291.99 us`.
  - `fanout_fanin_balanced`: Tokio `1.4625-1.5346 ms`, Spargio `1.3333-1.3496 ms`.
  - `fanout_fanin_skewed`: Tokio `2.4001-2.7005 ms`, Spargio `1.9590-1.9900 ms`.

- Native fs/net:
  - `fs_read_rtt_4k`: Tokio `1.6476-1.7647 ms`, Spargio `0.99148-1.0145 ms`, Compio `1.3893-1.4970 ms`.
  - `fs_read_throughput_4k_qd32`: Tokio `7.4895-7.6145 ms`, Spargio `5.9790-6.4699 ms`, Compio `5.4749-5.8905 ms`.
  - `net_echo_rtt_256b`: Tokio `7.7059-8.0959 ms`, Spargio `5.3708-5.6477 ms`, Compio `6.4743-6.7640 ms`.
  - `net_stream_throughput_4k_window32`: Tokio `11.163-11.324 ms`, Spargio `10.668-10.719 ms`, Compio `7.2779-7.4795 ms`.

- Imbalanced net:
  - `net_stream_imbalanced_4k_hot1_light7`: Tokio `13.426-14.098 ms`, Spargio `13.510-13.911 ms`, Compio `12.221-12.479 ms`.
  - `net_stream_hotspot_rotation_4k`: Tokio `8.6480-8.7488 ms`, Spargio `11.285-11.811 ms`, Compio `16.346-16.702 ms`.
  - `net_pipeline_hotspot_rotation_4k_window32`: Tokio `26.383-26.937 ms`, Spargio `34.962-35.935 ms`, Compio `50.764-51.179 ms`.

Outcome:

- README benchmark tables and interpretation updated to match this refresh.

## Next Plan: remove remaining blocking surfaces (checklist + sequence)

Goal:

- Keep data-plane waits and setup on native nonblocking/io_uring paths.
- Move control-plane APIs to async-first shapes, then deprecate blocking variants.

Remaining blocking surfaces identified:

- Boundary blocking ticket wait:
  - `BoundaryTicket::wait_timeout_blocking`.
- Boundary blocking server/client paths:
  - `BoundaryServer::recv`, `BoundaryServer::recv_timeout`, and blocking `BoundaryClient::call`.
- Timer helper:
  - `sleep` currently spawns a thread and uses `thread::sleep`.
- Hostname resolution path:
  - `to_socket_addrs()` in `first_socket_addr` can block for DNS.
- Synchronous runtime-control entry points:
  - `run_with` (`block_on`) and `shutdown` thread `join` waits.
- Queue-backend shard idle wait:
  - `rx.recv_timeout(idle_wait)` (fallback/control-plane backend).

Execution sequence (prioritized):

1. io_uring timer lane (high impact, low risk)
   - Add native timeout operation (`IORING_OP_TIMEOUT`) and route `sleep` through it on io_uring backend.
   - Keep queue backend fallback behavior unchanged.
   - Add TDD coverage for timer correctness/cancellation semantics.

2. Async-first boundary API (high impact, medium risk)
   - Add async `BoundaryServer::recv_async`/stream-style polling API.
   - Add async-first client call path and keep existing blocking APIs as compatibility wrappers.
   - Mark blocking variants as compatibility APIs in docs (and later deprecate).

3. Address-resolution split (medium impact, low risk)
   - Add `connect_socket_addr`-first API guidance and docs.
   - Keep hostname API but route through explicit resolver boundary so blocking DNS is isolated and optional.
   - Add tests that `SocketAddr` path stays fully nonblocking.

4. Runtime-control async variants (medium impact, medium risk)
   - Add `run_async` and `shutdown_async` (non-blocking caller thread semantics).
   - Keep existing sync entry points for ergonomics/back-compat.

5. Queue backend scope decision (medium impact, design choice)
   - Either:
     - keep queue backend as debug/fallback and accept blocking `recv_timeout`, or
     - reduce queue backend role and push io_uring-only profiles as default perf lane.
   - Record decision in ADR/log before implementation changes.

Acceptance checklist:

- [ ] No data-plane helper-thread blocking waits in io_uring mode.
- [ ] `sleep` uses native timeout path when io_uring backend is active.
- [ ] Boundary APIs have async-first equivalents covering current usage.
- [ ] Hostname resolution path is explicitly isolated from native data plane.
- [ ] README/implementation log reflect which blocking APIs are compatibility-only vs removed.

## Update: queue backend removed from public runtime configuration

Decision implemented from the blocking-surface plan:

- Queue backend is no longer selectable via `BackendKind`.
- `BackendKind` now exposes only `IoUring`.
- `RuntimeBuilder::default()` now defaults to `BackendKind::IoUring`.

Code and harness updates:

- Removed `BackendKind::Queue` usage from tests and benches.
- Updated runtime tests that previously forced queue mode to use io_uring (with existing graceful skip behavior when io_uring init is unavailable).
- Updated `ping_pong` and `fanout_fanin` benches to stop running `spargio_queue` variants.
- Updated README status text to describe io_uring-only backend.

Validation:

- `cargo fmt`
- `cargo test --features uring-native`
- `cargo bench --features uring-native --no-run`

Notes:

- Internal queue-oriented backend code paths remain in `ShardBackend` as dead code at this stage and are no longer instantiated through public builder/backend selection.
- Follow-up cleanup can remove those branches entirely if we want to reduce maintenance surface further.

## Update: internal queue backend branches removed

Follow-up cleanup completed after public queue-backend removal.

Changes:

- Removed internal `ShardBackend::Queue` handling branches from runtime dispatch.
- `ShardBackend` now routes only through io_uring paths in the Linux build.
- Removed queue-branch fallback logic in native submit handlers (`submit_native_*`).
- Removed shard-loop blocking idle wait path (`rx.recv_timeout(...)`), leaving nonblocking poll + cooperative yield behavior.
- Removed `RuntimeBuilder::idle_wait` field/method since it only supported the removed queue idle path.

Related API/harness alignment:

- `#[spargio::main(...)]` macro backend option now accepts only `"io_uring"`.
- Macro tests and examples updated accordingly.
- `ping_pong` and `fanout_fanin` benches no longer include `spargio_queue` variants.

Validation:

- `cargo fmt`
- `cargo test --features "uring-native macros"`
- `cargo bench --features uring-native --no-run`

Result:

- All checks pass.

## Update: blocking-surface plan slice implemented (Red/Green TDD)

Scope completed from the blocking-removal checklist:

- io_uring timer lane:
  - Added native timeout command path (`IORING_OP_TIMEOUT`) to the io_uring driver.
  - Added `UringNativeAny::sleep(Duration)`.
  - Routed top-level `spargio::sleep(...)` to shard-local native timeout path when running inside a Spargio shard; keeps fallback behavior outside shard context.

- Async-first boundary APIs:
  - Added async-first boundary surfaces:
    - `BoundaryClient::call_async(...)`
    - `BoundaryClient::call_async_with_timeout(...)`
    - `BoundaryServer::recv_async(...)`
    - `BoundaryServer::recv_timeout_async(...)`
    - `BoundaryTicket::wait_timeout(...)`
  - Kept blocking methods (`call`, `recv`, `recv_timeout`, `wait_timeout_blocking`) as compatibility wrappers.

- Address-resolution split:
  - Added explicit non-DNS socket-address APIs:
    - `net::TcpStream::connect_socket_addr(...)`
    - `net::TcpStream::connect_socket_addr_round_robin(...)`
    - `net::TcpStream::connect_many_socket_addr_round_robin(...)`
    - `net::TcpStream::connect_many_socket_addr_with_session_policy(...)`
    - `net::TcpStream::connect_socket_addr_with_session_policy(...)`
    - `net::TcpListener::bind_socket_addr(...)`
  - Kept hostname-based APIs as compatibility wrappers around a clearly named resolver path (`resolve_first_socket_addr_blocking`).

- Runtime-control async variants:
  - Added async runtime-entry/control APIs:
    - `run_async(...)`
    - `run_with_async(...)`
    - `Runtime::shutdown_async(...)`
  - Kept sync entry/control APIs (`run`, `run_with`, `shutdown`) for compatibility/ergonomics.

Red tests added:

- `tests/boundary_tdd.rs`
  - `boundary_async_call_and_recv_round_trip`
  - `boundary_async_recv_timeout_reports_timeout`
  - `boundary_ticket_wait_timeout_async_reports_timeout`
- `tests/runtime_tdd.rs`
  - `run_async_helper_executes_top_level_future`
  - `run_with_async_applies_custom_builder`
  - `runtime_shutdown_async_is_idempotent`
- `tests/ergonomics_tdd.rs`
  - `net_tcp_stream_connect_socket_addr_supports_read_write_all`
  - `net_tcp_listener_bind_socket_addr_accepts_and_wraps_stream`
- `tests/uring_native_tdd.rs`
  - `uring_native_unbound_sleep_uses_timeout_path`

Green + validation:

- `cargo fmt`
- `cargo test --features "uring-native macros" --test boundary_tdd --test runtime_tdd --test ergonomics_tdd --test uring_native_tdd`
- `cargo test --features "uring-native macros"`

Acceptance checklist status:

- [x] No data-plane helper-thread blocking waits in io_uring mode.
- [x] `sleep` uses native timeout path when io_uring backend is active on shard context.
- [x] Boundary APIs have async-first equivalents covering current usage.
- [x] Hostname resolution path is explicitly isolated from native data plane.
- [x] README/implementation log reflect which blocking APIs are compatibility-only vs removed.

## Update: removed public sync compatibility wrappers; async APIs are canonical (Red/Green TDD)

Rationale:

- Crate is not yet published; this is the lowest-risk point to make the API async-first and remove blocking wrapper surfaces.

What changed:

- Runtime entry/control API cleanup:
  - `run` is now async (`run(...).await`).
  - `run_with` is now async (`run_with(builder, ...).await`).
  - Removed public `run_async` and `run_with_async` aliases.
  - `Runtime::shutdown` is now async.
  - Removed public sync `Runtime::shutdown`; retained internal blocking shutdown path only for `Drop`.

- Boundary API cleanup:
  - `BoundaryClient::call` and `call_with_timeout` are async-first.
  - `BoundaryServer::recv` and `recv_timeout` are async-first.
  - `BoundaryTicket::wait_timeout` remains async.
  - Removed sync compatibility wrappers:
    - `BoundaryTicket::wait_timeout_blocking`
    - sync `BoundaryServer::recv`/`recv_timeout` wrappers
    - sync `BoundaryClient::call`/`call_with_timeout` wrappers

- Macro compatibility after async rename:
  - `#[spargio::main]` now uses a hidden `spargio::__private::block_on(...)` helper to invoke async `run_with(...)` from generated sync `main`.

- Examples/tests updated to new async API names:
  - boundary TDD switched to async call/recv/timeout paths.
  - runtime TDD switched to async `run`/`run_with`/`shutdown` usage.
  - `examples/network_work_stealing.rs` updated to async `run_with(...).await`.
  - `examples/mixed_mode_service.rs` updated for async boundary call path.

Validation:

- `cargo test --features "uring-native macros"`
- `cargo bench --features uring-native --no-run`

Result:

- Full test suite and benchmark target compilation pass after the async-first API break.

## Update: rotating-hotspot slowdown investigation plan (Tokio vs Spargio)

Question captured:

- Why are `net_stream_hotspot_rotation_4k` and `net_pipeline_hotspot_rotation_4k_window32` still faster on Tokio?

Current code-path findings:

- Both hotspot groups already use distributed stream setup in Spargio (`SpargioNetHarness::new_distributed()`), so this is not the earlier single-context concentration issue.
- Spargio hotspot stream path uses `send_all_batch + recv_multishot_segments (+ fallback read_exact_owned)`; Tokio uses simpler `write_all + read_exact` loops.
- Spargio pipeline hotspot path currently uses `write_all_owned/read_exact_owned` per frame and spawns per-stream jobs with generic `spawn_stealable`, not session-aligned placement.
- Native op submission still pays envelope/oneshot/tracking overhead per op when execution is off the stream session shard.

Working hypotheses for the current gap:

1. Placement mismatch in rotating-hotspot loops:
- per-stream tasks can execute off-session-shard (`spawn_stealable`), adding submit/reply overhead without enough skew persistence to amortize stealing wins.

2. Pipeline I/O method overhead:
- `write_all_owned/read_exact_owned` path has extra owned-buffer/method overhead in tight per-frame loops.

3. Multishot path may be suboptimal for this specific rotating shape:
- for short rotating bursts, multishot setup/segment handling may underperform simple exact-read loops.

4. Benchmark harness overhead differences:
- Tokio path uses a very lean inner loop and may currently benefit from less per-op user-space bookkeeping in this shape.

### Planned A/B matrix

A/B-1: task placement (both hotspot benchmarks)
- A: current `spawn_stealable`.
- B: `stream.spawn_stealable_on_session(...)`.
- C: `stream.spawn_on_session(...)`.

A/B-2: pipeline I/O method
- A: current `write_all_owned/read_exact_owned`.
- B: borrowed `write_all/read_exact` with reusable buffers.

A/B-3: stream-hotspot receive mode
- A: current multishot-first path.
- B: force read-exact path.

Execution plan:

1. Add experimental A/B benchmark lanes (net experiments target), no product-table changes yet.
2. Run targeted A/B for both hotspot benchmarks.
3. Implement only the winning changes into the main benchmark/runtime paths.
4. Keep TDD discipline: add failing tests for any API/runtime behavior changes, then implement to green.

## Update: rotating-hotspot A/B results + adopted optimizations

Executed the planned A/B matrix in `benches/net_experiments.rs`:

- `exp_net_stream_hotspot_rotation_ab_4k`
- `exp_net_pipeline_hotspot_rotation_ab_4k_window32`

Command set:

- `cargo bench --features uring-native --bench net_experiments -- exp_net_stream_hotspot_rotation_ab_4k --sample-size 12`
- `cargo bench --features uring-native --bench net_experiments -- exp_net_pipeline_hotspot_rotation_ab_4k_window32 --sample-size 12`

### A/B findings

`exp_net_stream_hotspot_rotation_ab_4k`:

- `tokio_hotspot_rotation`: `8.7424-8.8669 ms`
- `spargio_hotspot_stealable_multishot`: `11.667-11.801 ms`
- `spargio_hotspot_stealable_session_multishot`: `11.705-11.967 ms`
- `spargio_hotspot_pinned_multishot`: `9.8044-9.9619 ms`
- `spargio_hotspot_pinned_readexact`: `9.5227-9.5928 ms`

Interpretation:

- Session-pinned placement is the main gain for this shape.
- For rotating hotspot stream-only traffic, read-exact outperforms multishot.
- Stealable-session-preferred did not beat pinned here.

`exp_net_pipeline_hotspot_rotation_ab_4k_window32`:

- `tokio_pipeline_hotspot`: `26.473-26.678 ms`
- `spargio_pipeline_stealable_owned`: `32.167-32.563 ms`
- `spargio_pipeline_stealable_session_owned`: `32.356-32.844 ms`
- `spargio_pipeline_pinned_owned`: `29.618-30.016 ms`
- `spargio_pipeline_pinned_borrowed`: `30.080-30.247 ms`

Interpretation:

- Session-pinned placement is again the primary improvement.
- Owned I/O loop stays slightly better than borrowed mode in this pipeline shape.

### Optimizations implemented from A/B

Applied to product benchmark path (`benches/net_api.rs`):

1. `net_stream_hotspot_rotation_4k`:
- per-stream work now runs with `stream.spawn_on_session(...)` (session-pinned placement).
- receive mode switched to read-exact for this rotating stream-hotspot workload.

2. `net_pipeline_hotspot_rotation_4k_window32`:
- per-stream work now runs with `stream.spawn_on_session(...)` (session-pinned placement).
- kept owned I/O loop (`write_all_owned/read_exact_owned`) as the better A/B mode.

3. Kept existing defaults unchanged where A/B did not indicate improvement:
- throughput/imbalanced hot path remains multishot-first.
- generic stealable placement remains for non-hotspot benchmark paths.

### Post-optimization benchmark snapshots (`net_api`)

Commands:

- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 12`

Results:

- `net_stream_hotspot_rotation_4k/tokio_tcp_8streams_rotating_hotspot`: `8.6989-8.7937 ms`
- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`: `9.5875-9.8201 ms`
- `net_stream_hotspot_rotation_4k/compio_tcp_8streams_rotating_hotspot`: `16.782-17.053 ms`

- `net_pipeline_hotspot_rotation_4k_window32/tokio_tcp_pipeline_hotspot`: `26.328-26.504 ms`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`: `29.411-29.919 ms`
- `net_pipeline_hotspot_rotation_4k_window32/compio_tcp_pipeline_hotspot`: `50.787-51.425 ms`

Net effect vs prior `net_api` snapshots:

- Stream rotating-hotspot: Spargio improved materially (about 14-16% faster) and moved closer to Tokio.
- Pipeline rotating-hotspot: Spargio improved materially (about 8-11% faster) and moved closer to Tokio.
- Both workloads still trail Tokio, but the remaining gap is substantially smaller than before.
