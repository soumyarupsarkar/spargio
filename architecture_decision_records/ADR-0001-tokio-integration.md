# ADR-0001: Tokio Compatibility-First Integration with Native io_uring Fast Lane

- Status: Superseded by ADR-0002
- Date: 2026-02-26

## Context

We need Tokio compatibility for migration, while preserving a high-performance `io_uring` path with shard-local ring ownership and `msg_ring` cross-shard signaling.

Key goals:

1. Existing Tokio users can migrate with minimal rewrite.
2. Work stealing is supported where semantically safe.
3. Native `io_uring` ops (`IORING_OP_READ/WRITE/...`) remain available as an explicit fast lane.

## Decision

Adopt a two-lane runtime model with compatibility-first defaults:

1. Default compatibility lane:
   - readiness/reactor behavior implemented with `IORING_OP_POLL_ADD` (`poll` emulation model).
   - Tokio-like task behavior and APIs are the migration baseline.
2. Native fast lane:
   - explicit APIs that bypass readiness emulation and submit true `io_uring` ops.
3. Cross-shard signaling:
   - use `msg_ring` doorbells for inter-thread wakeups/dispatch, avoiding `eventfd` in the normal path.
4. Work stealing:
   - perform stealing at submission/dispatch time.
   - once native I/O is in-flight on a ring, that op remains ring-affine until completion.

## Architecture

### Scheduling

1. Local worker deque + global lock-free injector.
2. `stealable` tasks:
   - `Send` tasks with no in-flight ring-local ownership.
3. `pinned` tasks:
   - tasks owning ring-affine in-flight I/O state.
   - not migrated while in-flight.

### Submission-Time Placement

1. Select target worker before issuing native I/O.
2. Dispatch work to target via shared queue + `msg_ring` doorbell.
3. Target worker performs native `io_uring` submission and completion handling.

### API Shape

1. Compatibility APIs (default):
   - Tokio-oriented task and readiness interfaces.
2. Native extension APIs:
   - explicit submission functions for direct `io_uring` operations.
   - explicit buffer/file registration APIs for hot paths.

## Rationale

1. Migration practicality:
   - `POLL_ADD` compatibility lowers adoption friction for Tokio users.
2. Performance path clarity:
   - native lane keeps a direct route to best `io_uring` performance.
3. Work-stealing compatibility:
   - supports stealing for compatible work while preserving ring correctness.
4. Signaling efficiency:
   - `msg_ring` replaces `eventfd`-style cross-thread wakeups in core runtime flow.

## Reservations and Risks

1. `POLL_ADD` lifecycle complexity:
   - rearm/cancel/drop races are subtle and must be hardened.
2. Compatibility lane overhead:
   - readiness emulation can mask costs and underperform native ops.
3. Disk I/O caveat:
   - readiness semantics are not a full async disk model; native lane is the primary path for disk performance.
4. Dual-lane complexity:
   - scheduler and API surface are larger and require strict documentation/testing.

## Consequences

Positive:

1. Easier Tokio migration.
2. Clear incremental path from compatibility to native fast path.
3. Work stealing available for compatible workloads.

Tradeoffs:

1. More moving parts than a single-lane runtime.
2. Need strong guardrails around pinned vs stealable state transitions.
3. Compatibility performance should not be confused with native-lane performance.

## Alternatives Considered

1. Native-only `io_uring` APIs (no readiness compatibility):
   - rejected for migration friction.
2. Immediate deep Tokio runtime replacement:
   - rejected for implementation risk/complexity before interop proof.
3. Keep prior design with non-default `POLL_ADD`:
   - rejected because it slows practical adoption.

## Rollout Plan

1. Implement compatibility lane behind feature `tokio-compat`.
2. Implement native extension lane behind feature `uring-native`.
3. Add correctness tests:
   - poll rearm/cancel/drop races
   - cross-shard stealing before submission
   - pinned-state enforcement for in-flight native ops
4. Add benchmarks:
   - compatibility lane vs Tokio baseline
   - native lane vs compatibility lane
   - mixed load (stealable CPU + pinned I/O)
5. Promote to default once compatibility correctness and regression budgets are met.
