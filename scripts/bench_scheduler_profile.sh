#!/usr/bin/env bash
set -euo pipefail

GROUP="${1:-fanout_fanin_skewed}"
BENCH_LABEL="${2:-spargio_io_uring}"

BENCH_NAME="${BENCH_NAME:-fanout_fanin}"
OUT_DIR="${OUT_DIR:-target/scheduler_profiles}"
SUMMARY_JSON="${SUMMARY_JSON:-}"
RUN_PROFILE="${RUN_PROFILE:-1}"
WARMUP="${WARMUP:-0.02}"
MEASURE="${MEASURE:-0.04}"
SAMPLES="${SAMPLES:-10}"
if (( SAMPLES < 10 )); then
  SAMPLES=10
fi

if [[ "${RUN_PROFILE}" == "0" ]]; then
  echo "scheduler profile skipped (RUN_PROFILE=0)"
  exit 0
fi

if ! command -v valgrind >/dev/null 2>&1; then
  echo "valgrind not found; install valgrind to run scheduler profiler lane" >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"

cargo bench --bench "${BENCH_NAME}" --no-run >/dev/null

BIN="$(find target/release/deps -maxdepth 1 -type f -name "${BENCH_NAME}-*" -perm -111 | head -n 1)"
if [[ -z "${BIN}" ]]; then
  echo "unable to locate benchmark binary for ${BENCH_NAME}" >&2
  exit 1
fi

FILTER="${GROUP}/${BENCH_LABEL}"
CALL_OUT="${OUT_DIR}/${GROUP}_${BENCH_LABEL}.callgrind.out"
CACHE_OUT="${OUT_DIR}/${GROUP}_${BENCH_LABEL}.cachegrind.out"

valgrind --tool=callgrind --callgrind-out-file="${CALL_OUT}" \
  "${BIN}" \
  --warm-up-time "${WARMUP}" \
  --measurement-time "${MEASURE}" \
  --sample-size "${SAMPLES}" \
  "${FILTER}" >/dev/null

valgrind --tool=cachegrind --cache-sim=yes --cachegrind-out-file="${CACHE_OUT}" \
  "${BIN}" \
  --warm-up-time "${WARMUP}" \
  --measurement-time "${MEASURE}" \
  --sample-size "${SAMPLES}" \
  "${FILTER}" >/dev/null

callgrind_ir="$(awk '/^summary:/{print $2; exit}' "${CALL_OUT}")"
read -r _ cache_ir cache_i1mr cache_ilmr cache_dr cache_d1mr cache_dlmr cache_dw cache_d1mw cache_dlmw _ <<<"$(awk '/^summary:/{print; exit}' "${CACHE_OUT}")"

callgrind_ir="${callgrind_ir:-0}"
cache_ir="${cache_ir:-0}"
cache_d1mr="${cache_d1mr:-0}"
cache_d1mw="${cache_d1mw:-0}"

echo "scheduler profile complete"
echo "  filter=${FILTER}"
echo "  callgrind_ir=${callgrind_ir}"
echo "  cachegrind_ir=${cache_ir}"
echo "  cachegrind_d1_misses_read=${cache_d1mr}"
echo "  cachegrind_d1_misses_write=${cache_d1mw}"
echo "  callgrind_out=${CALL_OUT}"
echo "  cachegrind_out=${CACHE_OUT}"

if [[ -n "${SUMMARY_JSON}" ]]; then
  cat > "${SUMMARY_JSON}" <<JSON
{
  "group": "${GROUP}",
  "bench": "${BENCH_LABEL}",
  "callgrind_ir": ${callgrind_ir},
  "cachegrind_ir": ${cache_ir},
  "cachegrind_d1mr": ${cache_d1mr},
  "cachegrind_d1mw": ${cache_d1mw},
  "callgrind_out": "${CALL_OUT}",
  "cachegrind_out": "${CACHE_OUT}"
}
JSON
  echo "  summary_json=${SUMMARY_JSON}"
fi
