#!/usr/bin/env bash
set -euo pipefail

# Long-window QUIC soak + fault qualification.
cargo test -p spargio-quic --test soak_tdd -- --ignored
