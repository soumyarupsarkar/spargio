#!/usr/bin/env bash
set -euo pipefail

MAX_P95_REGRESSION_PCT="${MAX_P95_REGRESSION_PCT:-15.0}"
MAX_P99_REGRESSION_PCT="${MAX_P99_REGRESSION_PCT:-20.0}"
MIN_THROUGHPUT_RATIO="${MIN_THROUGHPUT_RATIO:-0.85}"

python3 - <<'PY'
import json
import os
import sys

max_p95 = float(os.environ.get("MAX_P95_REGRESSION_PCT", "15.0"))
max_p99 = float(os.environ.get("MAX_P99_REGRESSION_PCT", "20.0"))
min_tp_ratio = float(os.environ.get("MIN_THROUGHPUT_RATIO", "0.85"))

fixture = os.environ.get("QUIC_PERF_FIXTURE")
if fixture:
    with open(fixture, "r", encoding="utf-8") as f:
        payload = json.load(f)
    baseline = payload["baseline"]
    sample = payload["sample"]
    baseline_p95 = float(baseline["p95_ms"])
    baseline_p99 = float(baseline["p99_ms"])
    baseline_tp = float(baseline["throughput_mbps"])
    sample_p95 = float(sample["p95_ms"])
    sample_p99 = float(sample["p99_ms"])
    sample_tp = float(sample["throughput_mbps"])
else:
    baseline_p95 = float(os.environ["BASELINE_P95_MS"])
    baseline_p99 = float(os.environ["BASELINE_P99_MS"])
    baseline_tp = float(os.environ["BASELINE_THROUGHPUT_MBPS"])
    sample_p95 = float(os.environ["SAMPLE_P95_MS"])
    sample_p99 = float(os.environ["SAMPLE_P99_MS"])
    sample_tp = float(os.environ["SAMPLE_THROUGHPUT_MBPS"])

def regression_pct(baseline: float, sample: float) -> float:
    if baseline <= 0.0:
        return 0.0
    return ((sample - baseline) / baseline) * 100.0

p95_reg = regression_pct(baseline_p95, sample_p95)
p99_reg = regression_pct(baseline_p99, sample_p99)
throughput_ratio = 1.0 if baseline_tp <= 0.0 else sample_tp / baseline_tp

pass_gate = (
    p95_reg <= max_p95 and
    p99_reg <= max_p99 and
    throughput_ratio >= min_tp_ratio
)

print(
    "quic perf gate: "
    f"pass={str(pass_gate).lower()} "
    f"p95_regression_pct={p95_reg:.3f} "
    f"p99_regression_pct={p99_reg:.3f} "
    f"throughput_ratio={throughput_ratio:.4f}"
)

if not pass_gate:
    print(
        "quic perf gate failed: "
        f"max_p95={max_p95} max_p99={max_p99} min_throughput_ratio={min_tp_ratio}",
        file=sys.stderr,
    )
    sys.exit(1)
PY
