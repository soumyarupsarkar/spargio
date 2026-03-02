#!/usr/bin/env bash
set -euo pipefail

# Companion crate hardening lane: run broader behavioral suites, not smoke only.
cargo test -p spargio-process --tests
cargo test -p spargio-signal --tests
cargo test -p spargio-protocols --tests --features uring-native
cargo test -p spargio-tls --tests
cargo test -p spargio-ws --tests
cargo test -p spargio-quic --test quic_tdd
cargo test -p spargio-quic --test interop_tdd
