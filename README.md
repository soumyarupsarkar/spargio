# spargio

`spargio` is an experimental Rust async runtime built around `io_uring` and `msg_ring` for cross-shard coordination and work stealing.

Built with Codex.

## Benchmark Results

| Benchmark | Tokio | spargio queue | spargio io_uring | Readout |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | `1.37-1.48 ms` | `1.36-1.39 ms` | `363-380 us` | `spargio io_uring ~3.8x faster than tokio` |
| `steady_one_way_send_drain` (unbatched) | `104.6-115.8 us` | `1.31-1.36 ms` | `73.0-75.3 us` | `spargio io_uring ~1.5x faster than tokio` |
| `steady_one_way_send_drain` (Tokio batched) | `13.9-25.6 us` | n/a | n/a | Tokio batched wins |
| `cold_start_ping_pong` | `462.8-510.6 us` | `2.44-2.46 ms` | `260-297 us` | `spargio io_uring ~1.75x faster than tokio` |
| `fanout_fanin_balanced` | `1.42-1.50 ms` | `6.91-12.57 ms` | `1.33-1.35 ms` | `spargio io_uring ~1.1x faster than tokio` |
| `fanout_fanin_skewed` | `2.42-2.53 ms` | `28.66-32.67 ms` | `2.03-2.04 ms` | `spargio io_uring ~1.2x faster than tokio` |
| `fs_read_rtt_4k` (`qd=1`) | `1.63-1.72 ms` | n/a | `1.89-2.06 ms` | `spargio io_uring ~1.2x slower than tokio` |
| `fs_read_throughput_4k_qd32` | `7.49-7.62 ms` | n/a | `6.16-6.61 ms` | `spargio io_uring ~1.2x faster than tokio` |
| `net_echo_rtt_256b` (`qd=1`) | `7.58-7.90 ms` | n/a | `5.25-5.35 ms` | `spargio io_uring ~1.5x faster than tokio` |
| `net_stream_throughput_4k_window32` | `10.51-10.85 ms` | n/a | `10.84-10.95 ms` | `near parity; spargio io_uring slightly slower` |

Bench suites in this repo:

- `benches/ping_pong.rs`
- `benches/fanout_fanin.rs`
- `benches/fs_api.rs`
- `benches/net_api.rs`

## Why It's Faster Than Tokio (In These Benchmarks)

- Cross-shard wakeups use `IORING_OP_MSG_RING`, which keeps coordination on the ring path instead of relying on heavier cross-thread signaling paths.
- Stealable tasks are enqueued per shard, reducing contention compared to a single global hot queue.
- Submission-time target selection places work on less-loaded shard inboxes.
- The `io_uring` backend keeps control-path operations and completion handling tightly coupled to shard-local loops.

This combination is strongest on coordination-heavy workloads (fan-out/fan-in, frequent cross-shard wakeups, many short tasks).

## Done

- Sharded runtime with `Queue` and Linux `IoUring` backends.
- Cross-shard typed/raw messaging, nowait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Scheduler v2 MVP:
  - per-shard stealable deques
  - bounded steal budget (`steal_budget`)
  - stealable queue backpressure (`stealable_queue_capacity`, `RuntimeError::Overloaded`)
  - steal/backpressure stats (`steal_attempts`, `steal_success`, `stealable_backpressure`)
- Runtime primitives:
  - `sleep(Duration)`
  - `timeout(Duration, fut) -> Result<_, TimeoutError>`
  - `CancellationToken`
  - `TaskGroup` cooperative cancellation
- Native lane (`uring-native`) APIs:
  - file-style ops: `read_at`, `write_at`, `fsync`
  - stream/socket ops: `recv`, `send`, `recv_into`, `send_batch`, `send_all_batch`, `recv_batch_into`, `recv_multishot`, `recv_multishot_segments`
  - bound-FD wrapper: `UringBoundFd` with `bind_file`, `bind_tcp_stream`, `bind_udp_socket`, `bind_owned_fd`
- io_uring tuning preset:
  - `RuntimeBuilder::io_uring_throughput_mode(...)` (coop-taskrun + optional sqpoll) for throughput-oriented configurations.
- Bounded mixed-runtime boundary API: `spargio::boundary`.
- Runtime stats snapshots via `RuntimeHandle::stats_snapshot()`.
- KPI/perf automation:
  - fanout guardrail
  - ping guardrail
  - combined KPI guardrail script
- Reference mixed-mode service example.

## Not Done Yet

- Full production-grade work-stealing policy:
  - richer fairness and starvation controls
  - stronger adaptive victim-selection heuristics
- Tail-latency-focused perf program:
  - longer benchmark windows
  - p95/p99 regression gates
- Broader native I/O surface:
  - fuller filesystem API
  - fuller network API (beyond current send/recv MVP)
- Production hardening:
  - deeper stress/soak and failure-injection coverage
  - more operational observability and tracing
- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) as a full ecosystem lane.

## Quick Start

```bash
cargo test
cargo test --features uring-native
cargo bench --no-run
```

Benchmark helpers:

```bash
./scripts/bench_fanout_smoke.sh
./scripts/bench_ping_guardrail.sh
./scripts/bench_fanout_guardrail.sh
./scripts/bench_kpi_guardrail.sh
```

Reference app:

```bash
cargo run --example mixed_mode_service
```

## Repository Map

- `src/lib.rs`: runtime implementation.
- `tests/`: TDD coverage.
- `benches/`: Criterion benchmarks.
- `examples/`: mixed-mode reference app.
- `scripts/`: benchmark smoke/guard helpers.
- `.github/workflows/`: CI gates.
- `IMPLEMENTATION_LOG.md`: implementation and benchmark log.
- `architecture_decision_records/`: ADRs.

## Tokio Integration

Recommended integration model today:

- Run Tokio and spargio side-by-side.
- Exchange work/results through explicit boundaries (channels, request/response adapters, `spargio::boundary`).
- Keep existing Tokio ecosystem dependencies unchanged while moving selected hot paths into spargio.

Alternative path:

- A Tokio-compat readiness shim based on `IORING_OP_POLL_ADD` is possible, including work-stealing-aware scheduling behind that shim.
- Building that into a broad, dependency-transparent compatibility layer is a large investment.

## Inspirations

- `ourio`: <https://github.com/rockorager/ourio>
- `tokio-uring`: <https://github.com/tokio-rs/tokio-uring>

Related runtimes (`glommio`, `monoio`, `compio`) are likely faster for some thread-per-core-only workloads because that is their primary design center.

## Engineering Method

Development style is red/green TDD:

1. Add failing tests.
2. Implement minimal passing behavior.
3. Validate with full test and benchmark checks.
