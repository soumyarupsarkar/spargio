#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <baseline-dir> <candidate-dir>" >&2
  exit 2
fi

BASELINE_DIR="$1"
CANDIDATE_DIR="$2"

CPU_SET="${CPU_SET:-0-3}"
REPEATS="${REPEATS:-3}"
WARMUP="${WARMUP:-0.05}"
MEASURE="${MEASURE:-0.08}"
SAMPLES="${SAMPLES:-15}"
OUT_JSON="${OUT_JSON:-target/scheduler_profiles/net_api_calibration.json}"
FEATURES="${FEATURES:-uring-native}"

mkdir -p "$(dirname "${OUT_JSON}")"

cases=(
  "net_stream_hotspot_rotation_4k/spargio_tcp_8streams_rotating_hotspot"
  "net_pipeline_hotspot_rotation_4k_window32/spargio_tcp_pipeline_hotspot"
  "net_keyed_hotspot_rotation_4k/spargio_tcp_keyed_router_hotspot"
)

unit_to_ns() {
  local value="$1"
  local unit="$2"
  case "$unit" in
    ns) awk -v v="$value" 'BEGIN { printf "%.0f", v }' ;;
    "µs"|us) awk -v v="$value" 'BEGIN { printf "%.0f", v * 1000 }' ;;
    ms) awk -v v="$value" 'BEGIN { printf "%.0f", v * 1000000 }' ;;
    s) awk -v v="$value" 'BEGIN { printf "%.0f", v * 1000000000 }' ;;
    *) echo "0" ;;
  esac
}

parse_time_triplet_ns() {
  local log_file="$1"
  local line
  line="$(awk '/time:[[:space:]]+\[/{print; exit}' "$log_file")"
  if [[ -z "$line" ]]; then
    return 1
  fi

  local low low_unit mid mid_unit high high_unit
  read -r low low_unit mid mid_unit high high_unit <<<"$(echo "$line" | sed -E 's/.*\[([0-9.]+)[[:space:]]+([^[:space:]]+)[[:space:]]+([0-9.]+)[[:space:]]+([^[:space:]]+)[[:space:]]+([0-9.]+)[[:space:]]+([^[:space:]]+)\].*/\1 \2 \3 \4 \5 \6/')"

  local low_ns mid_ns high_ns
  low_ns="$(unit_to_ns "$low" "$low_unit")"
  mid_ns="$(unit_to_ns "$mid" "$mid_unit")"
  high_ns="$(unit_to_ns "$high" "$high_unit")"
  echo "$low_ns $mid_ns $high_ns"
}

results_csv="$(mktemp)"
trap 'rm -f "$results_csv"' EXIT

echo "variant,case,repeat,low_ns,mid_ns,high_ns" > "$results_csv"

run_case() {
  local variant="$1"
  local dir="$2"
  local case_filter="$3"
  local repeat_idx="$4"
  local log_file
  log_file="$(mktemp)"

  (
    cd "$dir"
    if [[ -n "$FEATURES" ]]; then
      taskset -c "$CPU_SET" cargo bench --features "$FEATURES" --bench net_api -- \
        "$case_filter" \
        --warm-up-time "$WARMUP" \
        --measurement-time "$MEASURE" \
        --sample-size "$SAMPLES"
    else
      taskset -c "$CPU_SET" cargo bench --bench net_api -- \
        "$case_filter" \
        --warm-up-time "$WARMUP" \
        --measurement-time "$MEASURE" \
        --sample-size "$SAMPLES"
    fi
  ) > "$log_file"

  local low_ns mid_ns high_ns
  if ! read -r low_ns mid_ns high_ns <<<"$(parse_time_triplet_ns "$log_file")"; then
    echo "failed to parse benchmark timing for filter '${case_filter}' (variant=${variant}, run=${repeat_idx})" >&2
    cat "$log_file" >&2
    exit 1
  fi

  echo "$variant,$case_filter,$repeat_idx,$low_ns,$mid_ns,$high_ns" >> "$results_csv"
  echo "[$variant][$case_filter][run ${repeat_idx}] mid_ns=${mid_ns}"

  rm -f "$log_file"
}

for case_filter in "${cases[@]}"; do
  for run_idx in $(seq 1 "$REPEATS"); do
    run_case baseline "$BASELINE_DIR" "$case_filter" "$run_idx"
    run_case candidate "$CANDIDATE_DIR" "$case_filter" "$run_idx"
  done
  echo
 done

python3 - "$results_csv" "$OUT_JSON" <<'PY'
import csv
import json
import statistics
import sys

csv_path, out_path = sys.argv[1], sys.argv[2]
rows = []
with open(csv_path, newline="", encoding="utf-8") as f:
    reader = csv.DictReader(f)
    for row in reader:
        row["repeat"] = int(row["repeat"])
        for k in ("low_ns", "mid_ns", "high_ns"):
            row[k] = int(row[k])
        rows.append(row)

by_case = {}
for row in rows:
    by_case.setdefault(row["case"], {}).setdefault(row["variant"], []).append(row["mid_ns"])

summary = {"cases": [], "recommendations": []}

for case, variants in sorted(by_case.items()):
    base = variants.get("baseline", [])
    cand = variants.get("candidate", [])
    if not base or not cand:
        continue
    base_mean = statistics.mean(base)
    cand_mean = statistics.mean(cand)
    ratio = cand_mean / base_mean if base_mean else 0.0
    delta_pct = (ratio - 1.0) * 100.0
    entry = {
        "case": case,
        "baseline_mid_ns": base,
        "candidate_mid_ns": cand,
        "baseline_mean_ns": base_mean,
        "candidate_mean_ns": cand_mean,
        "ratio_candidate_over_baseline": ratio,
        "delta_pct": delta_pct,
    }
    summary["cases"].append(entry)

    if delta_pct < -2.0:
        verdict = "improved"
    elif delta_pct > 2.0:
        verdict = "regressed"
    else:
        verdict = "flat"
    summary["recommendations"].append({"case": case, "verdict": verdict, "delta_pct": delta_pct})

with open(out_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)

print(json.dumps(summary, indent=2))
PY

echo "wrote calibration summary: ${OUT_JSON}"
