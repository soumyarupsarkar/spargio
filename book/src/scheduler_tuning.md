# Scheduler Tuning

Spargio's scheduler remains work-stealing first, but it now exposes a few
explicit knobs so placement behavior can be tuned by workload shape:

- `RuntimeBuilder::steal_budget(...)` controls how many stealable tasks a shard
  drains in one pass.
- `RuntimeBuilder::steal_victim_stride(...)` controls how the victim scan cursor
  advances when stealing from peer shards.
- `RuntimeBuilder::steal_victim_probe_count(...)` controls how many candidate
  victims are sampled per steal scan before selecting the best queue.
- `RuntimeBuilder::steal_batch_size(...)` controls maximum tasks stolen per scan
  when remote backlog is high.
- `RuntimeBuilder::steal_locality_margin(...)` controls how conservative the
  steal gate is before paying migration cost.
- `RuntimeBuilder::steal_fail_cost(...)` sets the penalty weight used by the
  adaptive steal gate after recent failed scans.
- `RuntimeBuilder::steal_backoff_min(...)` / `steal_backoff_max(...)` tune the
  adaptive cooldown window after repeated low-value scans.
- `RuntimeBuilder::stealable_queue_capacity(...)` controls enqueue-side
  backpressure.
- `RuntimeBuilder::stealable_queue_backend(...)` selects queue backend
  (`Mutex` default or `SegQueueExperimental`).

## Practical Starting Points

- Keep `steal_budget` modest for latency-sensitive traffic (small bursts).
- Increase `steal_budget` for throughput-heavy batch workloads.
- Use `steal_victim_stride > 1` when many shards compete for the same hot
  victims and you want broader scan spread.

## Observability

Use `RuntimeHandle::stats_snapshot()` to monitor:

- `steal_attempts`
- `steal_scans`
- `steal_success`
- `steal_skipped_backoff`
- `steal_skipped_locality`
- `steal_failed_streak_max`
- `stealable_stolen`
- `stealable_local_hits` and `RuntimeStats::local_hit_ratio()`
- `RuntimeStats::stolen_per_scan()`
- `stealable_wake_sent` / `stealable_wake_coalesced`
- `stealable_backpressure`
- `steal_victim_stride` (effective configured stride)

Tune knobs only with measurements from real workload traces and benchmark/guard
lanes.
