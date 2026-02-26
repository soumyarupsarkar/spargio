# spargio

`spargio` is an experimental Rust async runtime built around `io_uring` and `msg_ring` for cross-shard coordination and work stealing.

Built with Codex.

## Benchmark Results

| Benchmark | Tokio | spargio queue | spargio io_uring | Readout |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | `1.30-1.42 ms` | `1.50-1.53 ms` | `352-370 us` | `spargio io_uring ~3.8x faster than tokio` |
| `steady_one_way_send_drain` (unbatched) | `84.0-90.6 us` | `1.33-1.38 ms` | `66.6-68.3 us` | `spargio io_uring ~1.3x faster than tokio` |
| `steady_one_way_send_drain` (Tokio batched) | `14.4-26.1 us` | n/a | n/a | Tokio batched wins |
| `cold_start_ping_pong` | `500-555 us` | `2.42-2.45 ms` | `248-305 us` | `spargio io_uring ~1.9x faster than tokio` |
| `fanout_fanin_balanced` | `1.43-1.51 ms` | `30.3-45.4 ms` | `982-989 us` | `spargio io_uring ~1.5x faster than tokio` |
| `fanout_fanin_skewed` | `2.35-2.42 ms` | `54.0-54.9 ms` | `1.92-1.93 ms` | `spargio io_uring ~1.24x faster than tokio` |
| `disk_read_rtt_4k` | `1.82-2.00 ms` | n/a | `2.52-2.74 ms` | `spargio io_uring ~1.38x slower than tokio` |

Bench suites in this repo:

- `benches/ping_pong.rs`
- `benches/fanout_fanin.rs`
- `benches/disk_io.rs`

## Why It's Faster Than Tokio (In These Benchmarks)

- Cross-shard wakeups use `IORING_OP_MSG_RING`, which keeps coordination on the ring path instead of relying on heavier cross-thread signaling paths.
- Stealable tasks are enqueued per shard, reducing contention compared to a single global hot queue.
- Submission-time target selection places work on less-loaded shard inboxes.
- The `io_uring` backend keeps control-path operations and completion handling tightly coupled to shard-local loops.

This combination is strongest on coordination-heavy workloads (fan-out/fan-in, frequent cross-shard wakeups, many short tasks).

## Current Runtime Surface

- Sharded runtime with `Queue` and Linux `IoUring` backends.
- Cross-shard typed/raw messaging, nowait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Per-shard stealable inboxes with `msg_ring` wake path.
- Native lane MVP (`uring-native`): `read_at`, `write_at`.
- Bounded mixed-runtime boundary API: `spargio::boundary`.
- Runtime stats snapshots via `RuntimeHandle::stats_snapshot()`.
- Reference mixed-mode service example.

## Next Steps

- Move from inbox-based stealing to fuller deque-style stealing policy with fairness/victim heuristics.
- Strengthen regression gates around fan-out/fan-in and add p95/p99 tracking.
- Expand native I/O coverage (disk and network paths).
- Keep tuning placement and wake strategies for coordination-heavy workloads.

## Quick Start

```bash
cargo test
cargo test --features uring-native
cargo bench --no-run
```

Benchmark helpers:

```bash
./scripts/bench_fanout_smoke.sh
./scripts/bench_fanout_guardrail.sh
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
