#!/usr/bin/env bash
set -euo pipefail

TMP_OUT="$(mktemp)"
trap 'rm -f "${TMP_OUT}"' EXIT

WARMUP="${WARMUP:-0.05}"
MEASURE="${MEASURE:-0.05}"
SAMPLES="${SAMPLES:-20}"
MAX_RATIO="${MAX_RATIO:-2.5}"

cargo bench --bench fanout_fanin -- \
  --warm-up-time "${WARMUP}" \
  --measurement-time "${MEASURE}" \
  --sample-size "${SAMPLES}" | tee "${TMP_OUT}"

extract_time_line() {
  local label="$1"
  awk -v lbl="$label" '
    $0 ~ lbl { seen=1; next }
    seen && $0 ~ /time:/ { print; exit }
  ' "${TMP_OUT}"
}

to_ns() {
  local value="$1"
  local unit="$2"
  local factor
  case "$unit" in
    ns) factor=1 ;;
    "µs"|us) factor=1000 ;;
    ms) factor=1000000 ;;
    s) factor=1000000000 ;;
    *)
      echo "unknown unit: ${unit}" >&2
      return 1
      ;;
  esac
  awk -v v="$value" -v f="$factor" 'BEGIN { printf "%.0f", v * f }'
}

extract_ns() {
  local label="$1"
  local line
  line="$(extract_time_line "$label")"
  if [[ -z "$line" ]]; then
    echo ""
    return 0
  fi

  local parsed
  parsed="$(echo "$line" | sed -E 's/.*\[([0-9.]+)[[:space:]]+([^[:space:]]+).*/\1 \2/')"
  local value unit
  value="${parsed%% *}"
  unit="${parsed##* }"
  to_ns "$value" "$unit"
}

assert_ratio() {
  local tokio_label="$1"
  local spargio_label="$2"

  local tokio_ns spargio_ns
  tokio_ns="$(extract_ns "$tokio_label")"
  spargio_ns="$(extract_ns "$spargio_label")"

  if [[ -z "$tokio_ns" || -z "$spargio_ns" ]]; then
    echo "guardrail skipped: missing benchmark label(s): ${tokio_label} / ${spargio_label}"
    return 0
  fi

  local ok
  ok="$(awk -v t="$tokio_ns" -v s="$spargio_ns" -v r="$MAX_RATIO" 'BEGIN { if (s <= t * r) print "yes"; else print "no" }')"
  if [[ "$ok" != "yes" ]]; then
    echo "guardrail failed: ${spargio_label}=${spargio_ns}ns exceeded ${MAX_RATIO}x ${tokio_label}=${tokio_ns}ns" >&2
    return 1
  fi

  echo "guardrail ok: ${spargio_label}=${spargio_ns}ns vs ${tokio_label}=${tokio_ns}ns"
}

assert_ratio "fanout_fanin_balanced/tokio_mt_4" "fanout_fanin_balanced/spargio_io_uring"
assert_ratio "fanout_fanin_skewed/tokio_mt_4" "fanout_fanin_skewed/spargio_io_uring"
