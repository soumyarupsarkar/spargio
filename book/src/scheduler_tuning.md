# Scheduler Tuning

Spargio's scheduler remains work-stealing first, but it now exposes a few
explicit knobs so placement behavior can be tuned by workload shape:

- `RuntimeBuilder::steal_budget(...)` controls how many stealable tasks a shard
  drains in one pass.
- `RuntimeBuilder::steal_victim_stride(...)` controls how the victim scan cursor
  advances when stealing from peer shards.
- `RuntimeBuilder::stealable_queue_capacity(...)` controls enqueue-side
  backpressure.

## Practical Starting Points

- Keep `steal_budget` modest for latency-sensitive traffic (small bursts).
- Increase `steal_budget` for throughput-heavy batch workloads.
- Use `steal_victim_stride > 1` when many shards compete for the same hot
  victims and you want broader scan spread.

## Observability

Use `RuntimeHandle::stats_snapshot()` to monitor:

- `steal_attempts`
- `steal_success`
- `stealable_stolen`
- `stealable_backpressure`
- `steal_victim_stride` (effective configured stride)

Tune knobs only with measurements from real workload traces and benchmark/guard
lanes.
