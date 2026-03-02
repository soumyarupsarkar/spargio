#!/usr/bin/env bash
set -euo pipefail

# QUIC interop matrix: spargio endpoint wrappers against raw quinn peers.
cargo test -p spargio-quic --test interop_tdd
