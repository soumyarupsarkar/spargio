#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <criterion-group> <baseline-label> <candidate-label>" >&2
  exit 2
fi

GROUP="$1"
BASELINE="$2"
CANDIDATE="$3"

CRITERION_DIR="${CRITERION_DIR:-target/criterion}"
MAX_P50_RATIO="${MAX_P50_RATIO:-1.6}"
MAX_P95_RATIO="${MAX_P95_RATIO:-1.6}"
MAX_P99_RATIO="${MAX_P99_RATIO:-1.6}"

baseline_sample="${CRITERION_DIR}/${GROUP}/${BASELINE}/new/sample.json"
candidate_sample="${CRITERION_DIR}/${GROUP}/${CANDIDATE}/new/sample.json"

if [[ ! -f "${baseline_sample}" || ! -f "${candidate_sample}" ]]; then
  echo "guardrail skipped: missing sample(s): ${baseline_sample} / ${candidate_sample}"
  exit 0
fi

percentiles_from_sample() {
  local sample_path="$1"
  python3 - "$sample_path" <<'PY'
import json
import math
import sys

sample_path = sys.argv[1]
with open(sample_path, "r", encoding="utf-8") as f:
    sample = json.load(f)

iters = sample.get("iters", [])
times = sample.get("times", [])
values = []
for i, t in zip(iters, times):
    if i <= 0:
        continue
    values.append(float(t) / float(i))

if not values:
    print("0 0 0")
    raise SystemExit(0)

values.sort()
n = len(values)

def percentile(p: float) -> float:
    rank = max(1, math.ceil(p * n))
    idx = min(n - 1, rank - 1)
    return values[idx]

print(f"{percentile(0.50):.6f} {percentile(0.95):.6f} {percentile(0.99):.6f}")
PY
}

read -r base_p50 base_p95 base_p99 <<<"$(percentiles_from_sample "${baseline_sample}")"
read -r cand_p50 cand_p95 cand_p99 <<<"$(percentiles_from_sample "${candidate_sample}")"

ratio() {
  local baseline="$1"
  local candidate="$2"
  awk -v b="${baseline}" -v c="${candidate}" 'BEGIN { if (b == 0) print 0; else printf "%.6f", c / b }'
}

assert_ratio() {
  local label="$1"
  local baseline="$2"
  local candidate="$3"
  local max_ratio="$4"

  local current_ratio
  current_ratio="$(ratio "${baseline}" "${candidate}")"
  local ok
  ok="$(awk -v r="${current_ratio}" -v m="${max_ratio}" 'BEGIN { if (r <= m) print "yes"; else print "no" }')"
  if [[ "${ok}" != "yes" ]]; then
    echo "guardrail failed: ${GROUP}/${CANDIDATE} ${label} ratio=${current_ratio} exceeds ${max_ratio}" >&2
    echo "  baseline=${baseline}ns candidate=${candidate}ns" >&2
    exit 1
  fi
}

assert_ratio "p50" "${base_p50}" "${cand_p50}" "${MAX_P50_RATIO}"
assert_ratio "p95" "${base_p95}" "${cand_p95}" "${MAX_P95_RATIO}"
assert_ratio "p99" "${base_p99}" "${cand_p99}" "${MAX_P99_RATIO}"

echo "guardrail ok: ${GROUP}/${CANDIDATE} vs ${BASELINE} \
p50=${cand_p50}ns (${GROUP}/${BASELINE}=${base_p50}ns) \
p95=${cand_p95}ns (${GROUP}/${BASELINE}=${base_p95}ns) \
p99=${cand_p99}ns (${GROUP}/${BASELINE}=${base_p99}ns)"
