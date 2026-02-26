# ADR-0002: Prioritize `io_uring` + `msg_ring` Work-Stealing Runtime

- Status: Accepted
- Date: 2026-02-26
- Supersedes: ADR-0001

## Context

The central thesis of `spargio` is not Tokio drop-in compatibility; it is runtime differentiation:

1. native `io_uring` execution for hot paths
2. low-overhead cross-shard coordination through `msg_ring`
3. submission-time placement and work-stealing for stealable work
4. ring-affine guarantees for in-flight native I/O

We considered Tokio compatibility via readiness emulation (`IORING_OP_POLL_ADD` / `POLL_REMOVE`). This can work for some code, but dependency-transparent compatibility requires large, ongoing parity work (runtime semantics, API surface, and ecosystem behavior).

## Decision

Prioritize the core runtime value proposition and de-scope compatibility-first goals:

1. Focus product direction on `io_uring` + `msg_ring` + work-stealing internals.
2. De-scope and phase out Tokio-compatibility as a primary strategic objective.
3. Treat mixed-mode and sidecar deployment as integration options, not the project premise.
4. Keep interoperability boundaries thin and explicit where needed for practical adoption.

## Scope and Non-Goals

In scope:

1. Native fast-path APIs and runtime internals that preserve ring-affine I/O.
2. Submission-time placement policies and true stealing behavior for stealable work.
3. Cross-shard coordination primitives and observability suitable for production tuning.
4. Benchmark-driven validation for coordination-heavy workloads.

Out of scope (for now):

1. Full Tokio drop-in compatibility across dependency internals.
2. Full Tokio API/runtime-semantics parity.
3. Promising ecosystem-transparent migration without dependency changes.

## Rationale

1. Strategic focus:
   - differentiation comes from scheduler/coordination design, not compatibility emulation.
2. Engineering leverage:
   - reducing compatibility maintenance frees effort for performance-critical internals.
3. Delivery realism:
   - dependency-transparent compatibility is costly and long-tail by nature.
4. Clear positioning:
   - `spargio` should win where `msg_ring` coordination and shard-aware scheduling matter.

## Consequences

Positive:

1. Clearer roadmap and less ambiguity about what the project is optimizing for.
2. Faster progress on placement, stealing, and ring-affine native I/O paths.
3. Lower risk of underdelivering on broad compatibility promises.

Tradeoffs:

1. No near-term path to full Tokio replacement.
2. Mixed-mode deployments still need explicit runtime boundaries.
3. Some integrations will require adapters rather than transparent reuse.

## Note on Compatibility Alternatives

A deeper compatibility strategy remains valid for other runtimes or future efforts. Emulating readiness with `IORING_OP_POLL_ADD` and pursuing wider API parity can be worthwhile, but it demands heavy sustained investment. This ADR explicitly chooses not to make that the primary direction for `spargio`.

## Benchmark Focus

Primary target niche for validation:

1. intra-request fan-out/fan-in pipelines (for example log/analytics query serving)
2. shard skew scenarios that stress submission-time rebalancing
3. mixed CPU/control work with ring-affine native I/O

Primary reported metrics:

1. end-to-end p50/p95/p99 latency
2. throughput at fixed latency budgets
3. tail behavior under skew
4. CPU efficiency for equivalent workload and SLO

## Rollout Plan

1. Remove/deprecate compatibility-layer paths that are inconsistent with this focus.
2. Implement submission-time placement policies (`sticky`, explicit shard, policy-driven round-robin).
3. Implement true stealing for stealable CPU/control tasks with ring-affine I/O constraints.
4. Harden boundary contracts for mixed-mode deployment (bounded queues, cancellation, tracing).
5. Add and gate benchmark suites for the fan-out/fan-in niche and skew-heavy scenarios.
