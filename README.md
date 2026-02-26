# spargio

`spargio` is an experimental Rust `io_uring` runtime focused on `msg_ring`-based cross-shard coordination and work-stealing-friendly scheduling.

Built with Codex.

## Inspirations

- `ourio`: <https://github.com/rockorager/ourio>
- `tokio-uring`: <https://github.com/tokio-rs/tokio-uring>

## Current Aim

The core value proposition is an `io_uring` runtime that supports work stealing through `msg_ring`-based cross-shard coordination.

Primary goals:

1. Native `io_uring` operations for hot paths.
2. Low-overhead shard-to-shard signaling via `IORING_OP_MSG_RING`.
3. Submission-time placement and true work-stealing for stealable work.
4. Ring-affine execution for in-flight native I/O.

Tokio-compatibility note:

1. Tokio-like readiness behavior can be simulated using `IORING_OP_POLL_ADD` / `IORING_OP_POLL_REMOVE`.
2. We considered this as a route toward drop-in replacement behavior, but that path requires heavy ongoing investment to match runtime semantics and dependency expectations.
3. For `spargio`, we are intentionally prioritizing native runtime capabilities and mixed-mode communication over building a full drop-in compatibility layer.

This strategy direction is recorded in ADR-0002.

## Why This Direction

Broad Tokio compatibility without a full shim/fork does not transparently migrate dependencies that internally use Tokio runtime APIs.

Given that constraint, `spargio` focuses on differentiated runtime internals first:

1. `msg_ring` coordination as a first-class primitive.
2. placement and stealing policies tuned for shard-aware execution.
3. benchmarked wins on coordination-heavy fan-out/fan-in workloads.

The runtime can still be deployed in mixed mode with Tokio where that is practical, but mixed mode is a deployment option, not the project premise.

## Status Snapshot

Implemented:

- Sharded runtime core with `Queue` and Linux `IoUring` backends.
- Cross-shard typed/raw messaging, no-wait sends, batched sends, and flush tickets.
- Runtime handle APIs (`spawn_pinned`, `spawn_stealable`, `remote`).
- Native lane MVP (`uring-native`) with `read_at` and `write_at`.
- Bench suites for ping/pong, one-way drain, cold start, and first disk I/O RTT harness.

In progress / remaining:

- True deque/injector work-stealing scheduler.
- Richer submission-time placement policies.
- Boundary hardening for mixed-mode usage and benchmark gates for target fan-out/fan-in workloads.

## Engineering Process

Development is TDD-first using red/green cycles:

1. Add or update failing tests (red).
2. Implement the smallest viable change to pass (green).
3. Run full validation and benchmark build checks.

Project records:

- Implementation log: [`IMPLEMENTATION_LOG.md`](./IMPLEMENTATION_LOG.md)
- ADRs: [`architecture_decision_records/`](./architecture_decision_records/)

## Platform and Features

- Core crate builds cross-platform.
- Linux-only capabilities (`io_uring`, `msg_ring`, native lane) are target/feature gated.

Main cargo features:

- `uring-native`
- `glommio-bench`

## Quick Start

```bash
cargo test
cargo test --features uring-native
cargo bench --no-run
```

## Compatibility Note

`spargio` can interoperate with Tokio and other runtimes via explicit mixed-mode boundaries and message passing.

Building a dependency-transparent drop-in compatibility layer is possible in theory (for example via readiness emulation using `IORING_OP_POLL_ADD`), but it is a large maintenance commitment. We are not treating that as the primary direction for this project.

## Repository Layout

- `src/lib.rs`: runtime and backend implementation.
- `tests/`: behavior and TDD coverage.
- `benches/`: criterion benchmarks (`ping_pong`, `disk_io`).
- `architecture_decision_records/`: design decisions and pivots.
- `IMPLEMENTATION_LOG.md`: chronological implementation and benchmark notes.
