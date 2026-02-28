# Performance Regression Triage Loop

This document defines the default regression triage flow for benchmark guardrail failures.

## Capture

1. Re-run guardrails with a stable local profile:
   - `./scripts/bench_kpi_guardrail.sh`
2. Save outputs and Criterion artifacts:
   - preserve `target/criterion/*/new/sample.json`
   - capture command line and host details (CPU model, governor, kernel, commit SHA).

## Compare

1. Compare percentiles (p50/p95/p99) between baseline and candidate samples.
2. Confirm whether failure reproduces across at least two reruns with longer windows:
   - increase `WARMUP`, `MEASURE`, and `SAMPLES`.

## Bisect

1. Use `git bisect` across the suspected window.
2. At each step run:
   - `cargo test --features uring-native`
   - failing benchmark guardrail command(s).
3. Mark the first bad commit and record the benchmark deltas.

## Fix

1. Add a red test/guardrail that captures the specific regression mode.
2. Implement fix and rerun:
   - `cargo test`
   - `cargo test --features uring-native`
   - relevant guardrail scripts.
3. Record before/after percentile deltas and root cause in `IMPLEMENTATION_LOG.md`.
