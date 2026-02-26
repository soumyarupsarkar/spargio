#!/usr/bin/env bash
set -euo pipefail

./scripts/bench_ping_guardrail.sh
./scripts/bench_fanout_guardrail.sh
