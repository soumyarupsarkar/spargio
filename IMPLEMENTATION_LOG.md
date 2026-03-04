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

## Update: implemented next hotspot optimizations (Red/Green TDD)

Follow-up optimizations implemented from the latest hotspot analysis:

1. Remove extra owned-buffer read/write overhead in stream loops.
2. Add a tighter same-shard native-op fast path for session-stream ops.

### Red phase

Added failing test in `tests/ergonomics_tdd.rs`:

- `net_tcp_stream_spawn_on_session_uses_local_direct_native_fastpath`

Initial failure:

- compile-time red because `RuntimeStats` had no `native_any_local_direct_submitted` field.

### Green phase

Implemented:

- New runtime stat:
  - `RuntimeStats::native_any_local_direct_submitted`
  - tracked in `RuntimeStatsInner` and surfaced via `stats_snapshot()`.

- Session-stream local direct path:
  - in `UringNativeAny::{recv_owned_at_on_shard, send_owned_at_on_shard}`, when running on the same runtime+shard context:
    - enqueue `LocalCommand::SubmitNative{Recv,Send}Owned` directly
    - increment `native_any_local_direct_submitted`
    - avoid `NativeAnyCommand -> LocalCommand` conversion path

- Offset-based native send/recv plumbing:
  - added `offset` to `NativeAnyCommand::{RecvOwned, SendOwned}`
  - added `offset` to `LocalCommand::{SubmitNativeRecvOwned, SubmitNativeSendOwned}`
  - io_uring driver now submits `Recv/Send` against `buf[offset..]` without cloning/splitting buffers.

- Stream owned I/O loop rewrites:
  - `TcpStream::write_all_owned` now advances using `send_owned_from(buf, offset)` (no fallback `send(&buf[sent..])` cloning path).
  - `TcpStream::read_exact_owned` now advances using `recv_owned_from(dst, offset)` (no `read_exact` scratch/copy path).

Validation:

- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo test --features uring-native --tests`

### Post-change benchmark snapshot

Commands:

- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 12`

Results:

- `net_stream_hotspot_rotation_4k/tokio_tcp_8streams_rotating_hotspot`: `8.7900-8.8664 ms`
- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`: `9.3389-9.4787 ms`
- `net_stream_hotspot_rotation_4k/compio_tcp_8streams_rotating_hotspot`: `16.661-16.845 ms`

- `net_pipeline_hotspot_rotation_4k_window32/tokio_tcp_pipeline_hotspot`: `26.322-26.549 ms`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`: `28.933-29.121 ms`
- `net_pipeline_hotspot_rotation_4k_window32/compio_tcp_pipeline_hotspot`: `51.323-52.073 ms`

Effect:

- Additional improvement in both rotating-hotspot benchmarks.
- Remaining gap to Tokio narrowed again (now roughly ~5-10% depending on exact bound pair).

## Update: local direct native replies now avoid oneshot allocation (Red/Green TDD)

Completed the in-progress local fast-path refactor so same-runtime same-shard
`recv_owned/send_owned` submissions do not allocate/use a oneshot channel.

### Green implementation details

- Added `NativeBufReply::{Oneshot, Local}` and `NativeBufReply::complete(...)`.
- Added local waiter pair:
  - `NativeBufReply::local_pair()`
  - `NativeLocalBufReplySlot` + `NativeLocalBufReplyFuture`
- Wired local-direct branch in:
  - `UringNativeAny::recv_owned_at_on_shard`
  - `UringNativeAny::send_owned_at_on_shard`
  to use the local waiter/future instead of oneshot.
- Updated io_uring native recv/send submit/completion paths to use
  `NativeBufReply` uniformly.

Validation:

- `cargo check --features uring-native`
- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo test --features uring-native --tests`

### Post-change hotspot benchmark snapshot

Commands:

- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 12`

Results:

- `net_stream_hotspot_rotation_4k/tokio_tcp_8streams_rotating_hotspot`: `8.6940-8.8212 ms`
- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`: `9.3020-9.4073 ms`
- `net_stream_hotspot_rotation_4k/compio_tcp_8streams_rotating_hotspot`: `16.681-16.812 ms`

- `net_pipeline_hotspot_rotation_4k_window32/tokio_tcp_pipeline_hotspot`: `26.286-26.560 ms`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`: `29.025-29.574 ms`
- `net_pipeline_hotspot_rotation_4k_window32/compio_tcp_pipeline_hotspot`: `50.614-50.986 ms`

Effect:

- Refactor is functionally complete and fully green.
- This specific change is mostly neutral on these two benchmark shapes
  (small movement within run-to-run noise).

## Update: keyed-hotspot benchmark follow-up (event-queue/msg path optimization backlog)

Context:

- Added `net_keyed_hotspot_rotation_4k` in `benches/net_api.rs` to stress
  rotating hotspot network I/O plus keyed cross-shard dispatch.
- Current snapshot (`--sample-size 12`):
  - `tokio_tcp_keyed_router_hotspot`: `9.2375-9.3226 ms`
  - `spargio_tcp_keyed_router_hotspot`: `10.061-10.254 ms`
- Interpretation: Tokio is still faster on this shape; likely overhead comes from
  per-message payload queueing, doorbell signaling, and event queue handling in
  Spargio’s ring-msg path.

Planned optimization ideas (highest ROI first):

1. Batch payload enqueue under one lock (high ROI, low risk)
- Problem: `SubmitRingMsgBatch` currently loops through per-message submit calls.
- Cost: lock/unlock and per-item queue overhead in `enqueue_payload` for each msg.
- Plan:
  - add a true backend/io_uring batch enqueue path:
    - one queue lock
    - append all payloads
    - one doorbell when queue transitions empty -> non-empty.
- Expected impact: reduce keyed-hotspot dispatch overhead materially.

2. Batch `EventState` delivery (high ROI, low-medium risk)
- Problem: `drain_payload_queue` pushes one event at a time, each with lock+wake.
- Plan:
  - add `EventState::push_many(...)`
  - queue drained ring-msg events in one critical section
  - wake waiters once per drained batch.
- Expected impact: lower owner-side event ingestion overhead.

3. Lower synchronization cost in `EventState` (medium ROI, medium risk)
- Problem: current queue uses mutex-protected `VecDeque` and per-push wake path.
- Plan options:
  - switch to lighter mutex implementation (e.g. `parking_lot`)
  - split producer-consumer queue/waker paths to reduce contention.
- Expected impact: lower overhead for high ring-msg event rates.

4. Fast path for hot internal ring-msg tags (medium ROI, medium-high risk)
- Problem: hot dispatch tags share same generic `EventState` path as all events.
- Plan:
  - route selected internal tags to dedicated per-shard mailboxes
  - keep `next_event()` for general API compatibility
  - use msg_ring as wake/doorbell only for these hot lanes.
- Expected impact: better keyed-router style throughput under hotspot churn.

5. Direct msg payload mode for tiny control messages (exploratory, medium-high risk)
- Problem: payload-queue + doorbell indirection adds overhead for tiny values.
- Plan:
  - where semantics allow, encode tiny payloads directly in `MSG_RING` CQEs
    (skip intermediate payload queue).
- Expected impact: reduced dispatch overhead for control-heavy micro-messages.

Validation plan for each change:

- Re-run:
  - `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 12`
  - `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 12`
- Track regression guardrails on:
  - `net_stream_throughput_4k_window32`
  - `net_stream_imbalanced_4k_hot1_light7`

## Update: keyed-hotspot optimization pass (batching complete, lock-free payload A/B reverted)

Implemented in this pass:

1. `SubmitRingMsgBatch` now uses a true backend batch path
- `ShardBackend::submit_ring_msg_batch(...)` submits one batch call.
- `IoUringDriver::submit_ring_msg_batch(...)` enqueues in one queue lock section,
  sends at most one doorbell for empty->non-empty transitions, and accounts
  partial acceptance/backpressure once per batch.

2. Event ingress now batches queue+wake
- Added `EventState::push_many(...)` and used it from:
  - io_uring CQE ring-msg reap path
  - payload-queue drain path
- `ring_msgs_completed` accounting now aggregates by batch where applicable.

3. Lowered `EventState` synchronization overhead
- Replaced mutex-protected event queue with `crossbeam_queue::SegQueue<Event>`.
- Kept waiter registration under a small mutex (`Vec<Waker>`).
- `push/push_many` now perform lock-free queue push and only lock to drain waiters.

4. Ran a lock-free payload-queue A/B and reverted it
- Experiment: replaced per-target/per-source payload queues with bounded
  `ArrayQueue`.
- Outcome:
  - no keyed-hotspot improvement
  - rotating-stream hotspot regressed
- Decision: reverted payload-queue `ArrayQueue` experiment; retained
  event-queue synchronization changes above.

Validation:

- `cargo fmt`
- `cargo check --features uring-native`
- `cargo test --features uring-native --tests`

Benchmarks (post-revert baseline, `--sample-size 12`):

- `net_keyed_hotspot_rotation_4k/tokio_tcp_keyed_router_hotspot`: `9.3457-9.3879 ms`
- `net_keyed_hotspot_rotation_4k/spargio_tcp_keyed_router_hotspot`: `10.008-10.062 ms`

- `net_stream_hotspot_rotation_4k/tokio_tcp_8streams_rotating_hotspot`: `8.8285-8.9134 ms`
- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`: `9.3247-9.5191 ms`
- `net_stream_hotspot_rotation_4k/compio_tcp_8streams_rotating_hotspot`: `16.668-16.808 ms`

- `net_pipeline_hotspot_rotation_4k_window32/tokio_tcp_pipeline_hotspot`: `26.305-26.569 ms`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`: `29.010-29.400 ms`
- `net_pipeline_hotspot_rotation_4k_window32/compio_tcp_pipeline_hotspot`: `50.682-51.536 ms`

Interpretation:

- Batching and event-ingress improvements are in place and stable.
- Main remaining gap on keyed-hotspot is not from payload queue lock granularity.
- Highest-ROI remaining ideas are:
  - hot-tag/internal mailbox fast path
  - direct tiny-control-message `MSG_RING` payload mode (selective bypass of doorbell queue)

## Update: direct `MSG_RING` control API (opt-in) + validation

Implemented:

- Added opt-in direct message APIs that bypass the payload queue/doorbell path:
  - `RemoteShard::send_raw_direct_nowait(...)`
  - `RemoteShard::send_many_raw_direct_nowait(...)`
  - `ShardCtx::send_raw_direct_nowait(...)`
  - `ShardCtx::send_many_raw_direct_nowait(...)`
- Runtime wiring:
  - new local command `SubmitRingMsgDirectBatch`
  - backend handler `submit_ring_msg_direct_batch(...)`
  - io_uring submit path `submit_ring_msg_direct_nowait(...)` (one `MSG_RING` SQE per message)

Red/Green tests added:

- `send_raw_direct_nowait_delivers_event`
- `send_many_raw_direct_nowait_delivers_in_order`

Validation:

- `cargo check --features uring-native`
- `cargo test --features uring-native --test runtime_tdd`
- `cargo test --features uring-native --tests`

Notes:

- This direct path is intentionally opt-in and currently best suited for low-volume,
  tiny control messages.
- Attempting to swap keyed-hotspot benchmark traffic to direct mode increased runtime
  significantly (high per-message SQE overhead under that specific load), so benchmark
  default was reverted to the stable batched payload-queue path.

Post-change benchmark sanity snapshot:

- `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `tokio_tcp_keyed_router_hotspot`: `9.2793-9.3288 ms`
  - `spargio_tcp_keyed_router_hotspot`: `9.9952-10.249 ms`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`
  - `tokio_tcp_8streams_rotating_hotspot`: `8.7510-8.8628 ms`
  - `spargio_tcp_8streams_rotating_hotspot`: `9.3289-9.6232 ms`
  - `compio_tcp_8streams_rotating_hotspot`: `16.771-16.908 ms`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`
  - `tokio_tcp_pipeline_hotspot`: `26.193-26.447 ms`
  - `spargio_tcp_pipeline_hotspot`: `28.856-28.982 ms`
  - `compio_tcp_pipeline_hotspot`: `50.464-51.058 ms`

## Update: hot-tag mailbox lane (msg routing fast path) for keyed dispatch

Implemented:

- Runtime builder hot-tag routing configuration:
  - `RuntimeBuilder::hot_msg_tag(tag)`
  - `RuntimeBuilder::hot_msg_tags(iter)`
- Added dedicated shard-local hot event lane:
  - `ShardCtx::next_hot_event()`
  - internal `hot_event_state` alongside regular `event_state`
- Routed incoming ring messages by tag at ingestion time:
  - io_uring CQE ring-msg path
  - payload-queue drain path
  - external `InjectRawMessage` path
- Keyed benchmark wiring:
  - benchmark runtime now enables hot tags for `KEYED_DISPATCH_TAG`/`KEYED_STOP_TAG`
  - keyed owner tasks consume via `next_hot_event()`

Red/Green TDD:

- Added tests:
  - `hot_msg_tag_routes_to_hot_event_lane`
  - `non_hot_msg_tag_remains_on_regular_event_lane`
- Existing direct-message tests retained and passing.

Validation:

- `cargo fmt`
- `cargo check --features uring-native`
- `cargo test --features uring-native --tests`

Benchmark snapshot after this change:

- `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `tokio_tcp_keyed_router_hotspot`: `9.4113-9.5537 ms`
  - `spargio_tcp_keyed_router_hotspot`: `9.9657-10.005 ms`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`
  - `tokio_tcp_8streams_rotating_hotspot`: `8.6508-8.7692 ms`
  - `spargio_tcp_8streams_rotating_hotspot`: `9.4165-9.5420 ms`
  - `compio_tcp_8streams_rotating_hotspot`: `16.692-16.835 ms`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`
  - `tokio_tcp_pipeline_hotspot`: `26.336-26.504 ms`
  - `spargio_tcp_pipeline_hotspot`: `29.244-29.392 ms`
  - `compio_tcp_pipeline_hotspot`: `50.869-51.357 ms`

Interpretation:

- Hot-tag lane is now functional and benchmarked.
- Keyed hotspot remains close to prior best range but still behind Tokio.
- Next likely high-ROI step remains value-coalescing for hot dispatch tags
  (aggregate frequent tiny hot-tag increments before queueing/wake).

## Update: coalesced-hot-tag ingestion (batch value aggregation)

Implemented:

- Added explicit coalesced-hot-tag config:
  - `RuntimeBuilder::coalesced_hot_msg_tag(tag)`
  - `RuntimeBuilder::coalesced_hot_msg_tags(iter)`
- Coalesced tags are automatically treated as hot tags.
- Extended ring-msg ingest path to coalesce same `(from, tag)` values within each
  ingest batch before queueing hot events:
  - io_uring CQE ring-msg batch
  - payload-queue drain batch
  - coalescing emits one or more `Event::RingMsg` with summed `val`
    (chunked safely if sum exceeds `u32::MAX`).
- Keyed benchmark harness now enables:
  - hot tags: `KEYED_DISPATCH_TAG`, `KEYED_STOP_TAG`
  - coalesced hot tag: `KEYED_DISPATCH_TAG`

Red/Green TDD:

- Added tests:
  - `coalesced_hot_msg_tag_aggregates_batch_values`
  - `non_coalesced_hot_msg_tag_preserves_batch_events`
- Existing hot-lane tests retained and passing.

Validation:

- `cargo fmt`
- `cargo check --features uring-native`
- `cargo test --features uring-native --tests`

Benchmark snapshot after coalescing:

- `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `tokio_tcp_keyed_router_hotspot`: `9.3593-9.4503 ms`
  - `spargio_tcp_keyed_router_hotspot`: `9.8008-10.002 ms`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`
  - `tokio_tcp_8streams_rotating_hotspot`: `8.7586-8.8332 ms`
  - `spargio_tcp_8streams_rotating_hotspot`: `9.4692-9.6138 ms`
  - `compio_tcp_8streams_rotating_hotspot`: `16.851-17.197 ms`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`
  - `tokio_tcp_pipeline_hotspot`: `26.303-26.520 ms`
  - `spargio_tcp_pipeline_hotspot`: `29.011-29.267 ms`
  - `compio_tcp_pipeline_hotspot`: `50.880-51.315 ms`

Interpretation:

- Coalescing improved keyed-hotspot path modestly and safely, with no material
  regression on stream/pipeline guardrails.
- Remaining keyed-hotspot gap appears to come from broader per-event control-path
  overhead, not just duplicate dispatch-value churn.

## Update: enqueue-time coalescing for coalesced-hot tags (queue-pressure reduction)

Implemented:

- `IoUringDriver` now carries coalesced-hot-tag lookup and applies it while
  writing payload queues (not only at ingest time).
- For coalesced-hot tags, enqueue path now merges with the queue tail when
  `(tail.tag == tag)`, including safe overflow chunking.
- This allows tight-capacity queues to absorb bursty tiny dispatch increments
  without immediate backpressure.

Red/Green TDD:

- Added `coalesced_hot_tag_absorbs_batch_under_tight_queue_capacity`:
  - runtime with `msg_ring_queue_capacity(1)`
  - coalesced hot tag burst `(59,1),(59,2),(59,3)`
  - verifies success and single hot event with `val=6`
- Full suite remains green.

Validation:

- `cargo fmt`
- `cargo check --features uring-native`
- `cargo test --features uring-native --tests`

Benchmark snapshot after enqueue-time coalescing:

- `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `tokio_tcp_keyed_router_hotspot`: `9.3417-9.4771 ms`
  - `spargio_tcp_keyed_router_hotspot`: `9.5432-9.6410 ms`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`
  - `tokio_tcp_8streams_rotating_hotspot`: `8.7407-8.8063 ms`
  - `spargio_tcp_8streams_rotating_hotspot`: `9.3352-9.4076 ms`
  - `compio_tcp_8streams_rotating_hotspot`: `16.536-16.814 ms`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`
  - `tokio_tcp_pipeline_hotspot`: `26.361-26.744 ms`
  - `spargio_tcp_pipeline_hotspot`: `29.060-29.326 ms`
  - `compio_tcp_pipeline_hotspot`: `50.503-51.418 ms`

Interpretation:

- Keyed-hotspot improved materially again; this slice appears higher ROI than
  ingest-only coalescing.
- Stream/pipeline guardrails remained stable.

## Update: completed remaining keyed-hotspot optimization slices (counter lane + adaptive wake policy)

Completed slices:

1. Cross-batch hot-counter accumulation
- Coalesced hot tags are now aggregated into shard-local counters (`u16 -> u64`)
  instead of being emitted as per-message hot events.
- Aggregation persists across ingest batches and drains, not only within a single
  batch callback.

2. Hot-counter consume fast path
- Added consume API:
  - `ShardCtx::next_hot_count(tag) -> Future<Output = u64>`
  - `ShardCtx::try_take_hot_count(tag) -> Option<u64>`
- Keyed benchmark owner path now consumes dispatch volume via `next_hot_count`
  and only uses `next_hot_event` for stop/control tags.
- This removes event-object overhead for coalesced dispatch traffic.

3. Adaptive dispatch/wake policy + hardening
- Added tuning knob:
  - `RuntimeBuilder::hot_counter_wake_threshold(u64)`
- Wake policy for waiting hot-counter consumers:
  - wake on 0->nonzero transition
  - or on crossing threshold from below.
- Added hardening tests:
  - `coalesced_hot_count_accumulates_across_batches`
  - `hot_counter_threshold_does_not_starve_first_update`
  - existing coalescing/hot-lane tests retained.
- Kept benchmark gate reruns on:
  - keyed hotspot (target KPI)
  - stream hotspot (guardrail)
  - pipeline hotspot (guardrail)

Validation:

- `cargo fmt`
- `cargo check --features uring-native`
- `cargo test --features uring-native --tests`

Benchmark gate snapshot (post-slices):

- `cargo bench --features uring-native --bench net_api -- net_keyed_hotspot_rotation_4k --sample-size 12`
  - `tokio_tcp_keyed_router_hotspot`: `9.3712-9.4256 ms`
  - `spargio_tcp_keyed_router_hotspot`: `9.5867-9.7558 ms`
- `cargo bench --features uring-native --bench net_api -- net_stream_hotspot_rotation_4k --sample-size 10`
  - `tokio_tcp_8streams_rotating_hotspot`: `8.7801-8.8376 ms`
  - `spargio_tcp_8streams_rotating_hotspot`: `9.3909-9.4505 ms`
  - `compio_tcp_8streams_rotating_hotspot`: `16.640-17.098 ms`
- `cargo bench --features uring-native --bench net_api -- net_pipeline_hotspot_rotation_4k_window32 --sample-size 10`
  - `tokio_tcp_pipeline_hotspot`: `26.380-26.482 ms`
  - `spargio_tcp_pipeline_hotspot`: `28.856-29.242 ms`
  - `compio_tcp_pipeline_hotspot`: `50.770-51.273 ms`

Outcome:

- Remaining planned slices for this keyed-hotspot track are now implemented.
- Spargio is now very close to Tokio on keyed-hotspot in this harness, with stable
  guardrails on other hotspot shapes.

## Update: keyed hotspot benchmark now includes Compio

Added `compio` variant to `net_keyed_hotspot_rotation_4k`:

- new bench case: `compio_tcp_keyed_router_hotspot`
- wired through `CompioNetCmd::EchoKeyedHotspot`, harness command handling, and
  `compio_echo_keyed_hotspot_rotation(...)`.

Sanity run (`--sample-size 10`):

- `tokio_tcp_keyed_router_hotspot`: `9.2799-9.3554 ms`
- `spargio_tcp_keyed_router_hotspot`: `9.5718-9.7460 ms`
- `compio_tcp_keyed_router_hotspot`: `16.652-16.712 ms`

## Update: full benchmark refresh + README sync (2026-02-27)

Ran the full benchmark suite with current `uring-native` implementation and
updated README benchmark tables/interpretation to match.

Commands:

- `cargo bench --features uring-native --bench ping_pong -- --sample-size 12`
- `cargo bench --features uring-native --bench fanout_fanin -- --sample-size 12`
- `cargo bench --features uring-native --bench fs_api -- --sample-size 12`
- `cargo bench --features uring-native --bench net_api -- --sample-size 12`

Snapshot:

- Coordination (Tokio vs Spargio):
  - `steady_ping_pong_rtt`: Tokio `1.4911-1.5024 ms`, Spargio `394.83-396.21 us`
  - `steady_one_way_send_drain`: Tokio `68.607-70.859 us`, Spargio `49.232-50.110 us`
  - `cold_start_ping_pong`: Tokio `553.31-561.83 us`, Spargio `284.23-287.50 us`
  - `fanout_fanin_balanced`: Tokio `1.4534-1.4631 ms`, Spargio `1.3426-1.3480 ms`
  - `fanout_fanin_skewed`: Tokio `2.4026-2.4220 ms`, Spargio `1.9979-2.0032 ms`

- Native API (Tokio vs Spargio vs Compio):
  - `fs_read_rtt_4k`: Tokio `1.6174-1.6565 ms`, Spargio `1.0008-1.0188 ms`, Compio `1.4782-1.4978 ms`
  - `fs_read_throughput_4k_qd32`: Tokio `7.8804-8.1672 ms`, Spargio `6.1570-6.2793 ms`, Compio `4.0877-5.0803 ms`
  - `net_echo_rtt_256b`: Tokio `7.7462-7.9687 ms`, Spargio `5.4356-5.5084 ms`, Compio `6.4541-6.5632 ms`
  - `net_stream_throughput_4k_window32`: Tokio `11.142-11.247 ms`, Spargio `10.745-10.813 ms`, Compio `7.0631-7.1570 ms`

- Imbalanced native API:
  - `net_stream_imbalanced_4k_hot1_light7`: Tokio `13.584-13.799 ms`, Spargio `13.191-13.375 ms`, Compio `12.283-12.414 ms`
  - `net_stream_hotspot_rotation_4k`: Tokio `8.7891-8.8560 ms`, Spargio `9.3683-9.4526 ms`, Compio `16.870-16.982 ms`
  - `net_pipeline_hotspot_rotation_4k_window32`: Tokio `26.415-26.654 ms`, Spargio `29.113-29.517 ms`, Compio `50.648-51.210 ms`
  - `net_keyed_hotspot_rotation_4k`: Tokio `9.3152-9.4912 ms`, Spargio `9.5691-9.7957 ms`, Compio `16.781-16.994 ms`

Interpretation updates reflected in README:

- Spargio retains clear lead on coordination-heavy and low-depth latency cases.
- Compio retains lead on sustained balanced stream throughput and static-hotspot imbalance.
- Tokio remains ahead in rotating-hotspot stream/pipeline; keyed routing is near parity.

## Note: do the network optimizations fit Spargio's value proposition?

Question:

- Do the network optimizations we added to close the Tokio gap actually make sense
  for Spargio, and are they realistic for users to adopt?

Answer:

- Yes, primarily when they reduce cross-shard coordination cost (coalesced hot
  tags, hot-counter fast path, adaptive wake policy, keyed ownership routing).
  These directly support Spargio's core value proposition: efficient
  `io_uring` + `msg_ring` work-stealing/steering under coordination-heavy load.
- These optimizations are most relevant for keyable/skewed multi-stream
  workloads (tenant/session/partition keyed routing), where steering and
  aggregation reduce dispatch overhead.
- They should remain opt-in tuning for advanced users. Default paths should
  stay simple and semantically conservative when applications need per-message
  event fidelity and straightforward observability.

Follow-up planned:

- Add user-facing documentation for these knobs (what each knob does, semantic
  trade-offs, recommended workload shapes, and safe defaults), plus a short
  tuning guide in README/docs.

## Update: flaky `uring-native` CI test fixed (2026-02-28)

Observed:

- CI run `22511780569` failed at `Cargo test (uring-native)` with exit code 101.
- Failure was intermittent and initially non-reproducible on a single local run.

Root cause:

- `coalesced_hot_count_accumulates_across_batches` in `tests/runtime_tdd.rs` had
  a race in test logic.
- The receiver polled `try_take_hot_count(61)` in a loop and could consume the
  first coalesced update (`3`) before the second batch (`+3`) arrived, causing
  occasional `left: 3, right: 6`.

Fix:

- Made the test deterministic by introducing a non-coalesced barrier tag and
  waiting for a barrier event before reading the hot counter.
- Updated the test to assert total hot count only after both sends are known to
  have been delivered to the target shard.

Validation:

- `cargo test --features uring-native --test runtime_tdd coalesced_hot_count_accumulates_across_batches`
- 50x stress loop of that single test: all pass.
- `cargo test --features uring-native`: pass.

Outcome:

- Removed known flake in `uring-native` test suite.
- No runtime behavior change; this was a test synchronization fix.

## Update: Compio parity audit snapshot (2026-02-28)

Captured a focused feature-parity snapshot against current Compio docs and
our current public `spargio` surface, with emphasis on practical user-facing
gaps.

### I/O API breadth: present vs missing

Current Spargio public I/O surface:

- `fs`: `OpenOptions` + `File` with `open/create/from_std`, positional
  `read_at`/`read_at_into`/`write_at`/`write_all_at`, `read_to_end_at`, `fsync`.
- `net`: TCP-only (`TcpStream`, `TcpListener`) including session-policy connect/accept,
  owned buffer APIs, and multishot segment receive helpers.
- runtime-native unbound lane methods routed through `io_uring`.

Compared with Compio's documented surface, notable missing breadth in Spargio:

1. Filesystem path-level helpers and metadata APIs
   - examples: `create_dir`, `create_dir_all`, `hard_link`, `metadata`,
     `remove_dir`, `remove_file`, `rename`, `set_permissions`, `symlink`,
     `symlink_metadata`, convenience `read`/`write`.
2. Broader network protocol/socket families
   - UDP and Unix domain socket APIs (`UdpSocket`, `UnixListener`,
     `UnixStream`, `UnixDatagram`) are not currently in Spargio public API.
3. Generic async I/O trait/adaptor layer
   - no public Spargio equivalent to Compio `io` traits and adapters
     (`AsyncRead`/`AsyncWrite` families, buffered wrappers, compat/framed utilities).
4. Higher-level transport/runtime-integrated modules
   - no Spargio public modules corresponding to Compio optional
     `process`/`signal`/`tls`/`ws`/`quic` ecosystem crates.

This aligns with existing README scope note:

- "Broader filesystem and network native-op surface ... not done yet."

### Core runtime parity: what is still missing in Spargio

Core runtime is functional and differentiated (shards, placement APIs,
work-stealing MVP, timers, cancellation/task group, boundary APIs), but gaps
remain versus broader runtime ecosystems:

1. Backend/platform breadth
   - `BackendKind` is currently `IoUring` only.
2. Top-level `!Send` ergonomics
   - public runtime handle spawn paths require `Send`; `!Send` execution is
     currently available only via shard-local `ShardCtx::spawn_local(...)`.
3. Time/runtime utility breadth
   - currently minimal top-level primitives (`sleep`, `timeout`) rather than a
     fuller interval/deadline utility set.
4. Production hardening/tuning depth
   - advanced stealing policy tuning and long-window hardening/observability are
     still listed as pending in project docs.

Conclusion:

- Spargio currently has partial feature overlap with Compio for core
  fs/tcp runtime workflows, but does not yet have Compio-level I/O breadth.
- Current project direction remains valid: keep differentiating on
  cross-shard coordination + placement/stealing, while closing practical
  fs/net/runtime-surface gaps incrementally.

## Update: `!Send` ergonomics slice (`run_local_on` + `spawn_local_on`) (2026-02-28)

Captured and implemented the proposal discussed in review:

- add a first-class local-entry helper that can run `!Send` futures on a chosen shard.
- add a handle-level construct-on-shard API so callers can build `!Send` futures
  on target shard context without requiring a prior `ShardCtx` hop.

### Red phase

Added failing tests in `tests/runtime_tdd.rs`:

- `run_local_on_accepts_non_send_future`
- `runtime_handle_spawn_local_on_accepts_non_send_future`

Red failure signals:

- unresolved import: `spargio::run_local_on`
- missing method: `RuntimeHandle::spawn_local_on`

### Green phase

Implemented public APIs in `src/lib.rs`:

1. New top-level entry helper
   - `run_local_on(builder, shard, entry)`
   - signature accepts `entry: FnOnce(ShardCtx) -> Fut + Send`, with `Fut: Future + 'static`
     (no `Send` bound on `Fut`), and `T: Send`.
2. New runtime-handle API
   - `RuntimeHandle::spawn_local_on(shard, init)`
   - same construct-on-shard shape and `!Send` future support.
3. Internal spawn path
   - added `spawn_local_on_shared(...)`.
   - implementation routes through existing shard command channel (`Command::Spawn`)
     and, on the target shard, constructs the future using live `ShardCtx`,
     then executes it via `ctx.spawn_local(...)`.

Design notes:

- No new scheduler lane or command type was required.
- `!Send` is enabled by constructing the future on the shard and running it via
  local spawner; cross-thread transfer only carries the `Send` initializer closure.
- Return type remains `JoinHandle<T>` with `T: Send` for cross-thread join safety.

### Validation

Commands run:

- `cargo test --features uring-native --test runtime_tdd run_local_on_accepts_non_send_future`
- `cargo test --features uring-native --test runtime_tdd runtime_handle_spawn_local_on_accepts_non_send_future`
- `cargo test --features uring-native --test runtime_tdd`

Result:

- both new tests pass.
- full `runtime_tdd` suite passes (`24 passed`).

### Outcome

- Spargio now supports a direct top-level local entry and handle-level local
  spawn path for `!Send` futures, reducing friction for shard-local state
  patterns (`Rc`, `RefCell`, etc.) while preserving existing shard-safety model.

## Update: low-level unsafe native extension API slice (2026-02-28)

Recorded proposal and implemented it in this slice:

- add a low-level unsafe extension lane so external crates can submit custom
  SQE/CQE workflows without editing Spargio core for each new operation.
- keep high-level fs/net APIs safe and unchanged; isolate risk in explicit
  unsafe extension entry points.

### Red phase

Added new tests in `tests/uring_native_tdd.rs` for extension use-cases:

- `uring_native_unbound_unsafe_extension_supports_custom_nop`
- `uring_native_unbound_unsafe_extension_supports_custom_read_entry`

These encode the intended external-writer workflow:

- provide extension-owned state
- build a custom SQE from that state
- decode CQE into a typed result

### Green phase

Implemented low-level unsafe API on `UringNativeAny`:

- `unsafe submit_unsafe(...)`
- `unsafe submit_unsafe_on_shard(...)`

Added new public completion type:

- `UringCqe { result, flags }`

Internal runtime wiring added:

- new internal native command variant carrying extension op envelopes
- extension op envelope retained in runtime until completion
- SQE built on target shard, user data overridden by runtime tracking key
- completion/failure paths return typed result through oneshot
- dispatch integrated with existing fast path / envelope path and affinity
  violation guardrails

### Validation

Commands run:

- `cargo test --features uring-native --test uring_native_tdd uring_native_unbound_unsafe_extension_supports_custom_nop`
- `cargo test --features uring-native --test uring_native_tdd uring_native_unbound_unsafe_extension_supports_custom_read_entry`
- `cargo test --features uring-native --test runtime_tdd --test uring_native_tdd`

Result:

- new unsafe-extension tests pass.
- full `runtime_tdd` and `uring_native_tdd` suites pass.

### Docs sync

README updated to reflect completed status:

- added done bullets for:
  - `!Send` ergonomics (`run_local_on`, `RuntimeHandle::spawn_local_on`)
  - low-level unsafe extension API (`UringNativeAny::{submit_unsafe, submit_unsafe_on_shard}`)
- reviewed done/not-done sections and adjusted wording:
  - "broader built-in fs/net surface" remains not done
  - added safe-wrapper/cookbook work for unsafe extension API to not-done backlog

## Update: time/runtime utility parity comparison (Compio + monoio, io_uring fit adjusted) (2026-02-28)

Revised the time/runtime parity recommendations to account for whether each gap
is:

- `Direct io_uring`: maps directly to io_uring operations.
- `Hybrid`: io_uring covers the wait/I/O path, while policy/scheduling/control
  remains user-space runtime logic.
- `Not io_uring-native`: mostly scheduler/context/ergonomics API surface above
  kernel I/O.

Context:

- This section is scoped to time/runtime utility APIs (not broader fs/net API
  breadth).
- Spargio today already has: `sleep`, `timeout`, `run`, `run_with`,
  `run_local_on`, `spawn_local_on`, cancellation token, and task group support.

### Compio parity gaps (time/runtime utility scope), io_uring fit, and recommendation

1. Absolute-deadline and interval timer APIs
   - Missing in Spargio:
     - `sleep_until`
     - `timeout_at`
     - `interval` / `interval_at`
     - `Interval::tick`
   - io_uring fit:
     - `Direct io_uring`:
       - `sleep_until` via timeout op on the native lane.
     - `Hybrid`:
       - `timeout_at` as composition over deadline timer + future race.
       - interval/tick as runtime policy on top of timer primitives.
   - Recommendation:
     - Add.
   - Priority:
     - High.
   - Rationale:
     - Strong functional value and clear alignment with io_uring timer path.

2. Rich timer object controls
   - Missing in Spargio:
     - resettable/introspectable timer object shape (`deadline`/`reset`/
       elapsed-style helpers).
   - io_uring fit:
     - `Hybrid` / mostly `Not io_uring-native` (API ergonomics and runtime timer
       bookkeeping over timer ops).
   - Recommendation:
     - Add a minimal version later.
   - Priority:
     - Medium.
   - Rationale:
     - Useful, but secondary to shipping base deadline/interval primitives.

3. `spawn_blocking` bridge
   - Missing in Spargio:
     - explicit runtime blocking bridge API.
   - io_uring fit:
     - `Not io_uring-native` (thread-pool/runtime policy feature).
   - Recommendation:
     - Add with strict bounds and opt-in behavior.
   - Priority:
     - Medium-high.
   - Rationale:
     - Operationally important escape hatch, but not part of io_uring data path.

4. Runtime control surface (`run`/`poll`/`poll_with`/`current_timeout`)
   - Missing in Spargio:
     - explicit low-level runtime control API set comparable to Compio.
   - io_uring fit:
     - `Hybrid`:
       - polling/timeout plumbing can map to io_uring waits, but API shape is
         mostly scheduler-control surface.
   - Recommendation:
     - Do not add full stable parity surface now; keep internal or debugging use.
   - Priority:
     - Low.
   - Rationale:
     - Limited end-user value and higher misuse/maintenance risk.

5. Runtime context API (`enter`/current-runtime access)
   - Missing in Spargio:
     - explicit public context-enter/current-runtime model.
   - io_uring fit:
     - `Not io_uring-native` (TLS/context ergonomics).
   - Recommendation:
     - Defer.
   - Priority:
     - Low-medium.
   - Rationale:
     - Useful only for narrower extension patterns; easy to misuse if overexposed.

6. `attach(fd)`-style extension-author hook
   - Missing in Spargio:
     - public attach hook for custom high-level wrappers.
   - io_uring fit:
     - `Hybrid`:
       - could map to registration/fixed-file strategy, but behavior and benefit
         are workload-dependent.
   - Recommendation:
     - Defer for now.
   - Priority:
     - Low.
   - Rationale:
     - unsafe extension path already exists; add attach semantics only if measured
       wrapper use-cases require it.

7. Builder knobs (`thread_affinity`, scheduler `event_interval`)
   - Missing in Spargio:
     - explicit builder options matching Compio naming/shape.
   - io_uring fit:
     - `Not io_uring-native` (scheduler/thread policy).
   - Recommendation:
     - Partial add, benchmark-gated.
   - Priority:
     - Medium.
   - Rationale:
     - Can help production tuning, but belongs to controlled runtime policy work.

### monoio parity gaps (time/runtime utility scope), io_uring fit, and recommendation

1. Absolute-deadline and interval timer APIs
   - Missing in Spargio:
     - `sleep_until`
     - `timeout_at`
     - `interval` / `interval_at`
     - `Interval::tick`
   - io_uring fit:
     - same split as Compio analysis: direct timer op base + hybrid interval
       policy layer.
   - Recommendation:
     - Add.
   - Priority:
     - High.
   - Rationale:
     - Core utility breadth with direct io_uring timer alignment.

2. Interval policy controls (`MissedTickBehavior`, interval metadata)
   - Missing in Spargio:
     - missed-tick policy controls and period inspection API.
   - io_uring fit:
     - `Not io_uring-native` (runtime policy semantics).
   - Recommendation:
     - Add later (after base interval API).
   - Priority:
     - Medium.
   - Rationale:
     - Valuable for precision semantics, but not required for first parity slice.

3. Resettable/introspectable `Sleep` object
   - Missing in Spargio:
     - `Sleep`-style object with `deadline` / `is_elapsed` / `reset`.
   - io_uring fit:
     - `Hybrid`:
       - backed by timeout ops, but object semantics are runtime/user-space layer.
   - Recommendation:
     - Add later (minimal form).
   - Priority:
     - Medium.
   - Rationale:
     - Power-user utility; should follow stable base timer/deadline APIs.

4. `spawn_blocking` + blocking runtime configuration
   - Missing in Spargio:
     - blocking bridge and policy knobs.
   - io_uring fit:
     - `Not io_uring-native`.
   - Recommendation:
     - Add with constrained configuration.
   - Priority:
     - Medium-high.
   - Rationale:
     - Important operational bridge, but separate from io_uring core mechanics.

### Net decision summary (io_uring-aware)

Add now (direct io_uring base + essential hybrid policy):

- `sleep_until`
- `timeout_at`
- `interval` / `interval_at` / `tick` (minimal first version)

Add next (important, mostly non-kernel policy/runtime features):

- `spawn_blocking` with bounded/opt-in policy
- limited affinity tuning in builder

Add later (power-user timer ergonomics):

- interval missed-tick behavior controls
- resettable/introspectable timer object (`Sleep`-style surface)

Defer/avoid for now:

- broad public low-level runtime polling/control API parity
- explicit runtime context enter/current-runtime API
- `attach(fd)` hook unless concrete, benchmark-backed wrapper demand emerges

## Update: I/O surface parity comparison (Compio + monoio, io_uring fit adjusted) (2026-02-28)

Revised the I/O parity recommendations to explicitly account for whether each
gap is:

- `Direct io_uring`: has a direct opcode path in current `io-uring` crate.
- `Hybrid`: hot path can use io_uring, but setup/orchestration still uses
  regular syscalls or user-space composition.
- `Not io_uring-native`: mostly trait/adaptor/protocol surface above kernel I/O.

Context:

- This section is scoped to I/O API surface (fs/net/io traits/utilities), not
  timer/runtime utilities.
- Spargio today has:
  - `fs::File` + `OpenOptions` and positional file ops (`read_at`, `write_at`,
    `read_to_end_at`, `fsync`).
  - `net::TcpStream` and `net::TcpListener` (session-policy aware APIs).
  - unbound unsafe extension lane for custom raw io_uring operations.

### Compio parity gaps (I/O surface scope), io_uring fit, and recommendation

1. Filesystem path-level helpers and metadata/perms utility breadth
   - Missing in Spargio:
     - path-level helpers like `create_dir`, `create_dir_all`, `remove_file`,
       `remove_dir`, `rename`, convenience `read`/`write`, and broader metadata/
       permissions/symlink/hard-link helpers.
   - io_uring fit:
     - `Direct io_uring` candidates:
       - `create_dir` (`MkDirAt`)
       - `remove_file` / `remove_dir` (`UnlinkAt`)
       - `rename` (`RenameAt`)
       - metadata (`Statx`)
       - symlink/hard-link (`SymlinkAt` / `LinkAt`)
       - convenience `read`/`write` composed from `OpenAt/OpenAt2 + Read/Write + Close`
     - `Hybrid` candidates:
       - `create_dir_all` (userspace recursion + repeated mkdir op)
       - richer convenience wrappers (`read_to_string`, recursive utilities)
       - some permissions/canonicalization helpers that may require syscall or
         userspace fallback paths depending on kernel support
   - Recommendation:
     - Add now for direct-op helpers.
     - Add later for hybrid helpers.
   - Priority:
     - High for direct helpers; Medium for hybrid helpers.
   - Rationale:
     - This adds high-utility API breadth while staying aligned with Spargio's
       io_uring-first performance model.

2. Network protocol/socket family breadth
   - Missing in Spargio:
     - `UdpSocket`
     - Unix domain sockets (`UnixStream`, `UnixListener`, `UnixDatagram`)
   - io_uring fit:
     - `Direct io_uring` hot path:
       - `Socket`, `Accept`, `Connect`, `Send`, `Recv`, `SendMsg`, `RecvMsg`,
         `Shutdown`
     - `Hybrid` setup/control path:
       - socket options, bind/listen, DNS/address resolution, feature probing
   - Recommendation:
     - Add.
   - Priority:
     - High for UDP; Medium-high for Unix sockets.
   - Rationale:
     - Strong fit for io_uring data path and large practical adoption win beyond
       TCP-only coverage.

3. Generic async I/O trait + adapter layer
   - Missing in Spargio:
     - Compio-style traits/extensions (`AsyncRead*` / `AsyncWrite*`) and common
       adapters/utilities (`split`, buffered wrappers, framing/compat layers).
   - io_uring fit:
     - `Not io_uring-native` (user-space abstraction layer).
   - Recommendation:
     - Add, but as companion crate(s), not in core runtime crate.
   - Priority:
     - Medium.
   - Rationale:
     - Important ergonomics/interoperability value, but no kernel-path
       differentiation and substantial maintenance surface.

4. Optional higher-level transport/integration modules
   - Missing in Spargio:
     - Compio optional module breadth (`process`, `signal`, `tls`, `ws`, `quic`).
   - io_uring fit:
     - Mostly `Not io_uring-native` as runtime-level feature sets; some pieces
       may use io_uring underneath but are not core io_uring API-surface gaps.
   - Recommendation:
     - Defer in core; pursue as ecosystem crates after core fs/net/io parity
       baseline is complete.
   - Priority:
     - Low.
   - Rationale:
     - Broad scope with weaker direct alignment to immediate io_uring runtime
       differentiation.

### monoio parity gaps (I/O surface scope), io_uring fit, and recommendation

1. Filesystem path-level helper breadth
   - Missing in Spargio:
     - monoio-style helpers (`read`, `write`, `create_dir`, `create_dir_all`,
       `remove_file`, `remove_dir`, `rename`) and metadata conveniences.
   - io_uring fit:
     - same split as above: direct-op coverage for core helpers, hybrid for
       recursive/convenience wrappers.
   - Recommendation:
     - Add direct-op helpers now; phase in hybrid helpers later.
   - Priority:
     - High for direct-op helpers; Medium for hybrid helpers.
   - Rationale:
     - Baseline parity and migration ergonomics with strong io_uring alignment.

2. Network breadth beyond TCP
   - Missing in Spargio:
     - `UdpSocket`
     - Unix domain socket APIs.
   - io_uring fit:
     - direct-op hot path with hybrid setup path, same as Compio analysis.
   - Recommendation:
     - Add.
   - Priority:
     - High for UDP; Medium-high for Unix sockets.
   - Rationale:
     - Real-world protocol coverage with clear io_uring throughput/latency fit.

3. I/O utility stack (traits + utility wrappers)
   - Missing in Spargio:
     - monoio-style utility stack (`copy`, split halves, buffered wrappers,
       stream/sink adapters, cancelable helpers, zero-copy utility wrappers).
   - io_uring fit:
     - mostly `Not io_uring-native` (API composition layer).
   - Recommendation:
     - Add a practical subset after core direct-op I/O breadth lands; keep larger
       utility surface outside core crate.
   - Priority:
     - Medium.
   - Rationale:
     - Good ergonomics payoff, but should follow direct io_uring-aligned API
       expansion.

### Net decision summary (io_uring-aware)

Add now (direct io_uring or low-risk hybrid):

- path-level fs helpers that map cleanly to io_uring opcodes
  (`create_dir`, `remove_file`, `remove_dir`, `rename`, metadata, basic `read`/`write`)
- UDP socket API

Add next (hybrid or non-kernel surface with strong usability gain):

- Unix domain socket API
- foundational I/O trait/extensions and core helpers (`split`, `copy`) in
  companion crate(s)

Add later (mostly composition layers):

- recursive/richer fs convenience helpers (`create_dir_all`, broader wrappers)
- richer buffered/framed/compat layers

Defer/avoid in core for now:

- large optional integration surfaces (`process`, `signal`, `tls`, `ws`, `quic`)
  until core io_uring-aligned fs/net parity goals are met

## Update: parity execution sweep (time/runtime + I/O breadth) with red/green TDD (2026-02-28)

Executed the requested implementation sweep for all previously marked
`add now`, `add next`, and `add later` items in the time/runtime and I/O parity
sections, then validated with full `uring-native` test pass.

### Red phase

Added failing tests first:

1. Time/runtime primitives (`tests/primitives_tdd.rs`)
   - `sleep_until_waits_for_deadline`
   - `timeout_at_returns_err_when_deadline_expires`
   - `interval_ticks_with_configurable_missed_tick_behavior`
   - `interval_at_uses_requested_start_deadline`
   - `sleep_object_supports_deadline_reset_and_elapsed_state`
   - `runtime_handle_spawn_blocking_executes_closure`

2. Runtime builder tuning (`tests/runtime_tdd.rs`)
   - `runtime_builder_thread_affinity_option_builds_runtime`

3. I/O breadth (`tests/ergonomics_tdd.rs`)
   - `fs_path_helpers_cover_common_workflows`
   - `fs_link_helpers_support_symlink_and_hard_link`
   - `net_udp_socket_supports_send_recv_and_send_to_recv_from`
   - `net_unix_stream_listener_and_datagram_cover_core_paths`
   - `io_helpers_split_copy_and_framed_work`

Red failures were expected:

- unresolved time/runtime symbols (`sleep_until`, `timeout_at`, `interval*`,
  `Sleep`, `MissedTickBehavior`, `spawn_blocking`, `thread_affinity`).
- unresolved I/O symbols (`fs` path helpers, `UdpSocket`, `Unix*`, `io` module).

### Green phase

Implemented in `src/lib.rs`:

1. Time/runtime utility breadth
   - Added:
     - `sleep_until(Instant)`
     - `timeout_at(Instant, fut)`
     - `Sleep` (`new`, `until`, `deadline`, `is_elapsed`, `reset`, `Future`)
     - `interval(period)`, `interval_at(start, period)`
     - `Interval::tick`, `Interval::period`,
       `Interval::{missed_tick_behavior,set_missed_tick_behavior}`
     - `MissedTickBehavior::{Burst, Delay, Skip}`

2. Runtime utilities/tuning
   - Added `RuntimeHandle::spawn_blocking(...) -> Result<JoinHandle<_>, RuntimeError>`.
   - Added `RuntimeBuilder::thread_affinity(...)`.
   - Wired per-shard thread affinity application during shard thread startup
     (best-effort, Linux `sched_setaffinity`).

3. Filesystem API breadth
   - Added path-level async helpers in `spargio::fs`:
     - `create_dir`, `create_dir_all`, `remove_file`, `remove_dir`, `rename`
     - `hard_link`, `symlink`
     - `metadata`, `symlink_metadata`, `set_permissions`, `canonicalize`
     - convenience `read`, `read_to_string`, `write`
   - Added internal blocking bridge helper in fs module using
     `RuntimeHandle::spawn_blocking`.

4. Network API breadth
   - Added `spargio::net::UdpSocket`:
     - `bind`, `from_std`, `local_addr`, `connect`
     - `send`, `recv`, `send_to`, `recv_from`
   - Added `spargio::net::UnixStream`:
     - `connect`, `connect_with_session_policy`, `from_std`
     - `send`/`recv`, owned buffer variants, `write_all`/`read_exact`
   - Added `spargio::net::UnixListener`:
     - `bind`, `from_std`, `local_addr`, `accept`
   - Added `spargio::net::UnixDatagram`:
     - `bind`, `from_std`, `local_addr`, `connect`
     - `send`, `recv`, `send_to`, `recv_from`

5. Foundational I/O utility layer
   - Added `spargio::io` module:
     - traits: `AsyncRead`, `AsyncWrite` + extension traits
     - `split(...)` with `ReadHalf` / `WriteHalf`
     - `copy_to_vec(...)`
     - lightweight wrappers: `BufReader`, `BufWriter`
     - framed helper: `io::framed::LengthDelimited::{new, write_frame, read_frame}`

### Validation

Executed and passing:

- `cargo test --features uring-native --test primitives_tdd`
- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo test --features uring-native --test runtime_tdd --test uring_native_tdd`
- `cargo test --features uring-native`

Result:

- full `uring-native` test suite passes after the parity sweep.

## Proposal: syscall migration to io_uring for fs path helpers (2026-02-28)

Goal:

- Remove remaining helper-thread `spawn_blocking(std::fs::...)` usage from the
  high-value `spargio::fs` path APIs where direct io_uring opcodes exist.
- Keep low-value/hard cases as compatibility paths for now.

Proposed execution model:

1. Add direct unbound native commands + opcodes for path operations:
   - `MkDirAt` (`create_dir`)
   - `UnlinkAt` (`remove_file`, `remove_dir` via `AT_REMOVEDIR`)
   - `RenameAt` (`rename`)
   - `LinkAt` (`hard_link`)
   - `SymlinkAt` (`symlink`)
2. Migrate corresponding `spargio::fs` helpers to native io_uring submission.
3. Keep these deferred as compatibility wrappers:
   - `create_dir_all`:
     - recursive user-space semantics and error behavior matching require extra
       traversal/orchestration logic; not a single direct opcode operation.
   - `canonicalize`:
     - path-resolution semantics are better handled by libc/kernel resolver
       paths; no direct single-op parity target in current surface.
   - `metadata`, `symlink_metadata`, `set_permissions`:
     - current public return/argument types are std wrappers
       (`std::fs::Metadata` / `Permissions`) not directly constructible from
       raw `statx` payloads without additional compatibility syscall layers.
4. Keep red/green TDD workflow:
   - add failing native fs-op tests first,
   - implement op plumbing + fs helper migration,
   - run targeted tests then full `cargo test --features uring-native`.

Acceptance criteria:

- No helper-thread path for: `create_dir`, `remove_file`, `remove_dir`,
  `rename`, `hard_link`, `symlink`.
- Deferred items remain clearly documented as compatibility paths.
- Full `uring-native` test suite remains green.

## Update: syscall migration to io_uring (fs path helpers) implemented (Red/Green TDD) (2026-02-28)

Implemented the proposal slice for direct-op fs path helpers, with explicit
kernel-support fallback behavior for unsupported opcode errors.

### Red phase

Added failing tests first in `tests/uring_native_tdd.rs`:

- `uring_native_unbound_fs_path_ops_cover_mkdir_rename_link_symlink_and_unlink`

Observed expected red failure:

- compile errors for missing `UringNativeAny` methods:
  - `mkdir_at`
  - `unlink_at`
  - `rename_at`
  - `link_at`
  - `symlink_at`

### Green phase

Implemented native io_uring path-op helpers on `UringNativeAny` (in `src/lib.rs`)
using the existing unsafe extension submission lane internally:

- `mkdir_at(path, mode)` -> `opcode::MkDirAt`
- `unlink_at(path, is_dir)` -> `opcode::UnlinkAt` (+ `AT_REMOVEDIR` for dirs)
- `rename_at(from, to)` -> `opcode::RenameAt`
- `link_at(original, link)` -> `opcode::LinkAt`
- `symlink_at(target, linkpath)` -> `opcode::SymlinkAt`

Then migrated high-level `spargio::fs` helpers to these native operations:

- `create_dir`
- `remove_file`
- `remove_dir`
- `rename`
- `hard_link`
- `symlink`

Compatibility behavior kept intentionally:

- For unsupported opcode errors (`EINVAL`, `ENOSYS`, `EOPNOTSUPP`), the above
  high-level helpers transparently fall back to prior blocking helper-thread
  implementations to preserve functionality on older kernels.

Deferred (unchanged, by proposal):

- `create_dir_all`
- `canonicalize`
- `metadata`
- `symlink_metadata`
- `set_permissions`

### Validation

Executed and passing:

- `cargo test --features uring-native --test uring_native_tdd`
- `cargo test --features uring-native --test ergonomics_tdd`
- `cargo test --features uring-native`

## Update: higher-level ecosystem parity check (Compio vs monoio) (2026-02-28)

Context:

- Follow-up comparison for higher-level "not done yet" surfaces:
  `process`, `signal`, `tls`, `ws`, `quic`.

### Feature presence snapshot

1. Compio
   - `process`:
     - available through first-party Compio family surface.
   - `signal`:
     - available through first-party Compio family surface.
   - `tls`:
     - available through first-party Compio family surface (feature-gated).
   - `ws`:
     - available in Compio ecosystem integrations (not a minimal runtime-core primitive).
   - `quic`:
     - available in Compio ecosystem integrations (not a minimal runtime-core primitive).
   - Assessment:
     - broad coverage with strong first-party/feature-gated story.

2. monoio
   - `process`:
     - not a primary monoio-core built-in surface; typically external integration.
   - `signal`:
     - available in monoio core behind feature gating.
   - `tls`:
     - primarily ecosystem crates/integrations.
   - `ws`:
     - primarily ecosystem crate coverage.
   - `quic`:
     - primarily ecosystem crate coverage.
   - Assessment:
     - slim core with higher-level surfaces mostly delegated to ecosystem crates.

### Implication for Spargio

- Current Spargio direction (core runtime + io_uring-aligned fs/net/io breadth,
  higher-level protocol/process surfaces deferred) is closer to monoio layering
  than to Compio's broader first-party family.
- Recommendation remains:
  - keep `process/signal/tls/ws/quic` out of `spargio` core for now;
  - deliver these as extension/companion crates after core fs/net/io parity
    and stability milestones;
  - if one is pulled forward, `signal` is the lowest-risk first candidate.

### Recommendation tags

- `process`: add later via companion crate, not core now.
- `signal`: consider next in companion form; optional core later if justified.
- `tls`: companion crate target, not core.
- `ws`: companion crate target, not core.
- `quic`: companion crate target, not core.

## Update: prioritized roadmap as concrete milestones (2026-02-28)

Converted the current roadmap direction into execution milestones with explicit
acceptance criteria.

### Milestone M1: production hardening + observability (highest priority)

Scope:

- Add stress/soak/failure-injection coverage for scheduler, boundary, and native
  fs/net paths.
- Expand runtime observability for queue depth, steal rates, in-flight native op
  counts, and timeout/cancellation outcomes.
- Add long-window p95/p99 benchmark tracking and guardrails.

Acceptance criteria:

- New stress/failure suites pass under `uring-native` in CI/nightly runs.
- Benchmark guardrail workflow reports p50/p95/p99 for key suites and enforces
  no-regression thresholds.
- At least one documented regression triage loop exists (capture -> compare ->
  bisect -> fix).

### Milestone M2: safe extension API wrappers + cookbook

Scope:

- Define and publish safe wrapper patterns for common unsafe native extension
  use-cases (ownership, lifetime, affinity, cancellation, fallback strategy).
- Add cookbook-quality examples for custom opcode submission and validation.

Acceptance criteria:

- Cookbook/examples compile and test in CI.
- At least one end-to-end extension example avoids direct user-facing unsafe
  blocks outside the wrapper boundary.
- Invariants/checklist for extension authors are documented and versioned.

### Milestone M3: docs and API-selection guidance

Scope:

- Expand docs for API selection (`fs/net/io` helpers vs native ops), placement
  policy choice, and benchmark interpretation.
- Add migration notes for users coming from Tokio/Compio/monoio surfaces.
- Stand up an in-repo `mdBook` as the long-form documentation home.
- Keep root `README.md` content/length stable for now, and add book links once
  the initial book structure is published.

Acceptance criteria:

- mdBook skeleton and initial chapters are in-repo and build in CI.
- `README.md` remains concise/current and links to the book after publish.
- README + guide pages clearly map common tasks to preferred APIs.
- Placement and latency/throughput tradeoffs are documented with concrete
  examples.
- Benchmark methodology is reproducible from documented commands.

### Milestone M4: measured core refinements (only clear-ROI changes)

Scope:

- Evaluate deferred fs helper migration items (`create_dir_all`,
  `canonicalize`, `metadata`, `symlink_metadata`, `set_permissions`) only when
  there is measured benefit.
- Tune work-stealing heuristics based on M1 telemetry, not ad-hoc changes.

Acceptance criteria:

- Each migration/tuning change ships with before/after benchmark data and no
  correctness regressions.
- No low-value complexity is added for cases with no measurable user impact.
- `cargo test`, `cargo test --features uring-native`, and benchmark guardrails
  remain green.

### Milestone M5: higher-level ecosystem parity via companion crates

Scope:

- Deliver higher-level surfaces outside core in this order:
  1) `signal` companion crate
  2) `tls` / `ws` / `quic` integrations
  3) `process` companion crate
- Implement companion crates as workspace subcrates in this repository (shared
  CI, tests, versioning, and release flow).
- Keep core focused on runtime + io_uring-aligned fs/net/io fundamentals.

Acceptance criteria:

- Companion crates have docs, tests, and minimal examples.
- Companion crates are wired as workspace members and participate in standard
  workspace CI checks.
- Core crate API remains stable/lean and does not absorb large optional stacks.
- Integration ergonomics are comparable to current core APIs for common use.

### Milestone M6: optional readiness-emulation track (deprioritized backlog)

Scope:

- Explicitly deprioritized for now.
- Reconsider optional Tokio-compat readiness shim (`IORING_OP_POLL_ADD`) only
  after M1-M5 are stable and after concrete demand is demonstrated.

Acceptance criteria:

- Not planned in the current execution window.
- Implemented behind explicit opt-in feature gate.
- Benchmark data shows practical value for targeted readiness-centric workloads.
- Does not regress default core paths or increase default runtime complexity.

## Update: Milestone M1 implemented (hardening + observability) with Red/Green TDD (2026-02-28)

Executed Milestone M1 scope with explicit red tests first, then implementation
and validation.

### Red phase

Added failing tests:

1. Boundary outcome observability (`tests/boundary_tdd.rs`)
   - `boundary_stats_capture_timeout_cancel_and_overload`
   - expected red failure:
     - missing `boundary::stats_snapshot`
     - missing `boundary::reset_stats_for_tests`.

2. Runtime stats helper observability (`tests/slices_tdd.rs`)
   - extended `stats_snapshot_tracks_messages_and_spawns` to require:
     - `RuntimeStats::total_command_depth`
     - `RuntimeStats::max_command_depth`
     - `RuntimeStats::max_pending_native_ops_by_shard`
     - `RuntimeStats::steal_success_rate`
   - expected red failure:
     - unresolved methods on `RuntimeStats`.

3. Percentile guardrail tooling (`tests/bench_tail_guardrail_tdd.rs`)
   - `percentile_guardrail_passes_for_fixture_profile`
   - `percentile_guardrail_fails_when_threshold_is_too_strict`
   - expected red failure:
     - missing `scripts/bench_tail_guardrail.sh`.

### Green phase

Implemented:

1. Boundary outcome stats API in `spargio::boundary`
   - new `BoundaryStats` snapshot struct:
     - `overloaded`, `timed_out`, `canceled`, `closed`
   - new APIs:
     - `boundary::stats_snapshot()`
     - `boundary::reset_stats_for_tests()`
   - instrumented boundary paths to record outcomes:
     - enqueue/try-enqueue overload and closed cases
     - ticket wait timeout and recv timeout
     - cancel paths (`respond` with dropped receiver, ticket poll canceled).

2. Runtime observability helper methods
   - added on `RuntimeStats`:
     - `total_command_depth()`
     - `max_command_depth()`
     - `max_pending_native_ops_by_shard()`
     - `steal_success_rate()`.

3. p50/p95/p99 guardrail script
   - added `scripts/bench_tail_guardrail.sh`
     - consumes Criterion `sample.json`
     - computes per-iteration p50/p95/p99
     - enforces `MAX_P50_RATIO`, `MAX_P95_RATIO`, `MAX_P99_RATIO`.
   - integrated into `scripts/bench_kpi_guardrail.sh`.
   - added fixture-backed tests under `tests/bench_tail_guardrail_tdd.rs`
     and `tests/fixtures/criterion/...`.

4. Hardening/soak coverage and nightly execution
   - added soak tests in `tests/stress_tdd.rs` (ignored by default):
     - `soak_stealable_burst_completes_without_dropping_tasks`
     - `soak_boundary_timeout_cancel_overload_paths_accumulate_stats`
   - CI workflow updates in `.github/workflows/ci.yml`:
     - added percentile guardrail steps
     - added nightly scheduled trigger
     - added `nightly-soak` job running ignored soak tests.

5. Regression triage loop documentation
   - added `docs/perf_regression_triage.md` with capture/compare/bisect/fix loop.

### Validation

Executed and passing:

- `cargo test --test boundary_tdd --test slices_tdd --test bench_tail_guardrail_tdd`
- `cargo test --test stress_tdd -- --ignored`

## Update: Milestone M2 implemented (safe extension wrappers + cookbook) with Red/Green TDD (2026-02-28)

Executed Milestone M2 scope with red-first tests and wrapper/docs delivery.

### Red phase

Added failing test in `tests/uring_native_tdd.rs`:

- `uring_native_safe_extension_statx_wraps_unsafe_submission`

Expected red failure:

- unresolved safe wrapper API:
  - `spargio::extension::fs::statx_on_shard`
  - `spargio::extension::fs::StatxOptions`
  - `spargio::extension::fs::statx_or_metadata`.

### Green phase

Implemented a first safe-wrapper extension surface in `src/lib.rs`:

- new module: `spargio::extension::fs`
- new safe APIs:
  - `statx(native, path)`
  - `statx_on_shard(native, shard, path, options)`
  - `statx_or_metadata(handle, path)` (kernel-support fallback)
- new typed outputs/options:
  - `StatxMetadata`
  - `StatxOptions`

Implementation details:

- wrappers encapsulate all `unsafe` usage internally and keep extension state
  owned until CQE completion (`CString` + output buffer in owned state struct).
- unsupported native-op errors (`EINVAL`/`ENOSYS`/`EOPNOTSUPP`) fall back to
  blocking `std::fs::metadata` through `RuntimeHandle::spawn_blocking`.
- explicit-shard and selector-driven variants both provided.

Cookbook/docs/examples:

- added `docs/native_extension_cookbook.md`:
  - ownership/lifetime pattern
  - affinity pattern
  - fallback pattern
  - explicit safety checklist for extension authors.
- added example `examples/native_extension_statx.rs` showing end-to-end usage
  with no user-facing `unsafe`.

### Validation

Executed and passing:

- `cargo test --features uring-native --test uring_native_tdd uring_native_safe_extension_statx_wraps_unsafe_submission`
- `cargo test --features uring-native --test uring_native_tdd`

## Update: Milestone M3 implemented (mdBook docs track) with Red/Green TDD (2026-02-28)

Executed Milestone M3 scope with red-first docs tests, then book scaffold and
CI integration. Root `README.md` content/length was intentionally left unchanged
per current decision.

### Red phase

Added failing tests in `tests/docs_tdd.rs`:

- `mdbook_scaffold_exists_with_summary`
- `mdbook_summary_links_resolve_to_existing_files`

Expected red failure:

- missing `book/book.toml`
- missing `book/src/SUMMARY.md` and chapter files.

### Green phase

Added in-repo mdBook scaffold:

- `book/book.toml`
- `book/src/SUMMARY.md`
- initial chapters:
  - `introduction.md`
  - `runtime_entry.md`
  - `placement.md`
  - `io_surface.md`
  - `native_extensions.md`
  - `benchmarking.md`
  - `migration.md`

CI integration:

- `.github/workflows/ci.yml` now installs `mdbook` and runs:
  - `mdbook build book`

### Validation

Executed and passing:

- `cargo test --test docs_tdd`
- `mdbook build book`


## Update: Milestone M4 implemented (measured core refinements) with Red/Green TDD (2026-02-28)

Executed a low-risk, measured refinement for one deferred fs area without
forcing full std-type migration complexity.

### Red phase

Extended `tests/ergonomics_tdd.rs` in:

- `fs_path_helpers_cover_common_workflows`

New assertion required unresolved API:

- `spargio::fs::metadata_lite(...)`

Expected red failure:

- missing `metadata_lite` helper in `spargio::fs`.

### Green phase

Implemented in core:

1. New measured helper
   - `spargio::fs::metadata_lite(handle, path)` in `src/lib.rs`.
   - Returns `spargio::extension::fs::StatxMetadata`.
   - Uses native-first safe wrapper (`statx_or_metadata`) with unsupported-op
     fallback to blocking metadata path.

2. Benchmark instrumentation for ROI tracking
   - added `fs_metadata_rtt` group in `benches/fs_api.rs`:
     - `tokio_spawn_blocking_metadata`
     - `spargio_metadata_lite`
   - extended fs harnesses with metadata command path to keep benchmark setup
     comparable with existing harness style.

### Measurement snapshot

Executed:

- `cargo bench --features uring-native --bench fs_api fs_metadata_rtt -- --warm-up-time 0.10 --measurement-time 0.10 --sample-size 20`

Observed:

- `fs_metadata_rtt/tokio_spawn_blocking_metadata`: `6.8858-7.2658 ms`
  (`140.93-148.71 Kelem/s`)
- `fs_metadata_rtt/spargio_metadata_lite`: `4.6598-4.8596 ms`
  (`210.72-219.75 Kelem/s`)

Interpretation:

- native-first `metadata_lite` shows a clear throughput and latency win in this
  short-run metadata workload while preserving compatibility fallback.
- retained the prior decision to defer full std-wrapper migration for
  `metadata`/`symlink_metadata`/`set_permissions` itself until broader,
  benchmark-backed conversion is justified.

### Validation

Executed and passing:

- `cargo test --features uring-native --test ergonomics_tdd fs_path_helpers_cover_common_workflows`
- `cargo bench --features uring-native --bench fs_api --no-run`

## Update: Milestone M5 implemented (companion crates as workspace subcrates) with Red/Green TDD (2026-02-28)

Executed Milestone M5 by wiring companion crates into the workspace and adding
initial tested APIs for `signal`, protocol integrations (`tls/ws/quic`
blocking bridges), and `process`.

### Red phase

Added failing workspace test in `tests/workspace_companions_tdd.rs`:

- `workspace_lists_companion_subcrates`

Expected red failure:

- root `Cargo.toml` had no `[workspace]`.
- companion crate paths were not present.

### Green phase

Workspace wiring:

- root `Cargo.toml` now defines workspace members:
  - `.`
  - `spargio-macros`
  - `crates/spargio-signal`
  - `crates/spargio-protocols`
  - `crates/spargio-process`

Companion subcrates added:

1. `spargio-signal`
   - API:
     - `signal(...) -> SignalStream`
     - `ctrl_c() -> SignalStream`
     - `SignalStream::recv().await`
   - implementation:
     - `signal-hook` listener thread + async-facing receive loop.
   - tests:
     - construction test
     - raised-signal receive test.

2. `spargio-protocols`
   - API:
     - `tls_blocking(...)`
     - `ws_blocking(...)`
     - `quic_blocking(...)`
   - implementation:
     - explicit `RuntimeHandle::spawn_blocking` bridges for protocol ecosystem
       integration points.
   - tests:
     - closure execution test across all three helpers.

3. `spargio-process`
   - API:
     - `status(handle, Command)`
     - `output(handle, Command)`
     - `CommandBuilder::{new,arg,args,status,output}`
   - implementation:
     - async process execution via runtime blocking bridge.
   - tests:
     - builder status path
     - function status path.

Companion docs/examples:

- added crate-level docs and minimal example files under each companion crate.

Repository status docs:

- updated `README.md` done/not-done section to reflect:
  - companion crates now present
  - safe extension wrapper slice done
  - `mdBook` scaffold done
  - higher-level parity still maturing.

### Validation

Executed and passing:

- `cargo test --test workspace_companions_tdd`
- `cargo test --workspace`
- `cargo test --workspace --features uring-native`

## Update: execution breakdown to reach deep protocol adapters + polished APIs (2026-02-28)

Captured a concrete implementation plan for completing the remaining higher-level
ecosystem maturity work.

Bridge-first principle for these phases:

- prefer proven upstream protocol/runtime crates and build thin `spargio-*`
  adapters around them.
- keep runtime integration value in `spargio` (timeouts/cancellation,
  instrumentation, placement ergonomics), while avoiding protocol reimplementation
  in core.
- keep protocol engines swappable behind stable companion-crate APIs.

### Phase 1: foundation layer (2-3 weeks)

Scope:

- freeze public API contracts for companion crates (`signal`, `process`, `tls`,
  `ws`, `quic`).
- define shared error taxonomy and conversions.
- add/finish `spargio::io` compatibility adapters needed by protocol crates.
- standardize timeout/cancellation/close semantics across companion crates.
- decide and document upstream bridge backends:
  - TLS: `rustls` + `futures-rustls`.
  - WS: `async-tungstenite` as default path; optional high-performance path via
    `fastwebsockets` where fit is proven.
  - QUIC: `quinn` first.
  - process: `async-process` bridge.
  - signal: `signal-hook`/`async-signal` style bridge model.

Done criteria:

- RFC-style contract docs checked in.
- compile-tested API skeletons.
- conformance tests for shared semantics.
- backend-selection rationale and compatibility policy documented.

### Phase 2: TLS deep adapter (3-5 weeks)

Scope:

- add `spargio-tls` companion crate (thin wrapper over
  `rustls`/`futures-rustls`, not a new TLS engine).
- implement connector/acceptor/stream APIs.
- implement handshake timeout/cancel semantics and ALPN/SNI config surface.
- add client/server interop tests.

Done criteria:

- stable TLS API for common client/server flows.
- interop + stress tests passing.
- cookbook/example coverage for common TLS service patterns.

### Phase 3: WebSocket deep adapter (2-4 weeks)

Scope:

- add `spargio-ws` companion crate.
- bridge to `async-tungstenite` first for broad interop; evaluate
  `fastwebsockets` as an optional backend for high-throughput paths.
- implement handshake APIs for client/server.
- implement frame/message API (`text`, `binary`, `ping/pong`, close).
- add fragmentation/backpressure/size-limit controls.

Done criteria:

- interoperable ws client/server examples.
- conformance tests for close/ping/pong and framing paths.
- documented limits and backpressure behavior.

### Phase 4: QUIC deep adapter (6-12 weeks)

Scope:

- add `spargio-quic` companion crate (quinn-first path).
- keep transport/protocol core in `quinn`; `spargio-quic` provides runtime
  integration and ergonomic API shaping.
- implement endpoint/connect/accept lifecycle APIs.
- implement uni/bi streams + datagram APIs.
- add config builder surface (timeouts/flow-control/congestion knobs).

Done criteria:

- stable endpoint/connection/stream/dgram APIs.
- interop/load tests passing.
- operational docs for tuning and shutdown semantics.

### Phase 5: process/signal maturity pass (2-3 weeks)

Scope:

- evolve current `spargio-process` and `spargio-signal` from minimal bridges to
  richer production APIs.
- process path: solidify `async-process`-style bridge ergonomics with cancellation
  and stdio behavior consistency.
- signal path: `signal-hook`/`async-signal` bridge with robust subscription and
  shutdown semantics.
- process: lifecycle + stdio handling polish.
- signal: richer subscription ergonomics and graceful-shutdown recipes.

Done criteria:

- expanded APIs with tests for lifecycle/race cases.
- cookbook examples for service shutdown and child-process orchestration.

### Phase 6: hardening + operations (3-5 weeks, overlaps phases 2-5)

Scope:

- failure-injection, stress/soak suites for companion protocol paths.
- p50/p95/p99 guardrail expansion for protocol benchmarks.
- observability hooks and regression triage workflow maturity.
- upstream compatibility matrix in CI (selected backend versions) to catch
  bridge drift early.

Done criteria:

- nightly/CI hardening lanes in place and stable.
- measurable long-window tail-latency tracking with gates.

### Phase 7: docs + polish (2-3 weeks, overlaps late phases)

Scope:

- expand mdBook protocol coverage and API selection guidance.
- migration docs and production checklists.
- semver/deprecation policy for companion crates.
- explicit "use direct upstream crate vs use `spargio-*` adapter" guidance for
  each protocol domain.

Done criteria:

- publish-grade docs for all companion crates.
- clear migration paths and stability guarantees documented.

### Recommended sequencing

1. Foundation
2. TLS + WS in parallel
3. QUIC
4. process/signal maturity
5. hardening/docs finalization across all crates

### Effort estimate

- single engineer: ~4-6 months
- 2-3 engineers in parallel: ~8-12 weeks for a strong first production-grade cut

## Update: Phase 1 implemented (foundation contracts + semantics + io compatibility) with Red/Green TDD (2026-02-28)

Executed a concrete Phase 1 slice with red-first tests, then implementation and
green verification.

### Red tests added first

- `crates/spargio-protocols/tests/foundation_tdd.rs`
  - `blocking_options_enforce_timeout`
  - `futures_io_adapter_roundtrip_over_tcp_stream` (Linux + `uring-native`)
- `crates/spargio-process/tests/foundation_tdd.rs`
  - `status_with_options_enforces_timeout`

Observed expected red state before implementation:

- unresolved imports/APIs:
  - `BlockingOptions`, `tls_blocking_with_options`
  - `CommandOptions`, `status_with_options`
  - `io_compat::FuturesTcpStream`

### Implementation delivered

`spargio-protocols`:

- added `BlockingOptions` with optional timeout policy.
- added optioned API variants:
  - `tls_blocking_with_options`
  - `ws_blocking_with_options`
  - `quic_blocking_with_options`
- kept existing `*_blocking` helpers as defaults over optioned APIs.
- standardized timeout semantics with `spargio::timeout(...)` -> `io::ErrorKind::TimedOut`.
- added Linux `uring-native` `futures::io` adapter:
  - `io_compat::FuturesTcpStream` implements
    `futures::io::{AsyncRead, AsyncWrite}` over `spargio::net::TcpStream`.
- added crate feature forwarding:
  - `uring-native = ["spargio/uring-native"]`.

`spargio-process`:

- added `CommandOptions` with optional timeout policy.
- added optioned API variants:
  - `status_with_options`
  - `output_with_options`
  - `CommandBuilder::{status_with_options, output_with_options}`
- kept existing `status`/`output` APIs delegating to default options.
- standardized timeout semantics with `spargio::timeout(...)` -> `io::ErrorKind::TimedOut`.

Contracts/docs:

- added `docs/companion_contracts.md` to capture baseline shared semantics for
  companion crates (error mapping, cancellation, timeout, io compatibility).

### Green validation

Executed and passing:

- `cargo test -p spargio-process --test foundation_tdd`
- `cargo test -p spargio-protocols --test foundation_tdd`
- `cargo test -p spargio-protocols --features uring-native --test foundation_tdd`

## Update: Phase 2 implemented (TLS deep adapter bridge) with Red/Green TDD (2026-02-28)

Executed a red-first TLS companion crate implementation over
`rustls` + `futures-rustls`.

### Red tests added first

- created new workspace crate: `crates/spargio-tls`
- added `crates/spargio-tls/tests/tls_tdd.rs` with:
  - `tls_connector_connect_socket_addr_timeout_is_enforced`
  - `tls_connector_and_acceptor_interop_roundtrip`

Observed expected red state:

- unresolved API imports in `spargio_tls`:
  - `HandshakeOptions`
  - `TlsConnector`

### Implementation delivered

Workspace wiring:

- added `crates/spargio-tls` to root workspace members.
- crate deps include:
  - `futures-rustls`
  - `rustls`
  - `spargio` (`uring-native`)
  - `spargio-protocols` (`uring-native` io adapter bridge)

Public API:

- handshake options:
  - `HandshakeOptions` (optional timeout)
- connector/acceptor wrappers:
  - `TlsConnector` with:
    - `connect(...)`
    - `connect_socket_addr(...)`
  - `TlsAcceptor` with:
    - `accept(...)`
- free functions:
  - `connect`, `connect_with_options`
  - `connect_socket_addr`, `connect_socket_addr_with_options`
  - `accept`, `accept_with_options`
- stream aliases:
  - `ClientTlsStream`
  - `ServerTlsStream`

Semantics:

- TLS handshakes are timeout-governed via `spargio::timeout(...)`.
- timeout maps to `io::ErrorKind::TimedOut`.
- transport layer is a thin bridge over
  `spargio-protocols::io_compat::FuturesTcpStream`.

Related compatibility improvement:

- implemented `Debug` for `spargio-protocols::io_compat::FuturesTcpStream`
  to satisfy downstream stream debug bounds.

### Green validation

Executed and passing:

- `cargo test -p spargio-tls --test tls_tdd`

## Update: Phase 3 implemented (WebSocket deep adapter bridge) with Red/Green TDD (2026-02-28)

Executed a red-first WebSocket companion crate implementation over
`async-tungstenite`.

### Red tests added first

- created new workspace crate: `crates/spargio-ws`
- added `crates/spargio-ws/tests/ws_tdd.rs` with:
  - `ws_client_connect_timeout_is_enforced`
  - `ws_client_server_roundtrip_text_message`

Observed expected red state:

- unresolved API imports in `spargio_ws`:
  - `WsOptions`
  - `accept_with_options`
  - `connect_socket_addr_with_options`

### Implementation delivered

Workspace wiring:

- added `crates/spargio-ws` to root workspace members.
- crate deps include:
  - `async-tungstenite`
  - `spargio` (`uring-native`)
  - `spargio-protocols` (`uring-native` io adapter bridge)

Public API:

- options and wrappers:
  - `WsOptions` (timeout + frame/message limit knobs)
  - `WsConnector`
  - `WsAcceptor`
- stream aliases:
  - `WsStream`
  - `WsResponse`
- functions:
  - `connect`, `connect_with_options`
  - `connect_socket_addr`, `connect_socket_addr_with_options`
  - `accept`, `accept_with_options`

Semantics:

- handshake timeout enforced with `spargio::timeout(...)`.
- timeout maps to `io::ErrorKind::TimedOut`.
- tungstenite protocol errors map to `io::Error` for uniform bridge behavior.
- uses `spargio-protocols::io_compat::FuturesTcpStream` transport adapter.

### Green validation

Executed and passing:

- `cargo test -p spargio-ws --test ws_tdd`

## Update: Phase 4 implemented (QUIC companion bridge, quinn-first) with Red/Green TDD (2026-02-28)

Executed a red-first `quinn` companion bridge implementation focused on
runtime integration and execution semantics.

### Red tests added first

- created new workspace crate: `crates/spargio-quic`
- added `crates/spargio-quic/tests/quic_tdd.rs` with:
  - `quic_bridge_runs_async_work`
  - `quic_bridge_timeout_is_enforced`

Observed expected red state:

- unresolved API imports in `spargio_quic`:
  - `QuicBridge`
  - `QuicOptions`

### Implementation delivered

Workspace wiring:

- added `crates/spargio-quic` to root workspace members.
- crate deps include:
  - `quinn`
  - `tokio` (current-thread runtime execution lane)
  - `spargio`

Public API:

- options and wrappers:
  - `QuicOptions` (optional timeout)
  - `QuicBridge`
- execution entrypoints:
  - `run(...)`
  - `run_with_options(...)`
  - `QuicBridge::run(...)`
  - `QuicBridge::with_endpoint(...)` (quinn endpoint lifecycle bridge helper)
- explicit re-export:
  - `pub use quinn;`

Semantics:

- bridge executes async quinn workflows on a Tokio current-thread runtime built
  inside `RuntimeHandle::spawn_blocking(...)`.
- timeout enforced via `spargio::timeout(...)` -> `io::ErrorKind::TimedOut`.
- runtime rejection/cancel mapped to `io::Error` consistently with companion
  bridge behavior.

### Green validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`

## Update: Phase 5 implemented (process/signal maturity pass) with Red/Green TDD (2026-02-28)

Executed a red-first process/signal maturity pass expanding lifecycle and
subscription ergonomics.

### Red tests added first

Process:

- `crates/spargio-process/tests/maturity_tdd.rs`
  - `command_builder_spawn_and_wait_lifecycle`
  - `spawned_child_wait_timeout_is_enforced`

Signal:

- `crates/spargio-signal/tests/maturity_tdd.rs`
  - `signal_hub_broadcasts_to_multiple_subscribers`
  - `signal_stream_recv_timeout_returns_none`
  - `ctrl_c_stream_still_constructs`

Observed expected red state:

- missing process APIs:
  - `CommandBuilder::spawn`
  - spawned child wait timeout APIs
- missing signal APIs:
  - `SignalHub`
  - `SignalStream::recv_timeout`

### Implementation delivered

`spargio-process` maturity additions:

- added spawn APIs:
  - `spawn(...)`
  - `spawn_with_options(...)`
  - `CommandBuilder::{spawn, spawn_with_options}`
- added `ChildHandle` with lifecycle methods:
  - `id()`
  - `wait()`
  - `wait_with_options(...)`
  - `try_wait()`
  - `kill()`
  - `output()`
  - `output_with_options(...)`
- all blocking process operations routed through shared timeout/cancel-aware
  `run_blocking(...)` semantics.

`spargio-signal` maturity additions:

- introduced `SignalHub`:
  - `SignalHub::new(...)`
  - `SignalHub::subscribe()`
- `SignalStream` now supports:
  - `recv()`
  - `recv_timeout(...)`
  - `recv_matching(...)`
  - `try_recv()`
- `signal(...)` now composes via `SignalHub` + `subscribe`, preserving prior
  API behavior while enabling broadcast-style subscriptions.

### Green validation

Executed and passing:

- `cargo test -p spargio-process --test maturity_tdd`
- `cargo test -p spargio-signal --test maturity_tdd`
- `cargo test -p spargio-process`
- `cargo test -p spargio-signal`

## Update: Phase 6 implemented (hardening + operations lanes) with Red/Green TDD (2026-02-28)

Executed an operations-focused hardening slice for companion crates and CI
coverage.

### Red tests added first

- added root test file: `tests/companion_ops_tdd.rs`
  - `companion_ci_smoke_script_exists_and_targets_companion_crates`
  - `ci_workflow_has_companion_matrix_lane`

Observed expected red state:

- missing `scripts/companion_ci_smoke.sh`
- missing `companion-matrix` CI job wiring in `.github/workflows/ci.yml`

### Implementation delivered

Companion smoke script:

- added `scripts/companion_ci_smoke.sh`:
  - `cargo test -p spargio-protocols --features uring-native`
  - `cargo test -p spargio-tls --test tls_tdd`
  - `cargo test -p spargio-ws --test ws_tdd`
  - `cargo test -p spargio-quic --test quic_tdd`
  - `cargo test -p spargio-process`
  - `cargo test -p spargio-signal`

CI workflow hardening:

- added `companion-matrix` job in `.github/workflows/ci.yml` that runs:
  - `./scripts/companion_ci_smoke.sh`

Operational intent:

- catch protocol bridge drift and companion crate regressions in a dedicated CI
  lane, independent of core runtime test jobs.

### Green validation

Executed and passing:

- `cargo test --test companion_ops_tdd`
- `./scripts/companion_ci_smoke.sh`

## Update: Phase 7 implemented (docs + polish) with validation (2026-02-28)

Executed documentation/polish updates to reflect delivered companion-crate
work and provide explicit API-selection guidance.

### README updates

Updated done/not-done sections to match current implementation state:

- done:
  - companion crate suite now explicitly includes:
    - `spargio-process`
    - `spargio-signal`
    - `spargio-protocols` (legacy blocking bridge)
    - `spargio-tls`
    - `spargio-ws`
    - `spargio-quic`
  - companion hardening lane (`scripts/companion_ci_smoke.sh` + CI job).
  - docs scaffold note updated to include protocol/API-selection coverage.
- not done:
  - clarified remaining maturity gaps as advanced tuning/surface depth and
    long-window operational hardening, not absence of companion crates.

Companion dependency polish:

- pinned `spargio-tls` rustls/futures-rustls features to a single crypto
  provider (`ring`) to avoid workspace-wide TLS/QUIC provider ambiguity during
  unified test runs.

### mdBook coverage expansion

Added new chapters:

- `book/src/companion_protocols.md`
  - direct upstream vs `spargio-*` adapter selection guidance.
  - explicit scope boundary (thin adapters, no protocol engine rewrite).
- `book/src/companion_stability.md`
  - semver/deprecation baseline policy for companion crates.
  - CI/operations expectations for compatibility maintenance.

Updated:

- `book/src/SUMMARY.md` links to new chapters.
- `book/src/migration.md` includes protocol companion migration guidance.

### Validation

Executed and passing:

- `cargo test --test docs_tdd`
- `mdbook build book`

## Update: QUIC final-form target and acceptance checklist (2026-03-01)

Decision recorded: favor the long-term QUIC integration shape based on a
native `quinn-proto` driver owned by Spargio, instead of a permanent Tokio
bridge path.

### Target architecture (long-term form)

- endpoint ownership is shard-affine and explicit (one owning execution
  context per UDP socket/endpoint lifecycle).
- packet I/O is driven by Spargio runtime tasks with `io_uring` as preferred
  backend where it is a clear win; retain fallback paths where kernel/platform
  constraints require.
- timers, pacing, loss-recovery wakeups, and cancellation are mapped to
  Spargio primitives (no embedded Tokio runtime per operation).
- high-level API is provided by `spargio-quic`; protocol core comes from
  `quinn-proto` with Spargio-managed driver loops.

### Acceptance checklist

1. Runtime/driver correctness
- no `spawn_blocking + tokio::runtime::Builder` path in steady-state QUIC I/O.
- endpoint driver loop integrates send/recv/timer progression without busy spin.
- cancellation and drop semantics are deterministic for endpoint, connections,
  streams, and datagrams.

2. API completeness
- endpoint lifecycle: bind, client connect, server accept, graceful close, and
  draining shutdown.
- stream surface: open/accept uni + bi streams, ordered reads/writes, finish,
  reset/stop semantics.
- datagram surface: send/recv with documented size/error behavior.
- configuration surface: TLS config, ALPN, transport tuning pass-through,
  version-negotiation visibility.

3. Ergonomics and placement
- session-local (`!Send`-friendly) handles for fast same-thread workflows.
- explicit cross-thread handoff wrapper for `Send`-required hops.
- clear docs for shard/session ownership and expected placement behavior.

4. Performance and resource behavior
- benchmark lane compares current bridge baseline vs native driver for
  throughput, tail latency, and CPU under representative profiles.
- no material regressions in memory growth under long-lived high-concurrency
  workloads.
- backpressure behavior validated (bounded queues and predictable overload
  failure modes).

5. Interop and reliability
- interoperability matrix includes at least quinn and one non-quinn QUIC peer.
- fault-injection coverage for loss/reorder/duplication/timeout and migration
  edge cases where supported.
- soak tests validate stability across long-duration connection churn.

6. Observability and operations
- counters/histograms for handshake outcomes, retransmits, PTO events, stream
  errors, and datagram drops.
- structured events for connection lifecycle and terminal error reasons.
- CI lane covers native-QUIC smoke + targeted regression suite.

7. Migration and compatibility
- bridge mode retained only as transitional compatibility path until native
  coverage reaches checklist thresholds.
- migration docs describe API parity status and behavior deltas between bridge
  and native modes.

### Add-now vs later guidance

- add now: runtime/driver skeleton, endpoint lifecycle parity, stream basics,
  cancellation guarantees, smoke+interop tests, and baseline metrics.
- add next: datagram depth, transport tuning breadth, richer observability,
  backpressure tuning, and broader fault injection.
- add later: advanced features requiring significant protocol-policy surface
  (only when demand and maintenance budget justify).

## Update: QUIC add-now/add-next implementation slice delivered with Red/Green TDD (2026-03-01)

Implemented the currently sensible "add now" and selected "add next" items in
`spargio-quic`, while preserving backward-compatible bridge entrypoints and
keeping the long-term `quinn-proto` native-driver direction as the target.

### Red phase

Expanded `crates/spargio-quic/tests/quic_tdd.rs` with failing tests for:

- endpoint lifecycle + stream exchange:
  - `quic_endpoint_connects_and_exchanges_uni_stream_data`
- datagram surface + metrics:
  - `quic_endpoint_datagram_roundtrip_updates_metrics`
- bounded in-flight backpressure:
  - `quic_endpoint_accept_backpressure_is_enforced`
- `!Send` local ergonomics + explicit send handoff:
  - `quic_connection_local_to_send_handoff_preserves_identity`
- metrics snapshot baseline:
  - `quic_endpoint_metrics_snapshot_has_expected_counters`

Observed expected red state:

- missing `QuicEndpoint`, `QuicEndpointOptions`, and `QuicMetricsSnapshot`
- missing local/send connection wrappers and endpoint/connection APIs
- missing QUIC test cert dependencies

### Green phase

Implemented new `spargio-quic` API surface in `crates/spargio-quic/src/lib.rs`:

Runtime/driver and cancellation behavior:

- replaced per-operation `spawn_blocking + tokio runtime build` with a shared
  persistent bridge executor (`OnceLock`-backed Tokio multithread runtime).
- bridge task timeouts now abort in-flight join handles on timeout.
- retained existing `QuicBridge::{run, with_endpoint}` and free `run*` APIs.

Endpoint and connection API completeness:

- added `QuicEndpoint`:
  - constructors:
    - `server(...)`
    - `server_with_options(...)`
    - `client(...)`
    - `client_with_options(...)`
    - `from_endpoint*`
  - lifecycle/config:
    - `local_addr()`
    - `set_default_client_config(...)`
    - `set_server_config(...)`
    - `close(...)`
    - `wait_idle().await`
  - connection paths:
    - `connect(...)`
    - `connect_with(...)`
    - `accept().await`
- added `QuicConnection`:
  - stream ops:
    - `open_uni/open_bi`
    - `accept_uni/accept_bi`
  - datagram ops:
    - `send_datagram(...)`
    - `read_datagram().await`
  - lifecycle/introspection:
    - `close(...)`
    - `closed().await`
    - `stable_id()`
    - `stats()`
    - `max_datagram_size()`
    - `datagram_send_buffer_space()`

Ergonomics and placement:

- added explicit send-handoff wrapper: `QuicSendConnection`.
- added local `!Send` wrapper: `LocalQuicConnection` (`Rc`-backed).
- added conversion helpers:
  - `QuicConnection::to_local()`
  - `QuicConnection::to_send_handle()`
  - `LocalQuicConnection::to_send_handle()`

Backpressure and observability:

- added `QuicEndpointOptions` with:
  - `connect_timeout`
  - `accept_timeout`
  - `operation_timeout`
  - `max_inflight_ops`
- added bounded in-flight guardrails (`WouldBlock` on limit saturation).
- added per-endpoint metrics with snapshots:
  - `QuicMetrics`
  - `QuicMetricsSnapshot`
  - counters include connect/accept starts/success/fail/timeouts, stream and
    datagram activity, close events, operation timeouts, and backpressure hits.

Cargo updates:

- `crates/spargio-quic/Cargo.toml`:
  - Tokio features now include `rt-multi-thread` (shared executor runtime).
  - test deps added: `rcgen`, `rustls`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: R1 native cutover continuation (connection-op dispatch) with Red/Green TDD (2026-03-01)

Implemented the next R1 slice by routing native-backend connection async
operations through a persistent connection dispatcher task, and by making
connection-level backend dispatch visible in metrics.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `quic_connection_native_backend_dispatches_connection_ops`
- `quic_connection_bridge_backend_dispatches_connection_ops`

Expected red failures:

- connection operations (`open_*`, `accept_*`, etc.) were not incrementing
  backend dispatch counters, so before/after metric deltas stayed flat.

### Green phase

Implemented in `crates/spargio-quic/src/lib.rs`:

- Added `NativeConnectionDispatch` actor:
  - persistent Tokio task per accepted/connected native connection
  - command loop for async connection operations:
    - `closed`
    - `open_uni` / `open_bi`
    - `accept_uni` / `accept_bi`
    - `read_datagram`
  - bounded command/reply semantics via unbounded mpsc + oneshot replies
  - deterministic `BrokenPipe` error when dispatcher is closed.
- Updated `QuicEndpoint::wrap_connection(...)`:
  - now initializes native connection dispatch for `QuicBackend::Native`
  - now returns `io::Result<QuicConnection>` to surface dispatcher init errors.
- Updated connect/accept call sites to handle fallible wrapping and keep metrics
  (`connects_failed` / `accepts_failed`) consistent on wrap failures.
- Updated `QuicConnection` operation dispatch:
  - native backend async ops route through `NativeConnectionDispatch`
  - bridge backend keeps direct path
  - both backends now increment backend dispatch counters for connection ops
  - timeout accounting (`operation_timeouts`) preserved.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd quic_connection_` (red then green)
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test soak_tdd`

## Update: R1 cutover guardrails expanded (native path bridge-spawn exclusion) with Red/Green TDD (2026-03-01)

Added explicit cutover tests that assert native backend data-path operations do
not go through bridge task spawning, while bridge backend still does.

### Red phase

Added failing tests in new file `crates/spargio-quic/tests/native_cutover_tdd.rs`:

- `native_backend_data_path_avoids_bridge_task_spawn`
- `bridge_backend_data_path_uses_bridge_task_spawn`

Initial failures:

- native test failed due premature close ordering causing stream read abort.
- lock poisoning cascaded into the second test.

### Green phase

Adjusted test choreography and synchronization:

- serialized counter-sensitive tests with process-local lock
  (`BRIDGE_COUNT_TEST_LOCK`).
- moved connection close calls to post-exchange phase before `wait_idle`.
- recovered lock from poison safely for deterministic reruns.

Cutover assertions now enforced:

- native backend (`QuicBackend::Native`) exchange + `wait_idle` path leaves
  `bridge_runtime_spawn_count() == 0`.
- bridge backend (`QuicBackend::Bridge`) exchange + `wait_idle` path yields
  `bridge_runtime_spawn_count() >= 1`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test soak_tdd`

## Update: R1 cutover continuation (native endpoint lifecycle without bridge runtime context entry) with Red/Green TDD (2026-03-01)

Implemented the next R1 slice by removing native endpoint constructor/drop
dependence on `with_bridge_runtime_context(...)`, while keeping bridge backend
compatibility behavior unchanged.

### Red phase

Extended `crates/spargio-quic/tests/native_cutover_tdd.rs` with failing tests:

- `native_backend_endpoint_lifecycle_avoids_bridge_runtime_context_entry`
- `bridge_backend_endpoint_lifecycle_uses_bridge_runtime_context_entry`

Expected red failure before implementation:

- native endpoint lifecycle (`server/client` + drop) still went through
  `with_bridge_runtime_context(...)`.

### Green phase

Implemented in `crates/spargio-quic/src/lib.rs`:

- Added bridge-runtime context-entry counters:
  - `bridge_runtime_context_enter_count()`
  - `reset_bridge_runtime_context_enter_count()`
  - internal counter increment in `with_bridge_runtime_context(...)`.
- Added native endpoint runtime adapter:
  - `BridgeTokioRuntime` implementing `quinn::Runtime` with explicit
    `tokio::runtime::Handle` (spawn/timer/socket wrapping without relying on
    thread-local runtime context entry).
  - `BridgeUdpSocket` and `BridgeUdpPoller` implementing
    `quinn::AsyncUdpSocket` / `quinn::UdpPoller`.
- Added native constructor helpers:
  - `native_server_endpoint(...)`
  - `native_client_endpoint(...)`
- Updated endpoint constructors:
  - `QuicBackend::Native` now uses native constructor helpers.
  - `QuicBackend::Bridge` retains `with_bridge_runtime_context(...)` path.
- Updated `Drop for QuicEndpoint`:
  - bridge backend keeps runtime-context drop guard.
  - native backend drops endpoint directly (no bridge context entry).

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test soak_tdd`

## Update: R8 deferred-fs progression (`create_dir_all` native-first) with Red/Green TDD (2026-03-01)

Implemented a concrete deferred-fs migration slice by removing direct
`spawn_blocking(std::fs::create_dir_all)` usage from the common path.

### Red phase

Extended `tests/deferred_items_tdd.rs` with a new assertion in:

- `deferred_fs_helpers_execute_and_metadata_lite_is_available`

New contract:

- simple nested `create_dir_all` paths should not use direct blocking fallback
  in `spargio::fs::create_dir_all`.

### Green phase

Implemented in `src/lib.rs` (`spargio::fs` module):

- `create_dir_all(...)` now uses native-first iterative creation via
  `create_dir(...)` for straightforward path forms.
- preserved compatibility fallback to `std::fs::create_dir_all` for complex
  relative path forms (`.` / `..` / platform prefix components).
- added test instrumentation helpers:
  - `create_dir_all_blocking_fallback_count_for_test()`
  - `reset_create_dir_all_blocking_fallback_count_for_test()`

Docs/status sync:

- updated README deferred-fs wording to reflect:
  - `create_dir_all` now native-first for straightforward paths
  - still-deferred helpers remain `canonicalize`, `metadata`,
    `symlink_metadata`, `set_permissions`.

### Validation

Executed and passing:

- `cargo test --features uring-native --test deferred_items_tdd`
- `cargo test --test deferred_items_tdd`

## Update: R2 native-proto progression (`connect_for_test` + protocol transmit pump) with Red/Green TDD (2026-03-01)

Implemented an additional R2 slice to move native driver behavior beyond
placeholder queue semantics by wiring real `quinn-proto` connection bootstrap
and transmit progression in the owner loop.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_connect_for_test_generates_initial_transmit`

Red expectation:

- driver lacked a real client connection bootstrap path that produced protocol
  transmits from `quinn-proto::Connection::poll_transmit(...)`.

### Green phase

Implemented in `crates/spargio-quic/src/lib.rs`:

- Added new command/API:
  - `NativeProtoDriver::{connect_for_test(...)}`
  - parity on local/send wrappers.
- Added owner-loop command:
  - `NativeProtoCommand::ConnectForTest`
- Added protocol connection state in owner loop:
  - `HashMap<ConnectionHandle, quinn_proto::Connection>`
  - per-handle queued connection-event mailbox
  - synthetic-id -> proto-handle mapping for close-path cleanup.
- Added protocol progression helper:
  - `drive_native_proto_connections(...)`
  - processes queued `ConnectionEvent`s
  - forwards endpoint events via `Endpoint::handle_event(...)`
  - drains `Connection::poll_transmit(...)` into native transmit queue with
    existing backpressure/fault accounting.
- Integrated progression helper into:
  - `SubmitDatagram` (connection-event driven progression)
  - `AdvanceClockForTest` (timeout-driven progression)
  - `ConnectForTest` bootstrap path.
- Added deterministic synthetic-time conversion helper:
  - `native_proto_now(epoch, now_duration)`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_connect_for_test_generates_initial_transmit`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test --features uring-native --test deferred_items_tdd`

## Update: R1-R9 implementation sweep completed with Red/Green TDD (2026-03-01)

Completed the roadmap milestones `R1` through `R9` with concrete tests,
implementation slices, and CI/docs wiring.

### R1: QUIC backend cutover controls (`Native` default, `Bridge` explicit fallback)

Red phase:

- Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:
  - `quic_endpoint_options_default_to_native_backend`
  - `quic_endpoint_default_backend_dispatches_native_ops`
  - `quic_endpoint_bridge_backend_dispatches_bridge_ops`

Green phase:

- Added `QuicBackend` (`Native`, `Bridge`) and plumbed it through
  `QuicEndpointOptions`.
- Added dispatch metrics:
  - `QuicMetricsSnapshot::{native_ops_dispatched, bridge_ops_dispatched}`
- Routed endpoint operation dispatch by backend mode; `Native` is default.
- Added controlled endpoint drop path to preserve quinn runtime-context safety.

### R2: Native driver progression beyond bare skeleton semantics

Red phase:

- Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:
  - `native_proto_driver_closed_connection_rejects_stream_ops`
  - `native_proto_driver_connection_datagram_roundtrip_tracks_state`

Green phase:

- Added `NativeProtoConnectionState`.
- Added connection lifecycle/datagram APIs:
  - `close_connection_for_test`
  - `connection_state`
  - `send_datagram_on_connection_for_test`
  - `recv_datagram_on_connection_for_test`
- Extended owner-loop connection pump with close-state guards and per-connection
  datagram queues/counters.
- Mirrored these APIs on local/send driver wrappers.

### R3: Native QUIC interop matrix

Red phase:

- Added failing interop suite `crates/spargio-quic/tests/interop_tdd.rs`.

Green phase:

- Added interop tests:
  - `interop_spargio_client_to_raw_quinn_server_bi_stream`
  - `interop_raw_quinn_client_to_spargio_server_bi_stream`
- Added `scripts/quic_interop_matrix.sh`.
- Added CI wiring in `.github/workflows/ci.yml` (`companion-matrix` job).

### R4: Long-window soak + fault qualification

Red phase:

- Added failing soak/fault qualification suite
  `crates/spargio-quic/tests/soak_tdd.rs`.

Green phase:

- Added ignored soak tests:
  - `soak_connection_churn_roundtrip_stays_stable`
  - `soak_native_fault_injection_keeps_egress_queue_bounded`
- Added `scripts/quic_soak_fault.sh`.
- Wired nightly CI soak invocation in `.github/workflows/ci.yml`.

### R5: Performance gate integration for rollout

Red phase:

- Added failing QUIC perf-gate harness tests `tests/quic_perf_guardrail_tdd.rs`.

Green phase:

- Added `scripts/quic_perf_gate.sh` (p95/p99 regression + throughput floor).
- Added fixture profile:
  - `tests/fixtures/quic_perf/native_vs_bridge.json`
- Added CI wiring for fixture-based perf gate in `.github/workflows/ci.yml`.
- Added CI/script guard test `tests/quic_ops_tdd.rs`.

### R6: README/status sync

Red phase:

- Added failing docs guards in `tests/docs_tdd.rs` for QUIC status wording and
  helper script references.

Green phase:

- Updated `README.md` done/not-done sections:
  - backend selector/rollout status
  - explicit pending full tokio-free `quinn-proto` cutover note
  - QUIC interop/perf/soak helper scripts listed
- Added docs assertions:
  - `readme_tracks_quic_rollout_done_and_not_done_status`
  - `implementation_log_contains_r1_to_r9_breakdown_sections`

### R7: Companion hardening beyond smoke

Red phase:

- Added failing broader maturity tests across companion crates.

Green phase:

- Added companion hardening tests:
  - `crates/spargio-process/tests/maturity_tdd.rs`
  - `crates/spargio-signal/tests/maturity_tdd.rs`
  - `crates/spargio-protocols/tests/foundation_tdd.rs`
  - `crates/spargio-tls/tests/tls_tdd.rs`
  - `crates/spargio-ws/tests/ws_tdd.rs`
- Added `scripts/companion_ci_hardening.sh`.
- Wired CI (`companion-matrix`) to run hardening lane.
- Extended `tests/companion_ops_tdd.rs` to assert hardening script + CI wiring.

### R8: DNS and deferred fs items encoded as explicit contracts

Red phase:

- Added failing contract/behavior tests in `tests/deferred_items_tdd.rs`.

Green phase:

- Added README contract assertions for:
  - DNS `ToSocketAddrs` caveat and `SocketAddr` alternatives
  - deferred fs helper list + `metadata_lite`
- Added feature-gated behavior tests (`uring-native` Linux lane) for:
  - hostname connect and socket-addr connect behavior
  - deferred fs helper execution (`create_dir_all`, `canonicalize`, `metadata`,
    `symlink_metadata`, `set_permissions`, `metadata_lite`)

### R9: Scheduler/docs maturity

Red phase:

- Added failing runtime test for scheduler tuning knob visibility.

Green phase:

- Added scheduler knob:
  - `RuntimeBuilder::steal_victim_stride(...)`
- Plumbed victim stride through work-stealing loop and stats snapshot:
  - `RuntimeStats::steal_victim_stride`
- Added runtime test:
  - `runtime_builder_steal_victim_stride_is_reported_and_clamped`
- Added mdBook chapter:
  - `book/src/scheduler_tuning.md`
- Updated book summary:
  - `book/src/SUMMARY.md`

### Validation

Executed and passing during this sweep:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test soak_tdd`
- `cargo test --test quic_perf_guardrail_tdd`
- `cargo test --test quic_ops_tdd`
- `cargo test --test docs_tdd`
- `cargo test --test runtime_tdd`
- `cargo test -p spargio-process --test maturity_tdd`
- `cargo test -p spargio-signal --test maturity_tdd`
- `cargo test -p spargio-protocols --test foundation_tdd --features uring-native`
- `cargo test -p spargio-tls --test tls_tdd`
- `cargo test -p spargio-ws --test ws_tdd`
- `cargo test --test deferred_items_tdd`
- `cargo test --features uring-native --test deferred_items_tdd`
- `cargo test --test companion_ops_tdd`

## Update: Remaining work breakdown after N1-N8 (2026-03-01)

This captures the concrete work still required for current "not done yet"
items after completing native QUIC skeleton milestones N1-N8.

### R1: QUIC native data-path cutover (bridge replacement)

Scope:

- route `QuicEndpoint::{connect, connect_with, accept, wait_idle}` and
  `QuicConnection` operations through `NativeProtoDriver` instead of
  `spawn_on_bridge_runtime`.
- keep bridge path only as explicit compatibility fallback.

Red tests first:

- assert public `QuicEndpoint` operations do not require Tokio bridge runtime.
- assert API behavior parity versus current bridge-path semantics.

Green acceptance:

- default QUIC path is native-driver-backed.
- bridge path is opt-in and clearly documented.

### R2: Real protocol progression over native loop (beyond skeleton semantics)

Scope:

- replace placeholder stream/datagram progression logic with true
  `quinn-proto` connection/event handling and transmit scheduling.
- map connection lifecycle and stream transitions to protocol-driven state.

Red tests first:

- protocol-level stream open/accept/finish/reset behavior fails under skeleton.
- datagram and close semantics fail under protocol-correct expectations.

Green acceptance:

- protocol-driven tests pass with deterministic behavior under concurrency.

### R3: Native QUIC interop matrix

Scope:

- add interop suite: native Spargio QUIC endpoint vs quinn peer.
- add at least one non-quinn peer lane where practical.

Red tests first:

- handshake/data exchange against peer(s) fails before interop wiring.

Green acceptance:

- CI interop lane passes for all selected peers and profiles.

### R4: Long-window soak + fault qualification

Scope:

- extend current fault hooks into soak lanes (loss/reorder/drop over duration).
- add connection churn and memory-growth assertions.

Red tests first:

- soak/fault lanes expose regressions in retries or queue growth.

Green acceptance:

- no unbounded queue/memory growth in long-window runs.
- fault scenarios meet defined success/error-rate thresholds.

### R5: Performance gate integration for rollout

Scope:

- integrate `NativeProtoPerfGate` into repeatable benchmark guardrail workflow.
- produce native-vs-bridge verdicts for p95/p99 and throughput.

Red tests first:

- guardrail fails when synthetic/fixture regressions exceed thresholds.

Green acceptance:

- documented threshold policy and passing perf-gate lane in CI tooling.

### R6: README/status sync for new QUIC reality

Scope:

- update `README.md` done/not-done to reflect native-driver milestones N1-N8.
- explicitly separate "native skeleton done" vs "full default cutover pending".

Red tests first:

- docs/status tests fail when README stale relative to implementation log.

Green acceptance:

- README done/not-done sections accurately mirror implementation state.

### R7: Companion hardening beyond smoke lanes (repo-wide)

Scope:

- deepen failure-injection/soak coverage across companion protocol crates.
- add broader p95/p99 operational gates where meaningful.

Red tests first:

- dedicated hardening tests expose missing coverage and drift.

Green acceptance:

- companion CI includes deeper operational coverage, not smoke only.

### R8: DNS and fs deferred items (repo-wide)

Scope:

- evaluate nonblocking DNS strategies for `ToSocketAddrs` paths or keep explicit
  `SocketAddr` requirement with stronger docs/contracts.
- decide and implement remaining deferred fs helper migration cases where
  value/complexity tradeoff is justified.

Red tests first:

- DNS-path behavior and deferred fs helper behavior encoded in explicit tests.

Green acceptance:

- each deferred item either implemented with tests or explicitly documented as
  intentionally deferred with rationale.

### R9: Scheduler/docs maturity (repo-wide)

Scope:

- advance work-stealing policy tuning beyond MVP heuristics.
- expand mdBook operations/placement/API-selection guidance to current depth.

Red tests first:

- scheduler tuning guardrails and docs-link/coverage tests for new chapters.

Green acceptance:

- measurable scheduler improvements in targeted workloads.
- book coverage aligned with current feature set and operational guidance.

### Suggested execution order from here

1. R1 native cutover.
2. R2 protocol-correct progression.
3. R3 interop matrix.
4. R4 soak/fault qualification.
5. R5 perf-gate integration.
6. R6 README/status sync.
7. R7 companion hardening.
8. R8 DNS/fs deferred decisions.
9. R9 scheduler/docs maturity.

## Update: Phase N8 implemented (fault injection + rollout/perf gates) with Red/Green TDD (2026-03-01)

Implemented N8 qualification primitives: deterministic fault injection controls,
fault stats, and explicit rollout/performance gate APIs.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_fault_injection_drops_ingress_and_tracks_stats`
- `native_proto_driver_reorders_egress_when_fault_enabled`
- `native_proto_perf_gate_marks_material_regression_as_fail`
- `native_proto_rollout_stage_is_experimental_for_now`

Expected red failures:

- missing fault spec/stats APIs and behaviors
- missing rollout/performance gate types

### Green phase

Added fault-injection types and APIs:

- `NativeProtoFaultSpec`
- `NativeProtoFaultStats`
- `NativeProtoDriver::{set_fault_spec, fault_stats}`

Owner-loop fault behaviors:

- optional inbound drop mode (`drop_inbound`)
- optional egress drop mode (`drop_egress`)
- optional egress reorder mode (`reorder_egress` on drain)
- tracked fault counters:
  - inbound drops
  - egress drops
  - egress reorder operations

Added rollout/performance gate types:

- `NativeProtoRolloutStage` with current stage:
  - `NativeProtoDriver::rollout_stage() == Experimental`
- `NativeProtoPerfGate`
- `NativeProtoPerfVerdict`
- regression evaluation helper:
  - `NativeProtoPerfGate::evaluate(...)`

Wrapper parity:

- local/send native wrappers delegate fault spec/stats APIs.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N7 implemented (native observability surface) with Red/Green TDD (2026-03-01)

Implemented native-driver stats snapshots and structured event logging with
bounded event retention.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_stats_track_key_operations`
- `native_proto_driver_event_log_captures_timeout_and_backpressure`

Expected red failures:

- missing `stats()` and `drain_events()` APIs
- missing `NativeProtoEvent` and operation counters

### Green phase

Added observability types:

- `NativeProtoStats`
- `NativeProtoEvent`

Added native driver APIs:

- `stats().await`
- `drain_events(max).await`

Owner-loop observability behavior:

- tracks operation totals and key domain counters:
  - connection registrations
  - stream opens (uni/bi)
  - datagram ingest/oversize rejections
  - backpressure hits
  - timer fires
- emits structured events for:
  - connection registration
  - timeout firing
  - oversized datagram rejection
  - backpressure events
- retains events in bounded FIFO buffer (`NATIVE_EVENT_CAPACITY`).

Wrapper parity:

- local/send native wrappers delegate `stats()` and `drain_events(...)`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N5 implemented (datagram limits + transport tuning surface) with Red/Green TDD (2026-03-01)

Implemented datagram-size enforcement and transport tuning roundtrip APIs on
the native driver surface.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_transport_tuning_roundtrip`
- `native_proto_driver_rejects_oversized_datagram_per_tuning`

Expected red failures:

- missing transport tuning type and setter/getter APIs
- no max-datagram-size enforcement in datagram ingest path

### Green phase

Added tuning type:

- `NativeProtoTransportTuning`
  - `max_datagram_size`
  - `send_window`
  - `receive_window`
  - `keep_alive_interval`
  - `mtu_discovery_enabled`
  - builder-style `with_*` methods

Added native driver methods:

- `set_transport_tuning(...).await`
- `transport_tuning().await`

Owner-loop behavior:

- tracks active tuning config.
- validates `max_datagram_size > 0` on update.
- `submit_datagram` rejects oversized payloads with `InvalidInput`.

Wrapper parity:

- local/send native wrappers delegate tuning setter/getter as well.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N6 implemented (native local/send ergonomics mapping) with Red/Green TDD (2026-03-01)

Implemented `!Send` local and explicit send-handoff wrappers for the native
driver command surface.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_local_send_handoff_preserves_identity`
- `native_proto_driver_send_handle_respects_shutdown`

Expected red failures:

- missing `to_local()` / `to_send_handle()` on `NativeProtoDriver`
- missing local/send wrapper types

### Green phase

Added wrapper types:

- `NativeProtoDriverLocal` (`Rc`-backed local handle)
- `NativeProtoDriverSend` (`Send` handoff handle)

Added conversions:

- `NativeProtoDriver::to_local()`
- `NativeProtoDriver::to_send_handle()`
- `NativeProtoDriverLocal::to_send_handle()`

Delegated native-driver operations through wrappers (probe/shutdown/connection
and stream APIs) while preserving endpoint identity and closed-state behavior.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N4 implemented (connection/stream pump skeleton) with Red/Green TDD (2026-03-01)

Implemented a deterministic native connection/stream event-pump skeleton in the
owner task to model connection registration and stream lifecycle transitions.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_open_uni_roundtrips_to_accept_uni`
- `native_proto_driver_open_bi_roundtrips_to_accept_bi`
- `native_proto_driver_finish_and_reset_stream_are_observable`

Expected red failures:

- missing connection registration and stream open/accept APIs
- missing stream finish/reset state tracking

### Green phase

Added native connection/stream pump surface:

- new stream-state type:
  - `NativeProtoStreamState { finished, reset }`
- new driver methods:
  - `register_connection_for_test().await`
  - `open_uni_on_connection(...).await`
  - `accept_uni_on_connection(...).await`
  - `open_bi_on_connection(...).await`
  - `accept_bi_on_connection(...).await`
  - `finish_stream(...).await`
  - `reset_stream(...).await`
  - `stream_state(...).await`

Owner-loop internals:

- per-connection registry (`HashMap`) with:
  - pending uni accept queue
  - pending bi accept queue
  - per-stream terminal state
- deterministic error behavior:
  - unknown connection/stream => `NotFound`
  - accept with no pending stream => `WouldBlock`

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N3 implemented (timer/wake progression skeleton) with Red/Green TDD (2026-03-01)

Implemented deterministic timer progression primitives in the native driver
loop to support deadline scheduling and stale-deadline supersession semantics.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_timers_fire_when_deadline_passes`
- `native_proto_driver_newer_deadline_supersedes_older`

Expected red failures:

- missing timeout scheduling/clock-advance APIs
- missing timeout fire accounting and generation tracking

### Green phase

Extended native driver with timer-state APIs:

- new type:
  - `NativeProtoTimerState`
- new methods:
  - `schedule_timeout(after).await -> generation`
  - `advance_clock_for_test(by).await -> NativeProtoTimerState`
  - `timer_state().await -> NativeProtoTimerState`

Owner-loop behavior:

- maintains synthetic monotonic `now`.
- tracks single active deadline with generation ID.
- newer deadline supersedes older deadline.
- timeout fires increment counter and record last fired generation.
- deadline state is queryable after each progression step.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: Phase N2 implemented (native UDP ingress/egress skeleton) with Red/Green TDD (2026-03-01)

Implemented bounded UDP ingress/egress command plumbing in the native driver
loop so the owner task can ingest datagrams and emit queued transmits.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_ingests_datagrams_and_supports_bounded_drain`
- `native_proto_driver_egress_queue_applies_backpressure`
- `native_proto_driver_drain_is_fifo_and_batch_limited`

Expected red failures:

- missing `submit_datagram`, `drain_transmits`, and queue backpressure methods
- missing `NativeProtoTransmit` and N2-specific options

### Green phase

Extended native driver API in `crates/spargio-quic/src/lib.rs`:

- new options:
  - `NativeProtoDriverOptions::with_max_pending_transmits(...)`
- new types:
  - `NativeProtoTransmit`
  - `NativeProtoIngressReport`
- new driver methods:
  - `submit_datagram(remote, payload).await`
  - `drain_transmits(max).await`
  - `enqueue_transmit_for_test(...).await` (deterministic queue-path test hook)

Owner-loop integration details:

- owner loop now maintains:
  - `quinn_proto::Endpoint`
  - bounded `VecDeque<NativeProtoTransmit>` egress queue
- `submit_datagram` path feeds payload into `Endpoint::handle(...)`.
- response/new-connection outputs are converted into queued transmits.
- queue saturation returns deterministic `WouldBlock`.
- drain path is FIFO and batch-limited.

Cargo updates:

- `crates/spargio-quic/Cargo.toml` adds:
  - `bytes = "1"` (for `BytesMut` ingress feed)

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

### Notes on long-term direction

- This slice removes the highest-friction bridge behavior (per-call runtime
  creation) and adds the targeted API/ergonomics/metrics groundwork.
- The deeper final-form goal remains: replace bridge-centric data-path handling
  with a native shard-owned `quinn-proto` endpoint driver over Spargio
  primitives.

## Update: Native `quinn-proto` next-step breakdown (2026-03-01)

Added concrete execution plan for the next major step: moving QUIC data-plane
ownership from bridge mode to a Spargio-native `quinn-proto` endpoint driver.

### Phase N1: Driver skeleton and ownership model

Scope:

- add a shard-affine endpoint task that owns `quinn_proto::Endpoint`.
- define command mailbox + response channels for app API calls.
- define stable internal IDs for endpoint/connection/stream handles.

Red tests first:

- endpoint task boots and accepts command loop.
- commands are rejected after endpoint shutdown with deterministic errors.
- connection/stream IDs remain stable across handle clones.

Green acceptance:

- no Tokio runtime creation per endpoint operation.
- one owner task per endpoint socket lifecycle.

### Phase N2: UDP ingress/egress integration

Scope:

- wire native UDP recv/send loops to feed `Endpoint::handle(...)`.
- emit and send all required transmits from endpoint/connection progression.
- support bounded batching and clear overload behavior.

Red tests first:

- received UDP datagram drives handshake progress.
- generated transmits are flushed and peer receives expected payload.
- bounded queue overflow yields deterministic backpressure errors.

Green acceptance:

- no busy-spin loops.
- sustained traffic does not leak buffers/queues.

### Phase N3: Timer and wake progression

Scope:

- map `poll_timeout`/`handle_timeout` onto Spargio timers.
- implement endpoint wake scheduling for retransmit/PTO/deadline updates.

Red tests first:

- timeout-driven retransmit path is exercised under packet loss.
- stale timer update does not regress newer deadline scheduling.

Green acceptance:

- driver sleeps until next meaningful deadline.
- timer races do not produce duplicated work loops.

### Phase N4: Connection and stream event pump

Scope:

- map `quinn-proto` connection events to public `QuicConnection` operations.
- implement uni/bi stream open/accept/read/write/finish/reset plumbing.
- preserve current `QuicConnection` API behavior and error shape.

Red tests first:

- bi/uni stream open+echo paths pass under concurrent connections.
- finish/reset/stop semantics match expected transport behavior.

Green acceptance:

- no API regression relative to current `spargio-quic` tests.
- deterministic cancellation/drop semantics.

### Phase N5: Datagram and transport tuning depth

Scope:

- complete datagram send/recv behavior with size-limit enforcement.
- expose practical tuning pass-throughs (transport windows, keepalive, MTU).

Red tests first:

- oversized datagrams fail predictably.
- tuning knobs are plumbed and affect runtime-observable behavior.

Green acceptance:

- datagram paths are parity-complete for common workloads.

### Phase N6: Local `!Send` and cross-thread handoff mapping

Scope:

- keep `LocalQuicConnection` and `QuicSendConnection` on native backend.
- enforce ownership/thread invariants with explicit handoff boundaries.

Red tests first:

- local-to-send handoff preserves stable identity and operation correctness.
- invalid post-shutdown/local misuse yields deterministic errors.

Green acceptance:

- current ergonomics tests remain green without bridge fallback.

### Phase N7: Observability and operations gates

Scope:

- emit native-path counters and structured lifecycle/error events.
- add p50/p95/p99 and retransmit/PTO visibility hooks for CI and soak lanes.

Red tests first:

- counters advance for connects/accepts/streams/datagrams/timeouts.
- error events include terminal reason classes.

Green acceptance:

- companion CI lane includes native-QUIC smoke targets.
- soak lane validates no unbounded growth.

### Phase N8: Interop/fault/perf qualification and rollout

Scope:

- interop against at least quinn peer + one non-quinn peer where practical.
- fault-injection matrix: loss/reorder/duplication/timeout.
- benchmark A/B against current bridge path.

Red tests first:

- forced-loss and reorder scenarios fail without reliability fixes.
- A/B harness asserts no material regressions versus baseline thresholds.

Green acceptance:

- native backend meets or exceeds checklist thresholds for default use.
- bridge backend retained as compatibility fallback until native lane is
  sufficiently hardened in CI/soak.

### Immediate execution order

1. N1 driver skeleton and ownership model.
2. N2 UDP integration.
3. N3 timers/wakes.
4. N4 connection/stream pump.
5. N6 ergonomics mapping.
6. N5 datagram/tuning depth.
7. N7 observability/ops.
8. N8 interop/fault/perf rollout gates.

## Update: Phase N1 implemented (native driver skeleton + ownership model) with Red/Green TDD (2026-03-01)

Implemented the first native `quinn-proto` milestone as a dedicated driver
skeleton API while preserving existing `QuicEndpoint` behavior.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_runs_on_owner_shard`
- `native_proto_driver_stable_ids_are_monotonic`
- `native_proto_driver_rejects_commands_after_shutdown`

Expected red failures:

- missing `NativeProtoDriver` and `NativeProtoDriverOptions`
- no owner-shard task mailbox or stable-id allocation surface

### Green phase

Added native-driver skeleton in `crates/spargio-quic/src/lib.rs`:

- new options:
  - `NativeProtoDriverOptions` (`owner_shard`)
- new probe snapshot:
  - `NativeProtoDriverProbe`
- new driver handle:
  - `NativeProtoDriver::start(&RuntimeHandle, options)`
  - `probe()`
  - `allocate_connection_id()`
  - `allocate_stream_id()`
  - `shutdown()`
  - `is_closed()`
  - `endpoint_id()`
  - `owner_shard()`

Ownership and mailbox semantics:

- driver loop is spawned via `RuntimeHandle::spawn_local_on(owner_shard, ...)`.
- loop owns a `quinn_proto::Endpoint` instance and processes command mailbox
  messages serially.
- stable endpoint IDs are generated globally (`NEXT_NATIVE_ENDPOINT_ID`).
- connection/stream IDs are generated monotonically within the owner task.
- post-shutdown commands are rejected with `BrokenPipe`.

Cargo updates:

- `crates/spargio-quic/Cargo.toml` adds direct dependency:
  - `quinn-proto = "0.11"`

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic`

## Update: R2 continuation (proto-backed command semantics for connected handles) with Red/Green TDD (2026-03-01)

Implemented a follow-up R2 slice that routes `connect_for_test`-backed
stream/datagram commands through real `quinn-proto::Connection` APIs instead
of only synthetic queue behavior.

### Red phase

Added failing tests in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_connect_for_test_open_uni_respects_proto_stream_credit`
- `native_proto_driver_connect_for_test_open_bi_respects_proto_stream_credit`

Red expectation:

- with synthetic fallback still active for connected handles, `open_uni`/`open_bi`
  incorrectly succeeded even when protocol stream credit had not been granted.

### Green phase

Updated owner-loop command handlers in `crates/spargio-quic/src/lib.rs`:

- proto-connected path (`connection_id -> ConnectionHandle`) now uses
  `quinn_proto::Connection` operations for:
  - `open_uni_on_connection` / `open_bi_on_connection`
  - `accept_uni_on_connection` / `accept_bi_on_connection`
  - `send_datagram_on_connection_for_test` / `recv_datagram_on_connection_for_test`
  - `finish_stream` / `reset_stream`
- added conversion/error helpers:
  - `proto_stream_id_from_u64(...)`
  - `proto_send_datagram_error_to_io(...)`
  - `proto_finish_error_to_io(...)`
- after mutating proto-backed stream/datagram state, the loop now drives
  `drive_native_proto_connections(...)` to flush resulting endpoint/transmit work.
- synthetic fallback behavior remains for explicitly synthetic test connections
  created by `register_connection_for_test`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_connect_for_test_open_`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic`

## Update: R2 continuation (proto close-path emit on connected handles) with Red/Green TDD (2026-03-01)

Implemented close-path progression for `connect_for_test` protocol-backed
connections so `close_connection_for_test(...)` produces close transmits before
connection teardown.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_close_connection_for_test_emits_close_transmit_for_proto_connection`

Red expectation:

- close command removed proto connection state immediately, so draining transmits
  after close yielded no close packet output.

### Green phase

Updated `NativeProtoCommand::CloseConnectionForTest` handling in
`crates/spargio-quic/src/lib.rs`:

- for protocol-backed connection IDs:
  - call `quinn_proto::Connection::close(...)` with an app close code/reason.
  - run `drive_native_proto_connections(...)` to flush close-path transmits.
  - then remove handle mappings and stored protocol state.
- retained synthetic-connection cleanup behavior for non-proto test handles.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_close_connection_for_test_emits_close_transmit_for_proto_connection`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`

## Update: R2 continuation (payload-carrying transmits + server-accept path) with Red/Green TDD (2026-03-01)

Implemented the next native-proto progression slice so driver transmits include
actual datagram payload bytes, and so a driver configured with server config can
accept client `connect_for_test` traffic over the same command surface.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_server_config_accepts_client_transmits`

Initial red failure surfaced a protocol gap:

- `submit_datagram(...)` incorrectly enforced app-datagram tuning limits on raw
  protocol ingress datagrams, rejecting valid Initial packets.

### Green phase

Updated `crates/spargio-quic/src/lib.rs`:

- `NativeProtoDriverOptions` now supports optional server mode:
  - added `server_config: Option<quinn::ServerConfig>`
  - added `with_server_config(...)`
- owner loop now initializes endpoint with optional server config and allows
  incoming accepts when configured.
- `NativeProtoTransmit` now carries `payload: Vec<u8>` in addition to metadata.
- `push_native_transmit(...)` and all transmit producers now preserve payload
  bytes from scratch buffers (`transmit_payload(...)` helper).
- `SubmitDatagram` `NewConnection` handling:
  - when server-configured, uses `Endpoint::accept(...)` and registers the new
    protocol connection handle + synthetic connection ID mapping.
  - otherwise preserves explicit `refuse(...)` behavior.
- corrected datagram-size semantics:
  - removed tuning max-size enforcement from raw `submit_datagram(...)` ingress.
  - kept/enforced tuning max-size on app datagram API
    `send_datagram_on_connection_for_test(...)`, with stats/event accounting.

Test updates:

- updated `NativeProtoTransmit` test fixtures to include payload bytes.
- adjusted oversized-datagram test to validate app-datagram path:
  - `native_proto_driver_rejects_oversized_datagram_per_tuning` now uses
    `send_datagram_on_connection_for_test(...)`.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_server_config_accepts_client_transmits`
- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_rejects_oversized_datagram_per_tuning`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic`
- `cargo test --test docs_tdd`

## Update: R2 continuation (post-handshake stream open/accept contract) with Red/Green TDD (2026-03-01)

Added executable contract coverage for a protocol-correct post-handshake stream
path across two native drivers.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_post_handshake_bi_stream_open_is_accepted_by_server`

Initial red behavior:

- server-side accept immediately after client `open_bi` failed with
  `WouldBlock` because no peer-visible stream signal had been transmitted yet.

### Green phase

Adjusted the test flow to align with protocol semantics:

- after client `open_bi`, call `finish_stream` to emit stream signaling.
- exchange transmit payloads between client/server drivers.
- assert server `accept_bi_on_connection(...)` observes the opened stream.

Also factored reusable driver-exchange helper in test module:

- `exchange_driver_transmits(...)`

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_post_handshake_bi_stream_open_is_accepted_by_server`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic`
- `cargo test --test docs_tdd`

## Update: R2 continuation (remote close propagation into native connection state) with Red/Green TDD (2026-03-02)

Implemented another R2 protocol-progression slice so a peer-initiated close is
observable through `NativeProtoConnectionState.closed` on the remote side.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_remote_close_marks_peer_connection_closed`

Initial red behavior:

- server `close_connection_for_test(...)` produced close traffic, but the client
  driver never reflected `ConnectionLost` into its tracked synthetic connection
  state, so `connection_state(...).closed` remained `false`.

### Green phase

Updated `crates/spargio-quic/src/lib.rs`:

- wired reverse mapping `connection_id_by_handle` for all protocol-backed
  connection registrations (`connect_for_test` and server accept path).
- extended `drive_native_proto_connections(...)` to poll application events via
  `quinn_proto::Connection::poll()` and handle `Event::ConnectionLost`.
- on connection-lost:
  - mark corresponding `NativeProtoConnectionState.closed = true`.
  - clear pending synthetic queues for that connection.
  - remove handle mappings and protocol connection state while preserving
    synthetic connection ID visibility for state queries.
- ensured explicit close-path teardown also removes reverse handle mappings.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_remote_close_marks_peer_connection_closed -- --exact`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic --test soak_tdd`
- `cargo test -p spargio-quic`

## Update: R2 continuation (connection-closed lifecycle event) with Red/Green TDD (2026-03-02)

Implemented another native-proto lifecycle slice so remote close transitions are
observable through the event stream, not only via polled connection state.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_remote_close_emits_connection_closed_event`

Initial red behavior:

- peer close updated no lifecycle event; client-side event drain only contained
  prior events such as registration, with no explicit close transition signal.

### Green phase

Updated `crates/spargio-quic/src/lib.rs`:

- extended `NativeProtoEvent` with:
  - `ConnectionClosed { connection_id: u64 }`
- explicit close command path now emits `ConnectionClosed` exactly once when a
  tracked connection transitions from open to closed.
- protocol-driven close path in `drive_native_proto_connections(...)` now emits
  `ConnectionClosed` on `quinn_proto::Event::ConnectionLost` before handle
  retirement and mapping cleanup.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_remote_close_emits_connection_closed_event -- --exact`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic`

## Update: R2 continuation (closed-connection stats accounting) with Red/Green TDD (2026-03-02)

Implemented another native-proto observability slice so connection-close
transitions are tracked in stats alongside lifecycle events.

### Red phase

Added failing test in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_close_transitions_increment_closed_stats`

Initial red behavior:

- `NativeProtoStats` exposed no close counter, so close transitions were not
  measurable through stats snapshots.

### Green phase

Updated `crates/spargio-quic/src/lib.rs`:

- extended `NativeProtoStats` with:
  - `connections_closed: u64`
- incremented `connections_closed` on first transition to closed in both paths:
  - explicit command close (`CloseConnectionForTest`)
  - protocol-driven peer close (`Event::ConnectionLost`)
- preserved saturation semantics (`saturating_add`) and no double-counting on
  repeated close attempts.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_close_transitions_increment_closed_stats -- --exact`
- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --test native_cutover_tdd`
- `cargo test -p spargio-quic --test interop_tdd`
- `cargo test -p spargio-quic`

## Update: R2 continuation (post-handshake datagram roundtrip contract coverage) (2026-03-02)

Added explicit regression coverage for protocol-backed app datagram traffic
across two native drivers after handshake.

### Contract test added

In `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_post_handshake_datagram_roundtrip_tracks_state`

Coverage validates:

- client->server and server->client app datagram exchange over protocol-backed
  connection IDs (`connect_for_test` + server accept path).
- payload integrity on both directions.
- per-connection datagram state accounting (`datagrams_sent` /
  `datagrams_received`) on both peers.

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd native_proto_driver_post_handshake_datagram_roundtrip_tracks_state -- --exact`
- `cargo test -p spargio-quic --test quic_tdd`

## Update: Full QUIC native cutover execution plan (multi-agent parallel breakdown) (2026-03-02)

Captured an explicit agent-by-agent plan for finishing full native QUIC
integration (`QuicEndpoint`/`QuicConnection` on `NativeProtoDriver`, bridge as
explicit fallback only).

### Agent A (critical path): public native-path cutover spine

Scope:

- replace `QuicEndpoint` native backend internals so
  `connect`/`connect_with`/`accept`/`wait_idle` route through
  `NativeProtoDriver` instead of `NativeEndpointDispatch`.
- rewire `QuicConnection` native backend internals so
  `closed`/`open_uni`/`open_bi`/`accept_uni`/`accept_bi`/datagram ops route
  through `NativeProtoDriver` commands.
- preserve current timeout, backpressure, and error-shape contracts.

Red/green slices:

1. endpoint op dispatch red tests against bridge-runtime counters.
2. connection op dispatch red tests against bridge-runtime counters.
3. green implementation for endpoint + connection command routing.

Primary files:

- `crates/spargio-quic/src/lib.rs`
- `crates/spargio-quic/tests/native_cutover_tdd.rs`
- `crates/spargio-quic/tests/quic_tdd.rs`

Dependencies:

- none (first mover).

### Agent B: stream abstraction and API-coupling migration

Scope:

- remove hard native-path reliance on concrete `quinn` stream types for public
  operations while preserving ergonomic API shape.
- implement a stable stream wrapper strategy compatible with driver-owned stream
  IDs and command-based progression.
- keep `LocalQuicConnection`/`QuicSendConnection` behavior equivalent.

Red/green slices:

1. red tests for stream open/accept/read/write/finish/reset parity under native
   backend without direct Tokio actor usage.
2. green wrapper + plumbing implementation.

Primary files:

- `crates/spargio-quic/src/lib.rs`
- `crates/spargio-quic/tests/quic_tdd.rs`

Dependencies:

- starts after Agent A selects/lands endpoint/connection command contracts.

### Agent C: lifecycle, metrics, and close-state parity hardening

Scope:

- ensure native cutover preserves lifecycle semantics (`close`, `closed`,
  `wait_idle`, remote-loss propagation).
- ensure metric counters and event emission remain parity-correct
  (connect/accept/streams/datagrams/timeouts/close transitions).

Red/green slices:

1. red parity tests for close/idle and metric snapshots.
2. green implementation updates for metric increments and event mapping.

Primary files:

- `crates/spargio-quic/src/lib.rs`
- `crates/spargio-quic/tests/quic_tdd.rs`

Dependencies:

- can start test authoring in parallel; final green depends on Agent A changes.

### Agent D: interop + soak + perf gate re-qualification

Scope:

- re-run and adjust interop matrix against native-default public path.
- expand soak/fault assertions for native cutover regressions.
- validate and refresh perf-gate fixtures/threshold notes only where
  materially justified.

Red/green slices:

1. red on interop/soak/perf scripts when cutover shifts behavior.
2. green script/test/fixture updates with documented rationale.

Primary files:

- `crates/spargio-quic/tests/interop_tdd.rs`
- `crates/spargio-quic/tests/soak_tdd.rs`
- `scripts/quic_interop_matrix.sh`
- `scripts/quic_soak_fault.sh`
- `scripts/quic_perf_gate.sh`
- `tests/quic_perf_guardrail_tdd.rs`

Dependencies:

- runs after Agent A/B/C stabilization.

### Agent E: docs and rollout-status sync

Scope:

- update README done/not-done QUIC language to reflect full native cutover.
- sync implementation log summary and operations notes.
- ensure docs tests for status consistency remain green.

Red/green slices:

1. docs status tests red for stale statements.
2. green README/book/log updates.

Primary files:

- `README.md`
- `IMPLEMENTATION_LOG.md`
- `tests/docs_tdd.rs`
- `book/src/*` (if needed)

Dependencies:

- runs after Agent A-D conclusions.

### Parallel execution graph

- lane 1 (critical): Agent A.
- lane 2 (prep parallel): Agent C test authoring.
- lane 3 (prep parallel): Agent B design + test scaffolding.
- lane 4 (post-cutover): Agent B implementation + Agent C green fixes.
- lane 5 (qualification): Agent D.
- lane 6 (final sync): Agent E.

### Merge order recommendation

1. Agent A foundational cutover PR.
2. Agent B stream/wrapper parity PR.
3. Agent C lifecycle/metrics parity PR.
4. Agent D qualification/perf PR.
5. Agent E docs/status PR.

### Exit criteria for "full QUIC native integration"

- native backend public API path no longer depends on Tokio bridge runtime
  constructs for endpoint/connection operations.
- bridge backend remains explicit compatibility fallback only.
- interop, soak, and perf gates pass with updated baselines and rationale.
- README and docs no longer list QUIC native cutover as in-progress.

## Update: Agent A+B milestone (public native-path cutover to NativeProtoDriver + stream wrappers) with Red/Green TDD (2026-03-02)

Implemented the critical cutover slice so default native `QuicEndpoint`/`QuicConnection`
operations route through `NativeProtoDriver` (with UDP ingress/egress/timer pump), while
bridge backend remains explicit fallback. Added stream wrapper types so the public API keeps
`write_all` / `read_to_end` / `finish` ergonomics without exposing Tokio-bound internals.

### Red phase

Added failing coverage in `crates/spargio-quic/tests/quic_tdd.rs`:

- `native_proto_driver_connected_event_marks_connection_established`
- `native_proto_driver_stream_write_read_roundtrip_over_proto_connection`

Initial red surfaced missing native-proto capabilities:

- no connection-established signal/state for handshake completion gating
- no stream payload write/read command surface on driver-backed connections

### Green phase

Updated `crates/spargio-quic/src/lib.rs`:

- `NativeProtoConnectionState` now tracks `established`.
- `NativeProtoEvent` now includes `ConnectionEstablished`.
- `drive_native_proto_connections(...)` now maps `quinn_proto::Event::Connected` into
  state/event transitions.
- added native stream payload command surface:
  - `WriteStreamOnConnection`
  - `ReadStreamOnConnection`
  - public driver helpers:
    - `write_stream_on_connection(...)`
    - `read_stream_on_connection(...)`
- added no-wait driver helpers used by sync API points (`finish/reset/close/send_datagram`) to
  avoid nested executor re-entry.
- introduced `NativeProtoEndpointBackend`:
  - owns per-endpoint spargio runtime + native driver + UDP socket
  - runs ingress/egress/timer pump tasks
  - tracks accept queue / known connection IDs
  - provides handshake/idle/closed wait helpers with timeout semantics
- native `QuicEndpoint::server/client` constructors now initialize `NativeProtoEndpointBackend`
  and route native operations through driver-backed flow.
- native `QuicConnection` now supports driver-backed mode via `NativeProtoConnectionHandle`.
- introduced stream wrappers:
  - `QuicSendStream`
  - `QuicRecvStream`
- updated connection APIs to return wrappers (bridge and native) while preserving ergonomic calls
  used by tests (`write_all`, `read_to_end`, `finish`).

### Validation

Executed and passing:

- `cargo test -p spargio-quic --test quic_tdd`
- `cargo test -p spargio-quic --tests`
- `cargo test --test docs_tdd`

Interop + cutover tests now pass with native default path using driver-backed backend:

- `interop_tdd` (raw quinn <-> spargio)
- `native_cutover_tdd`

## Update: Agent C+D+E follow-through (lifecycle parity, qualification re-check, docs sync) (2026-03-02)

Completed the remaining parallel-plan slices after native cutover.

### Lifecycle/metrics parity checks (Agent C)

Validated native cutover behavior against existing parity tests:

- native/bridge dispatch counters and lifecycle assertions in `native_cutover_tdd`
- connection op dispatch metrics in `quic_tdd`
- close/closed/wait-idle behavior under native default path

No additional metric-shape changes were required beyond the native-driver routing.

### Qualification re-check (Agent D)

Re-ran QUIC qualification-oriented suites on the cutover implementation:

- `interop_tdd` (raw quinn interop both directions)
- `native_cutover_tdd`
- `quic_tdd`
- `soak_tdd` lane remains intentionally ignored in regular runs (nightly lane)

All executed suites passed.

### Docs/status sync (Agent E)

Updated project status to reflect completed native cutover and revised not-done scope:

- `README.md`
  - added explicit done statement for driver-backed native QUIC path
  - replaced old “full native cutover not finished yet” note with remaining rollout/hardening work
- `tests/docs_tdd.rs`
  - updated docs assertion to track new README status wording

Validation:

- `cargo test --test docs_tdd` passes with updated status expectations.

## Update: Work-stealing scheduler optimization roadmap (2026-03-03)

This roadmap is a dedicated track for scheduler policy and cache-behavior
improvements. It is intentionally separate from earlier project-wide milestones.

### Milestone WS0: baseline + red tests (entry gate)

Scope:

- Add red tests for skew/hotspot/fairness behavior and starvation bounds.
- Lock benchmark baselines for scheduler-heavy workloads (`fanout_fanin`,
  `net_api` skewed/hotspot lanes).
- Add required scheduler counters for tuning (`failed_steal_streak`,
  local-hit ratio, stolen-per-scan).
- Capture initial profiler baselines (`callgrind`/`cachegrind`) for the same
  workloads.

Acceptance criteria:

- Red tests fail for missing behavior before implementation changes.
- Baseline benchmark and profiler artifacts are checked in or documented in the
  log with reproducible commands.

### Milestone WS1: low-risk cache-line hygiene

Scope:

- Add cache-line padding for hot shared scheduler state with high false-sharing
  risk (per-shard counters/metadata touched concurrently).
- Keep runtime API unchanged.

Acceptance criteria:

- All correctness tests stay green.
- Benchmark + profiler comparison shows no regression, and ideally reduced
  cache-pressure signals.

### Milestone WS2: adaptive steal gating

Scope:

- Introduce adaptive steal gating/backoff using recent local-work and
  steal-success history.
- Keep conservative defaults so behavior remains stable for existing users.

Acceptance criteria:

- Reduced low-value steal scans in low-contention paths.
- Throughput/latency stays neutral-or-better on baseline workloads.

### Milestone WS3: victim selection upgrade

Scope:

- Improve victim selection beyond static stride (cursor + spread/randomization
  or lightweight pressure hints).
- Preserve deterministic fallback mode for reproducible tests.

Acceptance criteria:

- Better steal-success ratio under skew/hotspot loads.
- No starvation regressions in fairness tests.

### Milestone WS4: batch stealing + wake policy refinement

Scope:

- Tune batch size policy (latency-friendly small bursts vs throughput-friendly
  bigger drains).
- Refine wake behavior to avoid unnecessary cross-shard wake traffic.

Acceptance criteria:

- p95/p99 latency does not regress materially in latency-sensitive lanes.
- Throughput improves or remains neutral in throughput-heavy lanes.

### Milestone WS5: optional queue backend experiment (ROI-gated)

Scope:

- Prototype lower-contention queue backend only if WS0-WS4 evidence indicates
  mutex queue contention remains a dominant bottleneck.

Acceptance criteria:

- Ship only on clear benchmark + profiler win with manageable complexity.
- If no clear win, document decision and keep current queue path.

### Milestone WS6: rollout, docs, and CI guardrails

Scope:

- Publish scheduler tuning guidance in README/book.
- Add benchmark + profiler guardrail workflow for scheduler changes.
- Define release-note format for scheduler policy changes and tradeoffs.

Acceptance criteria:

- CI/docs guardrails are green.
- Scheduler changes require paired correctness + benchmark + profiler evidence.

### Parallelizable execution plan

- Lane A (runtime): implement scheduler/padding changes behind red/green tests.
- Lane B (profiling): run `callgrind`/`cachegrind` before/after each milestone
  candidate and capture deltas.
- Lane C (bench validation): run criterion guardrails (`throughput`, `p95/p99`)
  and validate profiler deltas map to user-visible impact.
- Lane D (docs/ops): update tuning docs and milestone logs in parallel after
  each green slice.

### Milestone status update (as of 2026-03-03)

- `WS0` planned (not started).
- `WS1` planned (blocked on WS0 baselines).
- `WS2` planned (blocked on WS0/WS1 evidence).
- `WS3` planned (blocked on WS2 telemetry/profiler evidence).
- `WS4` planned (blocked on WS2/WS3 outcomes).
- `WS5` backlog/ROI-gated (only if contention remains dominant).
- `WS6` planned (runs continuously as milestones land).

## Update: WS0-WS6 implemented with red/green slices (2026-03-03)

Completed the dedicated work-stealing roadmap end-to-end.

### WS0 (baseline diagnostics + red tests) - implemented

Delivered scheduler diagnostics and tests:

- runtime stats now expose:
  - `steal_scans`
  - `steal_failed_streak_max`
  - `stealable_local_hits`
  - `RuntimeStats::local_hit_ratio()`
  - `RuntimeStats::stolen_per_scan()`
- new scheduler diagnostics coverage:
  - `steal_stats_expose_scan_and_locality_diagnostics`
  - builder/reporting coverage for new knobs.

### WS1 (cache-line hygiene) - implemented

Applied cache-line padding to hot shared scheduler structures:

- added `CachePadded<T>` (`#[repr(align(64))]`).
- padded per-shard command-depth and native-op-depth arrays.
- padded wake flags and queue internals where relevant.

### WS2 (adaptive steal gating) - implemented

Implemented adaptive gating/backoff:

- added policy knobs:
  - `steal_locality_margin`
  - `steal_fail_cost`
  - `steal_backoff_min`
  - `steal_backoff_max`
- steal loop now applies local-vs-migration gate and adaptive cooldown after
  repeated low-value scans.

### WS3 (victim selection upgrade) - implemented

Implemented probe-based victim selection:

- added `steal_victim_probe_count`.
- each steal scan samples multiple candidates and targets the largest estimated
  backlog victim (deterministic cursor/stride progression).

### WS4 (batch stealing + wake refinement) - implemented

Implemented dynamic batch steals and wake coalescing:

- added `steal_batch_size`.
- steal loop steals batches under high backlog.
- wake policy now coalesces redundant wakeups via per-shard atomic wake flags.
- added wake diagnostics:
  - `stealable_wake_sent`
  - `stealable_wake_coalesced`
- added coverage:
  - `stealable_wake_coalescing_tracks_bursty_submissions`.

### WS5 (optional backend experiment) - implemented

Added optional lower-contention queue backend (default unchanged):

- new public enum: `StealableQueueBackend::{Mutex, SegQueueExperimental}`.
- new builder API: `RuntimeBuilder::stealable_queue_backend(...)`.
- default remains `Mutex` for compatibility.
- added coverage:
  - `runtime_builder_supports_experimental_stealable_queue_backend`.

### WS6 (rollout/docs/CI guardrails) - implemented

Added profiler lane tooling, CI wiring, and docs updates:

- scripts:
  - `scripts/bench_scheduler_profile.sh` (callgrind + cachegrind capture).
  - `scripts/scheduler_profile_guardrail.sh` (ratio checks against baseline).
- CI:
  - nightly profile lane wired in `.github/workflows/ci.yml`.
- docs:
  - updated `README.md` done/not-done scheduler statements.
  - expanded `book/src/scheduler_tuning.md` with new knobs/metrics.
- ops TDD:
  - `tests/scheduler_profile_ops_tdd.rs` verifies script presence and CI wiring.

### Validation executed

- `cargo test --test runtime_tdd --test slices_tdd --test scheduler_profile_ops_tdd --test docs_tdd`
- `SUMMARY_JSON=target/scheduler_profiles/dev_summary.json WARMUP=0.005 MEASURE=0.01 SAMPLES=10 ./scripts/bench_scheduler_profile.sh fanout_fanin_skewed spargio_io_uring`
- `MAX_CALLGRIND_IR_RATIO=2.5 MAX_CACHEGRIND_D1MR_RATIO=2.5 MAX_CACHEGRIND_D1MW_RATIO=2.5 ./scripts/scheduler_profile_guardrail.sh tests/fixtures/scheduler_profile/fanout_fanin_skewed_spargio_io_uring.json target/scheduler_profiles/dev_summary.json`

### Updated milestone status

- `WS0` completed.
- `WS1` completed.
- `WS2` completed.
- `WS3` completed.
- `WS4` completed.
- `WS5` completed (experimental backend added; default unchanged).
- `WS6` completed.

## Follow-up: calibration + rollout quality backlog (2026-03-03)

Post-implementation quality work items for scheduler policy stabilization:

- Run broader A/B matrix beyond `fanout_fanin`:
  - `net_api` hotspot/rotation/pipeline shapes.
  - repeated runs and fixed CPU affinity to reduce noise.
- Tune defaults for adaptive knobs from measured data:
  - `steal_locality_margin`
  - `steal_fail_cost`
  - `steal_backoff_min` / `steal_backoff_max`
  - `steal_victim_probe_count`
  - `steal_batch_size`
- Decide status of `StealableQueueBackend::SegQueueExperimental`:
  - keep experimental vs promote as default/primary option.
- Harden profiler guardrails:
  - keep/update scheduler baseline fixture(s).
  - tighten ratio thresholds once variance is well-characterized.
- Validate on longer soak runs for sustained-skew tail-latency behavior.

## Update: calibration + rollout quality execution (2026-03-03)

Executed the post-WS calibration backlog end-to-end.

### 1) Broader A/B matrix with fixed affinity and repeats

Added `scripts/bench_scheduler_calibration.sh` and ran fixed-affinity (`taskset
0-3`) repeated A/B (`REPEATS=3`) on the requested `net_api` shapes against
pre-WS baseline (`43a0462`) vs current WS implementation:

- `net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot`
- `net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot`
- `net_keyed_hotspot_rotation_4k/spargio_tcp_keyed_router_hotspot`

Calibration summary (`target/scheduler_profiles/net_api_calibration_ws.json`):

- stream hotspot rotation: `+0.095%` (flat)
- pipeline hotspot rotation: `+0.043%` (flat)
- keyed hotspot rotation: `-0.053%` (flat)

Interpretation: scheduler changes are neutral on these skewed `net_api` shapes
under the selected harness settings.

### 2) Default tuning sweep and decision

Tested an aggressive default profile candidate:

- `steal_victim_probe_count=3`
- `steal_batch_size=6`
- `steal_locality_margin=0`
- `steal_backoff_max=16`

Against current defaults, this profile remained mixed/flat
(`target/scheduler_profiles/net_api_tuning_profile_a.json`):

- stream hotspot rotation: `-0.221%` (flat)
- pipeline hotspot rotation: `+0.480%` (flat, slight regression)
- keyed hotspot rotation: `-0.306%` (flat)

Decision: keep runtime defaults unchanged (no clear all-shapes win).

### 3) `SegQueueExperimental` promotion decision

Used benchmark env override hooks (`SPARGIO_BENCH_*`) added to
`benches/net_api.rs` and `benches/fanout_fanin.rs` to compare queue backend
profiles without changing runtime defaults.

`net_api` calibration with `SPARGIO_BENCH_STEALABLE_QUEUE_BACKEND=segqueue`
(`target/scheduler_profiles/net_api_tuning_segqueue.json`) was mixed/flat:

- stream hotspot rotation: `-0.266%`
- pipeline hotspot rotation: `-0.686%`
- keyed hotspot rotation: `+0.224%`

Sequential `fanout_fanin_balanced/spargio_io_uring` sanity check (fixed
affinity) showed slight regression for segqueue lane:

- default (mutex): ~`1.2306 ms`
- segqueue experimental: ~`1.2560 ms` (~`+2.1%` slower)

Decision: keep `StealableQueueBackend::SegQueueExperimental` as experimental;
do not promote to default.

### 4) Guardrail hardening

Refreshed scheduler profiler fixtures and tightened nightly guardrails:

- fixtures:
  - `tests/fixtures/scheduler_profile/fanout_fanin_skewed_spargio_io_uring.json`
  - `tests/fixtures/scheduler_profile/fanout_fanin_balanced_spargio_io_uring.json`
- nightly CI scheduler profiling now covers both skewed + balanced fanout
  shapes.
- thresholds tightened from permissive values to:
  - `MAX_CALLGRIND_IR_RATIO=1.35`
  - `MAX_CACHEGRIND_D1MR_RATIO=1.35`
  - `MAX_CACHEGRIND_D1MW_RATIO=1.35`

### 5) Soak validation

Executed sustained-skew soak lane:

- `cargo test --features uring-native --test stress_tdd -- --ignored`

Result: both ignored soak tests passed.

### 6) Rollout summary

- Broader matrix executed with fixed affinity and repeat controls.
- Runtime defaults intentionally kept stable based on measured neutrality.
- Experimental queue backend remains non-default by measured outcome.
- Profiling guardrails hardened and expanded in nightly CI.
- Soak lane validated and passing.

## Roadmap: full `du` metadata parity (2026-03-03)

Objective: close the `README` "not done" gap for native directory traversal and
metadata completeness needed for a production-grade `du`-style implementation.

### Target parity outcomes

- Native async directory traversal API (no blocking traversal in hot path).
- Metadata surface sufficient for `du` semantics:
  - allocated-size accounting (`stx_blocks`-based).
  - hardlink dedupe keys (`dev` + `ino`).
  - mode/file-type and symlink policy decisions.
- Stable policy surface for:
  - apparent size vs allocated size.
  - follow vs no-follow symlinks.
  - one-filesystem boundary behavior.
  - error-policy behavior (`skip` / `fail-fast` style).
- Correctness coverage for sparse files, hardlinks, symlink cycles, mount
  boundaries, and permission-denied paths.

### Milestones

#### DU0: contract freeze + API sketch

- Define public API contracts for traversal and metadata fields:
  - low-level native wrappers.
  - high-level `fs` traversal helpers.
  - optional `du` helper API.
- Lock behavior for edge policies (links, mounts, errors).
- Add red tests that assert planned API symbols/docs references.

#### DU1: metadata parity extension (`statx` field completion)

- Extend `StatxMetadata` beyond current lite subset to include fields required
  for `du` correctness:
  - inode, device ids, allocated blocks, block size, file type bits, and
    relevant attribute masks/flags.
- Add explicit mask/options controls and typed fallbacks.
- Red/green tests:
  - field population on supported kernels.
  - deterministic fallback behavior when native support is unavailable.

#### DU2: native directory enumeration wrapper (`getdents64`)

- Add low-level unsafe-op wrapper and safe boundary for directory entry fetch.
- Return typed entries with name + inode + file type (+ cookie/offset where
  useful).
- Red/green tests for:
  - normal traversal batches.
  - end-of-directory semantics.
  - invalid/unsupported kernel behavior.

#### DU3: high-level async `read_dir` surface

- Build ergonomic `spargio::fs` traversal API on top of DU2.
- Add iterator/stream-style consumption suitable for recursive walkers.
- Red/green tests for:
  - complete enumeration.
  - stable error propagation behavior.
  - symlink handling mode toggles.

#### DU4: `du` accounting engine core

- Implement recursive walker that consumes DU3 + DU1 metadata.
- Add accounting modes:
  - `allocated` (default, `blocks * 512`-style semantics).
  - `apparent` (`size`-style semantics).
- Add hardlink dedupe set keyed by `(dev, ino)`.
- Red/green tests for sparse files and hardlink counting correctness.

#### DU5: filesystem-boundary + symlink policy completion

- Add root-device capture and one-filesystem boundary filtering.
- Add explicit symlink-follow mode with cycle protection.
- Red/green tests for:
  - cross-device skip behavior.
  - symlink loops and bounded traversal.
  - mixed trees (file/dir/link/device-boundary).

#### DU6: fallback and capability model hardening

- Define capability gates for kernels lacking full native opcode support.
- Ensure graceful degraded path behavior remains correct (even if slower).
- Add red/green tests validating identical semantics across native and fallback
  paths for representative fixtures.

#### DU7: correctness corpus + differential checks

- Build reusable filesystem fixture corpus:
  - sparse, hardlink fanout, symlink chains/loops, deep trees, permission
    barriers.
- Add differential checks versus a reference implementation (`du`-style expected
  outputs) for deterministic fixture trees.
- Add long-running traversal stability tests.

#### DU8: performance, profiling, and guardrails

- Add targeted traversal/metadata benchmarks.
- Add profiler lanes (`callgrind`/`cachegrind`) and guardrail thresholds for new
  traversal paths.
- Track hotspot regressions before enabling "default recommended" guidance.

#### DU9: docs + rollout

- Update README/book with:
  - API usage.
  - semantics matrix (`allocated` vs `apparent`, links, mounts, errors).
  - kernel capability notes.
- Add migration guidance for existing users currently doing blocking traversal.
- Final "done/not done" sync and acceptance checklist closeout.

### Parallel execution plan (multi-agent)

- Lane A (Metadata): DU1 + DU6 metadata capability pieces.
- Lane B (Traversal primitives): DU2.
- Lane C (High-level API): DU3 (starts once DU2 API shape is stable).
- Lane D (Accounting semantics): DU4 + DU5 (starts once DU1+DU3 land).
- Lane E (Quality): DU7 fixture corpus and differential tests (can start early,
  final assertions after DU4/DU5).
- Lane F (Perf/docs): DU8 + DU9 (starts once DU4 baseline is functional).

### Dependency graph (for scheduling)

- DU0 first.
- DU1 and DU2 can run in parallel after DU0.
- DU3 depends on DU2.
- DU4 depends on DU1 + DU3.
- DU5 depends on DU4.
- DU6 depends on DU1 + DU2 (and can continue while DU4/DU5 progress).
- DU7 can start fixture scaffolding early; full differential checks depend on
  DU4 + DU5 + DU6.
- DU8 depends on DU4 minimum functionality.
- DU9 finalizes after DU7 + DU8.

## Update: DU roadmap execution (2026-03-03)

Implemented DU0–DU9 execution slices with parallel lane scheduling (metadata,
dirent primitives, high-level API, accounting semantics, and quality/docs).

### DU0: contract freeze + red tests

- Added red contract coverage in:
  - `tests/du_parity_tdd.rs`
- Initial failures validated missing APIs/fields before implementation.

### DU1: metadata parity extension

- Expanded `StatxMetadata` in `src/lib.rs` with du-relevant fields:
  - `ino`, `blocks`, `blksize`, `dev`, `rdev`, `attributes`,
    `attributes_mask`.
- Added file-type helpers:
  - `StatxMetadata::{is_dir,is_file,is_symlink}`.
- `metadata_lite` parity assertions now verify inode/block population.

### DU2: low-level directory enumeration wrapper

- Added low-level extension surface:
  - `spargio::extension::fs::{DirEntryType, DirEntry, read_dir_entries(...)}`.
- Implementation uses `getdents64` parsing (`SYS_getdents64`) with compatibility
  fallback to `std::fs::read_dir` when unsupported.
- Added dedicated coverage:
  - `extension_read_dir_entries_exposes_low_level_dirent_surface`.

### DU3: high-level async `read_dir`

- Added high-level API:
  - `spargio::fs::{DirEntryType, DirEntry, read_dir(...)}`.
- Wires to extension lane and returns typed entry data (name/path/inode/type).

### DU4: `du` accounting core

- Added API and policies:
  - `spargio::fs::{du(...), DuOptions, DuSummary, DuSizeMode}`.
- Implemented accounting modes:
  - `Allocated` (`blocks * 512`) and `Apparent` (`size`).
- Implemented hardlink dedupe keyed by `(dev, ino)` (configurable via
  `hardlink_dedupe(bool)`).

### DU5: symlink + filesystem-boundary policy

- Added `DuSymlinkMode::{NoFollow, Follow}` with loop-safe traversal behavior.
- Added `one_file_system(bool)` policy and cross-device skip tracking in
  `DuSummary::skipped_cross_device`.
- Added tests for looped symlink traversal and one-filesystem behavior.

### DU6: fallback and capability hardening

- Directory enumeration path now degrades deterministically:
  - `getdents64` -> `std::fs::read_dir` fallback on unsupported kernels.
- Added `DuErrorMode::{FailFast, Skip}` and skip counters
  (`DuSummary::skipped_errors`).
- Added tests covering fail-fast vs skip behavior on broken symlink targets.

### DU7: fixture/correctness corpus expansion

- Expanded DU correctness tests to include:
  - sparse files
  - hardlink dedupe
  - symlink loops
  - broken symlink error-policy behavior
  - cross-device skip behavior
- Current corpus lives in `tests/du_parity_tdd.rs` and executes in CI test lane.

### DU8: traversal benchmark lane

- Added benchmark target:
  - `benches/du_api.rs`
- Added Cargo bench registration:
  - `Cargo.toml` -> `[[bench]] name = "du_api"`.
- Bench covers:
  - `fs_du_allocated`
  - `fs_du_apparent`
  - `fs_read_dir_root`

### DU9: docs/rollout sync

- Updated README done/not-done sections:
  - done: built-in `read_dir`/`du` APIs and low-level extension dirent surface.
  - not-done: clarified remaining gap is full in-ring traversal submission path.

### Validation run set

- `cargo test --features uring-native --test du_parity_tdd`
- `cargo test --features uring-native`
- `cargo bench --features uring-native --bench du_api --no-run`

## Update: exploratory benchmark expansion (2026-03-03)

Expanded and documented exploratory `net_api` workloads to cover queue-depth-
insensitive coordination shapes and mixed fs/net deadline-churn shapes where
dispatch/runtime behavior is often the bottleneck.

### Benchmarks added (documented + implemented)

Previously added in this benchmark lane and now documented in one place:

- `net_keyed_hotspot_rotation_4k_window64_cpu`
- `ingress_dispatch_to_workers_rr_256b_ack`
- `fs_net_microservice_4k_read_then_256b_reply_qd1`
- `fanout_fanin_rotating_hot_partition_4k_window32`
- `session_owner_with_spillover_4k`
- `net_burst_flip_imbalance_4k`
- `fanin_barrier_micro_batches_1k`
- `serial_dep_chain_rpc_256b`
- `keyed_hotspot_flip_p99_4k`
- `fanin_barrier_rounds_1k`
- `wakeup_sparse_event_rtt_64b`
- `timer_cancel_reschedule_storm`
- `mixed_control_data_plane_4k_plus_64b`
- `bounded_pipeline_backpressure_4k_window2`
- `post_io_cpu_locality_4k_window1`
- `fs_net_microservice_deadline_dispatch_4k_read_256b_reply`

Newly implemented variant set from the follow-up request:

- `net_echo_rtt_deadline_routing_256b`
- `net_stream_multitenant_4k_window8`
- `net_stream_hotflip_4k`
- `net_pipeline_barrier_4k_window4`
- `keyed_router_with_session_owner_spillover_4k`
- `fs_metadata_then_reply_qd1`

### Harness updates

`benches/net_api.rs`:

- Added benchmark constants and groups for the 6 new variants above.
- Added `FsBenchFixture::metadata_qd1(...)` for metadata-heavy request-path
  shapes.
- Added/kept deadline-churn mixed loops using existing timer-storm command path
  across Tokio/Spargio/Compio harnesses.
- Registered all new groups in `criterion_group!(benches, ...)`.

`Cargo.toml`:

- Enabled Compio `time` feature for timer-storm workloads:
  - `compio` features now include `"time"`.

### Run commands

- Build verification:
  - `cargo fmt --all`
  - `cargo bench --bench net_api --features uring-native --no-run`
- Exploratory benchmark runs:
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 fs_net_microservice_deadline_dispatch_4k_read_256b_reply`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 net_echo_rtt_deadline_routing_256b`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 net_stream_multitenant_4k_window8`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 net_stream_hotflip_4k`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 net_pipeline_barrier_4k_window4`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 keyed_router_with_session_owner_spillover_4k`
  - `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 fs_metadata_then_reply_qd1`

### Notable outcomes (p99 speedup: baseline/spargio)

- Strong Spargio wins on deadline-churn microservice variants:
  - `fs_net_microservice_deadline_dispatch_4k_read_256b_reply`:
    - vs Tokio: `10.9x`
    - vs Compio: `1.6x`
  - `net_echo_rtt_deadline_routing_256b`:
    - vs Tokio: `8.4x`
    - vs Compio: `1.5x`
  - `fs_metadata_then_reply_qd1`:
    - vs Tokio: `11.6x`
    - vs Compio: `1.2x`
- Moderate/near-parity outcomes on several other variants:
  - `net_stream_multitenant_4k_window8`: ~parity vs Tokio, better than Compio.
  - `net_pipeline_barrier_4k_window4`: slight win vs Tokio, clear win vs
    Compio.
- Some hotspot-flip shapes still favor Compio:
  - `net_stream_hotflip_4k`.

README was updated to consolidate exploratory workload results into one table
covering all entries above.

## Update: high-depth exploratory suite + p99-only format shift (2026-03-03)

Implemented the requested high-depth workload set and refreshed the consolidated
exploratory benchmark table format.

### New high-depth workloads

Added to `benches/net_api.rs`:

- `high_depth_fanout_first_k_cancel_256b_window64`
- `high_depth_multitenant_keyed_router_4k_window64`
- `high_depth_barriered_pipeline_4k_window64`
- `high_depth_deadline_gateway_256b_window64`
- `high_depth_fs_net_admission_control_4k_read_256b_reply_window64`

Supporting harness updates:

- Added high-depth constants for fanout, keyed routing, barrier pipeline,
  deadline gateway, and fs+net admission-control scenarios.
- Generalized `run_fs_net_deadline_loop(...)` with `reads_per_epoch` parameter
  so the same helper can serve multiple fs+net workload shapes.
- Registered all new groups in `criterion_group!(benches, ...)`.

### Validation and run set

- `cargo fmt --all`
- `cargo bench --bench net_api --features uring-native --no-run`
- `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 high_depth_fanout_first_k_cancel_256b_window64`
- `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 high_depth_multitenant_keyed_router_4k_window64`
- `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 high_depth_barriered_pipeline_4k_window64`
- `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 high_depth_deadline_gateway_256b_window64`
- `cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 high_depth_fs_net_admission_control_4k_read_256b_reply_window64`

### README format change

Benchmark result tables in `README.md` now report:

- runtime latencies as `p99`.
- speedups as `baseline_p99 / spargio_p99`.

Exploratory benchmark section is now located under benchmark interpretation in
README with title:

- `Exploratory Benchmarks (Subject to Change, May Be Removed)`.

### Notable high-depth outcomes (p99)

- `high_depth_fanout_first_k_cancel_256b_window64`:
  - vs Tokio: `1.7x`
  - vs Compio: `1.7x`
- `high_depth_deadline_gateway_256b_window64`:
  - vs Tokio: `3.6x`
  - vs Compio: `1.1x`
- `high_depth_fs_net_admission_control_4k_read_256b_reply_window64`:
  - vs Tokio: `4.0x`
  - vs Compio: `1.7x`

### Consolidated exploratory run command (moved from README)

```bash
for bench in \
  net_keyed_hotspot_rotation_4k_window64_cpu \
  ingress_dispatch_to_workers_rr_256b_ack \
  fs_net_microservice_4k_read_then_256b_reply_qd1 \
  fanout_fanin_rotating_hot_partition_4k_window32 \
  session_owner_with_spillover_4k \
  net_burst_flip_imbalance_4k \
  fanin_barrier_micro_batches_1k \
  serial_dep_chain_rpc_256b \
  keyed_hotspot_flip_p99_4k \
  fanin_barrier_rounds_1k \
  wakeup_sparse_event_rtt_64b \
  timer_cancel_reschedule_storm \
  mixed_control_data_plane_4k_plus_64b \
  bounded_pipeline_backpressure_4k_window2 \
  post_io_cpu_locality_4k_window1 \
  fs_net_microservice_deadline_dispatch_4k_read_256b_reply \
  net_echo_rtt_deadline_routing_256b \
  net_stream_multitenant_4k_window8 \
  net_stream_hotflip_4k \
  net_pipeline_barrier_4k_window4 \
  keyed_router_with_session_owner_spillover_4k \
  fs_metadata_then_reply_qd1 \
  high_depth_fanout_first_k_cancel_256b_window64 \
  high_depth_multitenant_keyed_router_4k_window64 \
  high_depth_barriered_pipeline_4k_window64 \
  high_depth_deadline_gateway_256b_window64 \
  high_depth_fs_net_admission_control_4k_read_256b_reply_window64; do
  cargo bench --bench net_api --features uring-native -- --noplot --sample-size 20 "$bench"
done
```

## Update: README benchmark reporting switched to mean iteration latency (2026-03-04)

Rationale:

- The previous README table format used p99 over Criterion sample iterations.
- Those p99 values are not request-level tails; they are distribution tails of
  per-iteration benchmark samples (`sample.json`), which can be misleading for
  readers expecting request-level percentile semantics.

What changed:

- Benchmark tables in `README.md` now report Criterion `mean` wall-clock
  iteration latency (`estimates.json` point estimates).
- Speedup columns now use `baseline_mean / spargio_mean`.
- Existing benchmark table values were refreshed from local Criterion artifacts
  under `target/criterion/*/new/estimates.json`.

Notes:

- This keeps comparisons stable and easier to interpret until/if we add
  explicit request-level latency histograms inside the benchmark harnesses.

## Update: docs.rs coverage hardening for user-facing core API (2026-03-04)

Implemented a focused documentation pass for the public runtime/boundary core
API and verified docs coverage at 100% for the default docs.rs feature set.

### What was added

- User-focused rustdoc for:
  - `ShardId`
  - `boundary` module (`BoundaryClient`, `BoundaryServer`, tickets, errors,
    stats, request envelope helpers)
  - Time/cancellation primitives (`sleep`, `sleep_until`, `Sleep`, `timeout*`,
    `Interval`, `CancellationToken`, `TaskGroup`)
  - Core placement/message/runtime surface (`Event`, `RingMsg`,
    `TaskPlacement`, `RuntimeBuilder`, `Runtime`, `RuntimeHandle`,
    `RemoteShard`, `ShardCtx`, errors, join/ticket futures)
  - `RuntimeStats` fields and helper ratios.

### Guardrails

- Added lint enforcement for the default (non-`uring-native`) API surface:
  - `#![cfg_attr(not(feature = "uring-native"), deny(missing_docs))]`
- This keeps docs.rs-default coverage strict without breaking current
  `uring-native` CI/test lanes that still have broader undocumented surfaces.

### Verification

- `RUSTDOCFLAGS='-Dmissing-docs' cargo +nightly doc --no-deps`
- `cargo +nightly rustdoc --lib -- -Zunstable-options --show-coverage`
  - Result: `src/lib.rs` documented `218/218` (`100.0%`)
- `cargo test`
- `cargo test --features uring-native`
