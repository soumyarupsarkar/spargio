#!/usr/bin/env bash
set -euo pipefail

# Companion crates should stay buildable/testable as a coherent bridge layer.
cargo test -p spargio-protocols --features uring-native
cargo test -p spargio-tls --test tls_tdd
cargo test -p spargio-ws --test ws_tdd
cargo test -p spargio-quic --test quic_tdd
cargo test -p spargio-process
cargo test -p spargio-signal
