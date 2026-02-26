#!/usr/bin/env bash
set -euo pipefail

WARMUP="${WARMUP:-0.10}"
MEASURE="${MEASURE:-0.10}"
SAMPLES="${SAMPLES:-20}"

cargo bench --bench fanout_fanin -- \
  --warm-up-time "${WARMUP}" \
  --measurement-time "${MEASURE}" \
  --sample-size "${SAMPLES}"
