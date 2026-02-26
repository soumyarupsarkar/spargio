# spargio

`spargio` is an experimental Rust async runtime built around `io_uring` and `msg_ring` for cross-shard coordination and work stealing.

Built with Codex.

## Benchmark Results

| Benchmark | Tokio | spargio io_uring | Readout |
| --- | --- | --- | --- |
| `steady_ping_pong_rtt` | `1.434-1.553 ms` | `366-381 us` | `spargio io_uring ~4.0x faster than tokio` |
| `steady_one_way_send_drain` | `68.8-76.2 us` | `70.5-71.7 us` | `near parity (tokio slightly faster in this run)` |
| `cold_start_ping_pong` | `561-610 us` | `249-267 us` | `spargio io_uring ~2.3x faster than tokio` |
| `fanout_fanin_balanced` | `1.473-1.621 ms` | `1.387-1.404 ms` | `spargio io_uring ~1.1x faster than tokio` |
| `fanout_fanin_skewed` | `2.366-2.437 ms` | `1.993-2.003 ms` | `spargio io_uring ~1.2x faster than tokio` |
| `fs_read_rtt_4k` (`qd=1`) | `1.648-1.714 ms` | `1.494-1.805 ms` | `near parity (high sensitivity at qd=1)` |
| `fs_read_throughput_4k_qd32` | `8.603-9.196 ms` | `5.806-6.681 ms` | `spargio io_uring ~1.4x faster than tokio` |
| `net_echo_rtt_256b` (`qd=1`) | `8.044-8.518 ms` | `5.375-5.806 ms` | `spargio io_uring ~1.5x faster than tokio` |
| `net_stream_throughput_4k_window32` | `10.470-10.946 ms` | `10.762-10.901 ms` | `near parity` |

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

- Unbound submission-time lane steering for all native ops:
  - `UringNativeAny`-style API for stealable tasks
  - lane selector + FD affinity leases + cancellation routing
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
