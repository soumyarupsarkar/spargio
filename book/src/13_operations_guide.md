# Operations Guide

This chapter covers operational readiness for production services.

## Observability Checklist

Use `RuntimeHandle::stats_snapshot()` and track:

- message throughput and backpressure counters
- steal attempts/success/skip reasons
- local-hit ratio vs stolen-per-scan
- boundary timeout and overload behavior

Track these metrics across rollout stages, not only load tests.
Knob definitions and profiling workflow live in
[Performance Guide](12_performance_guide.md#work-stealing-controls-and-profiling).

```rust
use std::time::Duration;

#[spargio::main]
async fn main(handle: spargio::RuntimeHandle) {
    loop {
        let runtime = handle.stats_snapshot();
        let boundary = spargio::boundary::stats_snapshot();

        println!(
            "local_hit_ratio={:.2} stolen_per_scan={:.2} steal_success_rate={:.2} boundary_timeouts={} boundary_overload={}",
            runtime.local_hit_ratio(),
            runtime.stolen_per_scan(),
            runtime.steal_success_rate(),
            boundary.timed_out,
            boundary.overloaded
        );

        if runtime.stealable_backpressure > 0 {
            eprintln!("backpressure observed; review queue capacity and placement mix");
        }

        spargio::sleep(Duration::from_secs(10)).await;
    }
}
```

What this does:

- pulls runtime and boundary snapshots on a fixed interval.
- computes scheduler health ratios directly from snapshot helpers.
- flags backpressure immediately so operators can tune before tail latency grows.

## Soak and Fault Coverage

Run scheduled soak and fault scenarios with your own workload driver:

- long-window soak:
  keep traffic running for hours, not minutes, and watch for drift.
- fault injection:
  inject connection resets, slow downstreams, and temporary dependency failures.
- recovery checks:
  confirm the system returns to normal latency/error budgets after faults end.

Minimum pass criteria to document in rollout notes:

- no sustained growth in error-rate counters
- stable tail latency over repeated windows
- no monotonic increase in backpressure or timeout counters

## Rollout Guidance

Use staged rollout with clear rollback criteria:

1. canary with fixed workload cohort
2. verify latency tails and error rates
3. verify scheduler and boundary counters are within expected range
4. expand only after repeated stable windows

For QUIC, keep `rollout_stage` conservative until long-window qualification passes are stable.

Example rollout checkpoints:

- canary: no significant increase in `p95`/`p99`
- partial rollout: backpressure/timeout counters remain within agreed thresholds
- full rollout: sustained stability over multiple traffic windows

## Runbook Seeds

Document and rehearse concrete triggers and first responses:

- queue saturation:
  if backpressure counters climb continuously, reduce fanout/batch size or raise queue capacity after validating memory budget.
- timeout spikes:
  if boundary timeout counters jump, verify downstream latency first, then revisit timeout budgets and placement strategy.
- protocol fallback:
  if a protocol path regresses under load, define explicit fallback behavior (for example QUIC backend mode changes) and rollback thresholds.
- performance regression triage:
  define who captures baseline comparison, who profiles (`cachegrind`/`perf`), and who decides rollout/rollback.
