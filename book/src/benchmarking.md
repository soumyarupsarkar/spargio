# Benchmarking and Guardrails

Primary benchmark suites:

- `ping_pong`
- `fanout_fanin`
- `fs_api`
- `net_api`
- `net_experiments`.

Guardrails:

- ratio guardrails (`scripts/bench_ping_guardrail.sh`,
  `scripts/bench_fanout_guardrail.sh`)
- percentile guardrails (`scripts/bench_tail_guardrail.sh`) using Criterion
  `sample.json` p50/p95/p99.

Regression triage loop is documented in `docs/perf_regression_triage.md`.
