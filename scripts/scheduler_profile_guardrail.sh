#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <baseline-summary.json> <candidate-summary.json>" >&2
  exit 2
fi

BASELINE_JSON="$1"
CANDIDATE_JSON="$2"

MAX_CALLGRIND_IR_RATIO="${MAX_CALLGRIND_IR_RATIO:-1.20}"
MAX_CACHEGRIND_D1MR_RATIO="${MAX_CACHEGRIND_D1MR_RATIO:-1.20}"
MAX_CACHEGRIND_D1MW_RATIO="${MAX_CACHEGRIND_D1MW_RATIO:-1.20}"

if [[ ! -f "${BASELINE_JSON}" || ! -f "${CANDIDATE_JSON}" ]]; then
  echo "guardrail skipped: missing summary json(s)"
  exit 0
fi

read_metric() {
  local file="$1"
  local key="$2"
  python3 - "$file" "$key" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)

print(data.get(sys.argv[2], 0))
PY
}

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
    echo "guardrail failed: ${label} ratio=${current_ratio} exceeds ${max_ratio}" >&2
    echo "  baseline=${baseline} candidate=${candidate}" >&2
    exit 1
  fi

  echo "guardrail ok: ${label} ratio=${current_ratio}"
}

base_callgrind_ir="$(read_metric "${BASELINE_JSON}" "callgrind_ir")"
cand_callgrind_ir="$(read_metric "${CANDIDATE_JSON}" "callgrind_ir")"
base_d1mr="$(read_metric "${BASELINE_JSON}" "cachegrind_d1mr")"
cand_d1mr="$(read_metric "${CANDIDATE_JSON}" "cachegrind_d1mr")"
base_d1mw="$(read_metric "${BASELINE_JSON}" "cachegrind_d1mw")"
cand_d1mw="$(read_metric "${CANDIDATE_JSON}" "cachegrind_d1mw")"

assert_ratio "callgrind_ir" "${base_callgrind_ir}" "${cand_callgrind_ir}" "${MAX_CALLGRIND_IR_RATIO}"
assert_ratio "cachegrind_d1mr" "${base_d1mr}" "${cand_d1mr}" "${MAX_CACHEGRIND_D1MR_RATIO}"
assert_ratio "cachegrind_d1mw" "${base_d1mw}" "${cand_d1mw}" "${MAX_CACHEGRIND_D1MW_RATIO}"
