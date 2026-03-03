# Status and Roadmap

This chapter summarizes what is ready today, what is still limited, and what users should plan around.

## Done

- Sharded runtime and placement APIs.
- Adaptive work-stealing with tunable knobs.
- Core fs/net/io ergonomic layers.
- Native unbound API and unsafe extension submission surface.
- Companion crates for TLS/WS/QUIC/process/signal.
- QUIC native driver-backed default dispatch with bridge fallback mode.
- User-facing performance and operations guidance for tuning and rollout.

## Not Done Yet (Near-term)

- Hostname-based `ToSocketAddrs` paths can still block on DNS resolution.
- Remaining fs helper migration to native io_uring (`canonicalize`, `metadata`, `symlink_metadata`, `set_permissions`) is deferred.
- Work-stealing guidance still needs deeper production case studies and calibration examples.
- Book coverage still needs expansion in advanced API-selection, placement, and failure-mode playbooks.

## Longer-term Improvement Ideas

- Full production-grade higher-level ecosystem maturity and deeper protocol tuning.
- QUIC long-window rollout hardening and requalification depth.
- Multi-endpoint QUIC sharding/fan-out orchestration built into higher-level APIs.
- Fully in-ring directory traversal once stable upstream `getdents` opcode support is available.
- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) remains backlog-only.

## What To Do Today

If you need one of the current gaps now, use these practical paths:

- strict non-DNS connect/bind behavior:
  use explicit `SocketAddr` APIs (`connect_socket_addr*`, `bind_socket_addr`).
- operation not exposed in core runtime API:
  build a safe wrapper on top of `submit_unsafe*` (see [Extending Spargio with Custom io_uring Opcodes](native_extensions.md)).
- metadata-heavy tooling with minimal blocking:
  use `metadata_lite` and `du`/`read_dir` helpers while full in-ring traversal remains unavailable.
- multi-core QUIC listener scaling:
  deploy multiple endpoints explicitly and distribute incoming traffic at the deployment edge.
