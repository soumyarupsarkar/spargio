# Performance Guide

This chapter is for tuning your application on Spargio, not for benchmarking Spargio itself.
The main rule: measure your workload shape, then tune based on evidence.

## Measure What Users Feel

Track these together:

- latency (`p50`, `p95`, `p99`) for your user-facing operations
- throughput under realistic concurrency and payload sizes
- error/backpressure behavior under sustained load
- scheduler counters from `RuntimeHandle::stats_snapshot()` (especially locality and steal ratios)

Do not optimize for a single number in isolation.

## Use A Representative Workload

Before touching runtime knobs, make sure you have benchmarks reflecting production traffic shape:

- real request mix (read/write ratio, endpoint mix, fanout patterns)
- realistic payload distributions (not only tiny happy-path payloads)
- realistic concurrency (steady plus burst periods)
- realistic topology (same shard count and affinity strategy you expect in deployment)

If your workload is skewed (hot keys/hot connections), include that skew in your benchmark/workload runs.

## Work-Stealing Controls and Profiling

Tune steal controls only after placement and API-level choices are reasonable.

Primary knobs:

- `RuntimeBuilder::steal_budget(...)`: how much stealable work a shard drains per pass
- `RuntimeBuilder::steal_victim_stride(...)`: how victim scans rotate across shards
- `RuntimeBuilder::steal_victim_probe_count(...)`: how many victims are sampled per scan
- `RuntimeBuilder::steal_batch_size(...)`: maximum tasks moved per successful steal
- `RuntimeBuilder::steal_locality_margin(...)`: how strongly locality is favored over migration
- `RuntimeBuilder::steal_fail_cost(...)`: penalty after repeated low-value scans
- `RuntimeBuilder::steal_backoff_min(...)` / `steal_backoff_max(...)`: adaptive cooldown bounds
- `RuntimeBuilder::stealable_queue_capacity(...)`: enqueue-side backpressure threshold
- `RuntimeBuilder::stealable_queue_backend(...)`: shared-queue backend selection

```rust
#[spargio::main]
async fn main(_handle: spargio::RuntimeHandle) -> Result<(), spargio::RuntimeError> {
    let builder = spargio::Runtime::builder()
        .shards(4)
        .steal_budget(64)
        .steal_victim_stride(2)
        .steal_victim_probe_count(3)
        .steal_batch_size(8)
        .steal_locality_margin(2)
        .steal_fail_cost(3)
        .steal_backoff_min(2)
        .steal_backoff_max(64)
        .stealable_queue_capacity(16_384);

    spargio::run_with(builder, |handle| async move {
        let stats = handle.stats_snapshot();
        println!(
            "local_hit_ratio={:.2} stolen_per_scan={:.2} steal_success_rate={:.2}",
            stats.local_hit_ratio(),
            stats.stolen_per_scan(),
            stats.steal_success_rate(),
        );
    })
    .await
}
```

What this does:

- sets explicit steal-policy values at runtime startup
- runs your workload under those settings
- reads scheduler counters so you can judge whether changes improved locality/throughput tradeoffs

For day-2 monitoring and rollout counters, see [Operations Guide](operations_guide.md).

## Profile Before Guessing

Use system profilers to confirm where time and cache misses are going.

Example cachegrind flow:

```bash
cargo build --release
valgrind --tool=cachegrind --cachegrind-out-file=cachegrind.out \
  ./target/release/your_app
cg_annotate cachegrind.out | head -n 120
```

Example callgrind flow:

```bash
valgrind --tool=callgrind --callgrind-out-file=callgrind.out \
  ./target/release/your_app
```

Example Linux perf flow:

```bash
perf stat -d ./target/release/your_app
perf record -g ./target/release/your_app
perf report
```

Use these tools with the same workload driver you use for latency/throughput testing.

## Suggested Tuning Loop

1. capture a baseline on representative load
2. change one thing (placement, knob, buffering, batching, etc.)
3. re-run the same load and compare latency, throughput, and scheduler counters
4. profile with cachegrind/perf if the result is unclear
5. keep only changes that improve your target metrics without hurting critical tails

## Common Mistakes

- tuning steal knobs before verifying placement strategy
- comparing runs with different load shapes or machine settings
- optimizing mean latency while worsening `p95`/`p99`
- making multiple tuning changes at once, then not knowing which one helped

## Further Reading

- The Rust Performance Book: <https://nnethercote.github.io/perf-book/>
