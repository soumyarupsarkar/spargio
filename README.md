# spargio

`spargio` is an experimental Rust async runtime focused on `io_uring` + `msg_ring` cross-shard coordination.

Current aim: prove that coordination-heavy async workloads can run faster when work placement and cross-thread wakeups are built around `msg_ring`.

Built with Codex.

## Latest Empirical Results (2026-02-26)

Profile used for these quick local runs:

`--warm-up-time 0.05 --measurement-time 0.05 --sample-size 20`

| Benchmark | Tokio | spargio queue | spargio io_uring | Midpoint readout |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | `1.30-1.42 ms` | `1.50-1.53 ms` | `352-370 us` | `spargio io_uring ~3.8x faster than tokio` |
| `steady_one_way_send_drain` (unbatched) | `84.0-90.6 us` | `1.33-1.38 ms` | `66.6-68.3 us` | `spargio io_uring ~1.3x faster than tokio` |
| `steady_one_way_send_drain` (Tokio batched) | `14.4-26.1 us` | n/a | n/a | Tokio batched still wins |
| `cold_start_ping_pong` | `500-555 us` | `2.42-2.45 ms` | `248-305 us` | `spargio io_uring ~1.9x faster than tokio` |
| `fanout_fanin_balanced` | `1.43-1.51 ms` | `30.3-45.4 ms` | `982-989 us` | `spargio io_uring ~1.5x faster than tokio` |
| `fanout_fanin_skewed` | `2.35-2.42 ms` | `54.0-54.9 ms` | `1.92-1.93 ms` | `spargio io_uring ~1.24x faster than tokio` |
| `disk_read_rtt_4k` | `1.82-2.00 ms` | n/a | `2.52-2.74 ms` | `spargio io_uring ~1.38x slower than tokio` |

Why this is meaningful:

- The project value proposition is showing up in the fan-out/fan-in benchmarks, where work distribution and cross-shard coordination dominate.
- Earlier on February 26, 2026, `fanout_fanin_balanced` was a loss for `spargio io_uring` (~`1.61-1.65 ms` vs Tokio ~`1.35-1.38 ms`). It is now a win (~`0.98-0.99 ms` vs ~`1.43-1.51 ms`).

These are short-run numbers; use longer measurement windows for release-quality claims.

## Why It Is Faster In These Cases

Recent scheduler/runtime changes:

- Replaced global stealable injection with per-shard stealable inboxes.
- Added submission-time target selection based on inbox depth.
- Added io_uring `msg_ring` wakeups for stealable dispatch, so cross-shard nudges avoid the command-queue wake path in shard-context submits.

Net effect:

- Less shared-queue contention and fewer cacheline hot spots.
- Lower cross-shard wake latency for coordination-heavy workloads.
- Better behavior under fan-out/fan-in pressure, especially when many small tasks are launched and joined.

## Project Scope

`spargio` is not trying to be a full Tokio drop-in shim.

We intentionally de-scoped full Tokio-compat readiness emulation (`IORING_OP_POLL_ADD` as a universal compatibility layer) because it requires much larger ongoing investment and dilutes the current benchmark niche.

## What Is Done

- Sharded runtime with `Queue` and Linux `IoUring` backends.
- Cross-shard typed/raw messaging, no-wait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Per-shard stealable inboxes with io_uring `msg_ring` wake path.
- Native lane MVP (`uring-native`): `read_at`, `write_at`.
- Bounded mixed-runtime boundary API: `spargio::boundary`.
- Runtime stats snapshots via `RuntimeHandle::stats_snapshot()`.
- Criterion suites: `ping_pong`, `fanout_fanin`, `disk_io`.
- CI gates and benchmark scripts.
- Mixed-mode reference service example.

## Next Steps

- Move from inbox-based stealing to true deque-style stealing policy (victim selection and fairness controls).
- Extend benchmark reporting to longer runs plus p95/p99 tails for coordination-heavy scenarios.
- Improve disk and native I/O path coverage, then add network I/O slices.
- Tighten regression gates around the fan-out/fan-in niche that currently demonstrates value.

## Inspirations

- `ourio`: <https://github.com/rockorager/ourio>
- `tokio-uring`: <https://github.com/tokio-rs/tokio-uring>

Related runtimes (`glommio`, `monoio`, `compio`) are likely faster for some thread-per-core-only workloads because that is their primary design center.

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

Mixed-mode reference service:

```bash
cargo run --example mixed_mode_service
```

## Repository Map

- `src/lib.rs`: runtime implementation.
- `tests/`: TDD coverage.
- `benches/`: Criterion benchmarks.
- `examples/`: mixed-mode reference app.
- `scripts/`: benchmark smoke/guard helpers.
- `.github/workflows/`: CI regression gates.
- `architecture_decision_records/`: ADR history.
- `IMPLEMENTATION_LOG.md`: chronological implementation and benchmark log.

## Engineering Method

Development style is red/green TDD:

1. Add failing tests.
2. Implement minimal passing behavior.
3. Validate with full test and benchmark build checks.
