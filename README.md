# spargio

`spargio` is a work-stealing async runtime for Rust built around `io_uring`.

Traditionally, runtimes using `io_uring` (glommio/monoio/compio) have a thread-per-core model. `spargio` doesn't. We use `msg_ring` for cross-shard coordination and submission-time task placement. Interestingly this gives us more "work stealing" opportunities than tokio's epoll-based model.

Unlike Tokio’s default runtime model, Spargio can steer native disk and network I/O operations to another shard before submission, placing SQEs on a chosen `io_uring` queue. This adds a pre-submission dispatch lever (in addition to task stealing), which can reduce queue imbalance and tail latency in coordination-heavy or bursty workloads.

The benchmark results below show where this helps in practice.

## Disclaimer

This is a proof of concept built with Codex to see if the idea is worth pursuing; I have not reviewed all the code yet. Do not use in production.

## Benchmark Results

| Benchmark | Description | Tokio | spargio | Speedup |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | Two-worker request/ack round-trip loop | `1.434-1.553 ms` | `366-381 us` | `4.0x` |
| `steady_one_way_send_drain` | One-way sends, then explicit drain barrier | `68.8-76.2 us` | `70.5-71.7 us` | `1.0x` |
| `cold_start_ping_pong` | Includes runtime/harness startup and teardown | `561-610 us` | `249-267 us` | `2.3x` |
| `fanout_fanin_balanced` | Even fanout/fanin across shards | `1.473-1.621 ms` | `1.387-1.404 ms` | `1.1x` |
| `fanout_fanin_skewed` | Skewed fanout/fanin with hotspot pressure | `2.366-2.437 ms` | `1.993-2.003 ms` | `1.2x` |
| `fs_read_rtt_4k` (`qd=1`) | 4 KiB file read latency, depth 1 | `1.754-1.867 ms` | `1.003-1.028 ms` | `1.8x` |
| `fs_read_throughput_4k_qd32` | 4 KiB file reads, queue depth 32 | `8.732-9.015 ms` | `6.085-6.866 ms` | `1.4x` |
| `net_echo_rtt_256b` (`qd=1`) | 256-byte TCP echo latency, depth 1 | `7.918-8.187 ms` | `5.539-5.812 ms` | `1.4x` |
| `net_stream_throughput_4k_window32` | 4 KiB stream throughput, window 32 | `10.544-10.656 ms` | `10.996-11.408 ms` | `0.9x` |

Spargio leads most clearly in coordination-heavy and latency-sensitive paths, while some pure throughput cases (for example `steady_one_way_send_drain` and `net_stream_throughput_4k_window32`) are currently near parity.

For CPU/coordination-heavy suites, this `spargio` value is the same runtime message path regardless of bound/unbound native-I/O mode. For native-I/O suites, this column reflects the unbound `UringNativeAny` path.

Bench suites in this repo:

- `benches/ping_pong.rs`
- `benches/fanout_fanin.rs`
- `benches/fs_api.rs`
- `benches/net_api.rs`

## Why Spargio is faster

- `spargio` is built around `io_uring` + `msg_ring`, so cross-shard signaling and completion flow stay on ring paths instead of readiness/event-loop wakeup paths.
- Cross-shard coordination uses `IORING_OP_MSG_RING`, which provides low-latency doorbells and direct ring-to-ring messaging.
- Shard-local runtime loops plus stealable compute tasks reduce coordination contention under fanout/fanin pressure.
- Native APIs include reusable/persistent paths (`send_all_batch`, `recv_multishot_segments`, `read_at_into`, file sessions), which reduce allocation churn and per-op control overhead.
- It performs particularly well in coordination-heavy scenarios (fanout/fanin, frequent cross-shard wakeups, and short control-path-dominated operations).

These mechanisms are where Spargio’s measured wins come from in the benchmark suite.

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
  - file-style ops: `read_at`, `read_at_into`, `write_at`, `fsync`
  - stream/socket ops: `recv`, `send`, `recv_into`, `send_batch`, `send_all_batch`, `recv_batch_into`, `recv_multishot`, `recv_multishot_segments`
  - bound-FD wrapper: `UringBoundFd` with `bind_file`, `bind_tcp_stream`, `bind_udp_socket`, `bind_owned_fd`
- Unbound native lane API:
  - `RuntimeHandle::uring_native_unbound() -> UringNativeAny`
  - submission-time shard selection via `NativeLaneSelector` (pending-native depth + round-robin tie-break)
  - FD affinity lease table (`weak` file, `strong` stream, `hard` multishot) and active `op_id -> shard` route tracking
  - direct native command-envelope submission path (`SubmitNativeAny`) with same-shard local fast path
- Persistent file session API:
  - `UringBoundFd::start_file_session()`
  - `UringFileSession::{read_at, read_at_into, shutdown}`
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

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE).

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `spargio` by you shall be licensed as MIT, without any
additional terms or conditions.
