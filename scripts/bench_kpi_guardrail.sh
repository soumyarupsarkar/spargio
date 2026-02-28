#!/usr/bin/env bash
set -euo pipefail

./scripts/bench_ping_guardrail.sh
./scripts/bench_fanout_guardrail.sh

./scripts/bench_tail_guardrail.sh steady_ping_pong_rtt tokio_two_worker spargio_io_uring
./scripts/bench_tail_guardrail.sh steady_one_way_send_drain tokio_two_worker spargio_io_uring
./scripts/bench_tail_guardrail.sh cold_start_ping_pong tokio_two_worker spargio_io_uring

MAX_P50_RATIO=2.5 MAX_P95_RATIO=2.5 MAX_P99_RATIO=2.5 \
  ./scripts/bench_tail_guardrail.sh fanout_fanin_balanced tokio_mt_4 spargio_io_uring
MAX_P50_RATIO=2.5 MAX_P95_RATIO=2.5 MAX_P99_RATIO=2.5 \
  ./scripts/bench_tail_guardrail.sh fanout_fanin_skewed tokio_mt_4 spargio_io_uring
