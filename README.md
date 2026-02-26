# spargio

`spargio` is an experimental Rust async runtime exploring a `msg_ring`-first architecture on top of Linux `io_uring`.

Built with Codex.

## Inspirations

This project is directly inspired by:

- `ourio`: <https://github.com/rockorager/ourio>
- `tokio-uring`: <https://github.com/tokio-rs/tokio-uring>

## What We Are Trying To Build

We are building a Tokio-compatible migration path with a native `io_uring` fast lane:

1. Compatibility lane:
   - Tokio-like readiness behavior via `IORING_OP_POLL_ADD` / `POLL_REMOVE`.
   - APIs that let existing Tokio-style code migrate incrementally.
2. Native fast lane:
   - Explicit `io_uring` operations (`READ`, `WRITE`, etc.) for hot paths.
3. Cross-shard signaling:
   - Use `IORING_OP_MSG_RING` for shard-to-shard wakeups/message delivery.
4. Scheduler direction:
   - Keep ring-affine I/O correctness while adding submission-time placement and work-stealing for stealable tasks.

The target is a practical Tokio-interop runtime that can evolve into a high-performance `io_uring`-native execution model without a flag-day rewrite.

## Performance Reality (Current)

We expect runtimes such as `glommio`, `monoio`, and `compio` to be faster in many real workloads today. A major reason is structural: they are designed around strict thread-per-core, ring-affine execution paths that minimize cross-thread scheduling and synchronization overhead.

This project currently carries more compatibility and mixed-mode goals, which introduces extra overhead. Maturity still matters too (scheduler, driver, and API-path optimization depth), but the thread-per-core-first design is a primary performance advantage.

## Current Status

Implemented today:

- Sharded runtime core with two backends:
  - `Queue` backend (baseline/fallback).
  - Linux `IoUring` backend.
- Cross-shard messaging:
  - typed/raw sends, no-wait sends, batched sends, explicit flush ticket.
- Tokio interop handle:
  - `Runtime::handle()`
  - `spawn_pinned`, `spawn_stealable`, `remote`.
- Tokio compat lane:
  - poll reactor (`POLL_ADD`/`POLL_REMOVE`)
  - readiness waits (`wait_readable`, `wait_writable`)
  - wrappers: `CompatFd` and `CompatStreamFd` (`AsyncRead`/`AsyncWrite`).
- Native lane MVP (`uring-native` feature):
  - `RuntimeHandle::uring_native_lane(shard)`
  - `read_at` / `write_at`.
- Benchmarks:
  - message RTT / one-way / cold-start
  - first disk I/O RTT harness.

See [`IMPLEMENTATION_LOG.md`](./IMPLEMENTATION_LOG.md) for chronological detail.

## What Is Not Done Yet

- True deque/injector work-stealing scheduler.
- Submission-time placement policies beyond current basic behavior.
- Re-homing poll compatibility processing into shard driver path.
- Full stress/race hardening suite and benchmark regression gates.

## Platform / Features

- Core crate builds cross-platform.
- Linux-only capabilities (`io_uring`, `msg_ring`, poll-compat, native lane) are feature/target gated.

Main cargo features:

- `tokio-compat`
- `uring-native`
- `glommio-bench` (bench comparison path)

## Quick Start

```bash
cargo test
cargo test --features tokio-compat
cargo test --features uring-native
cargo test --features "tokio-compat uring-native"
cargo bench --no-run
```

## Repository Layout

- `src/lib.rs`: runtime and backend implementation.
- `tests/`: TDD behavior tests for runtime, compat lane, and native lane.
- `benches/`: Criterion benchmarks (`ping_pong`, `disk_io`).
- `architecture_decision_records/`: ADRs for integration strategy.
- `DESIGN_OPTIONS.md`: earlier interface/architecture options.
- `IMPLEMENTATION_LOG.md`: running implementation history.
