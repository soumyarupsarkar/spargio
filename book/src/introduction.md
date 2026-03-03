# Introduction

`spargio` is a **work-stealing `io_uring`-based async runtime** for Rust, using `msg_ring` for cross-thread coordination, with explicit task placement across shards.

Instead of a strict thread-per-core/share-nothing execution model like other `io_uring` runtimes (`glommio`/`monoio`/`compio` and `tokio_uring`), `spargio` uses submission-time steering of stealable tasks across threads (a novel form of work-stealing).

Out-of-the-box, we support async disk I/O, network I/O (including TLS/WebSockets/QUIC), process execution, and signal handling, and provide an extension API for additional `io_uring` operations. We support both `tokio`-style stealable tasks and `compio`-style pinned (thread-affine) tasks.

This book is organized for fast decision-making:

- Start with [Quick Start](quick_start.md) if you want runnable code first.
- Read [Task Placement and !Send Execution](runtime_model.md) before tuning placement or performance.
- Use [I/O API Layers and Selection](io_apis.md) when choosing high-level vs native APIs.
- Use [Companion Protocol Crates (TLS/WS/QUIC/Process/Signal)](protocol_crates.md) for TLS/WS/QUIC/process/signal.
- Use [Extending Spargio with Custom io_uring Opcodes](native_extensions.md) when core APIs do not expose a needed operation.
- Use [Performance Guide](performance_guide.md) and [Operations Guide](operations_guide.md) for production workflows.

## Who This Book Is For

- Engineers building high-throughput services on Linux with shard-aware hot paths.
- Engineers building data- and file-processing applications on Linux with files of varying sizes.
- Engineers extending Spargio with custom `io_uring` operations.
- Teams migrating from Tokio/Compio/monoio and needing explicit placement guidance.

## Scope

This book covers:

- Runtime entry and lifecycle.
- Placement (`Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`) and `!Send` execution.
- Core fs/net/io APIs and known caveats.
- Companion protocol crates and maturity expectations.
- Native extension safety model.
- Benchmarking, profiling, and rollout playbooks.

This book does not attempt to replace upstream protocol manuals (`rustls`, `tungstenite`, `quinn`) or Linux kernel references.
