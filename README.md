# spargio

`spargio` is a **work-stealing `io_uring`-based async runtime** for Rust, using `msg_ring` for cross-thread coordination.

Instead of a strict thread-per-core/share-nothing execution model like other `io_uring` runtimes (`glommio`/`monoio`/`compio` and `tokio_uring`), `spargio` uses submission-time steering of stealable tasks across threads (a novel form of work-stealing).

In our benchmarks (detailed below), `spargio` outperforms `compio` (and likely all share-nothing runtimes) in imbalanced workloads by up to 70%, and outperforms `tokio` for cases involving high coordination or disk I/O by up to 320%. `compio` leads for sustained, balanced workloads by up to 70%.

Out-of-the-box, we support async disk I/O, network I/O (including TLS/WebSockets/QUIC), process execution, and signal handling, and provide an extension API for additional `io_uring` operations. We support both `tokio`-style stealable tasks and `compio`-style pinned (thread-affine) tasks.

## Disclaimer

`spargio` began as an experimental proof-of-concept built with Codex. I have not manually reviewed all the code yet. Use for evaluation only.

## Quick start

Pre-requisites: Linux 6.0+ recommended (5.18+ for core io_uring + msg_ring paths)

Add `spargio` as a dependency:
```bash
cargo add spargio --features macros,uring-native
```

Then use it for native I/O operations and stealable task spawning:
```rust
use spargio::{fs::File, net::TcpListener, RuntimeHandle};

#[spargio::main]
async fn main(handle: RuntimeHandle) -> std::io::Result<()> {
    std::fs::create_dir_all("ingest-out")?;
    let listener = TcpListener::bind(handle.clone(), "127.0.0.1:7001").await?;
    let mut id = 0u64;

    loop {
        let (stream, _) = listener.accept_round_robin().await?;
        let (h, s, path) = (handle.clone(), stream.clone(), format!("ingest-out/{id}.bin"));
        id += 1;

        stream.spawn_stealable_on_session(&handle, async move {
            let file = File::create(h, path).await.unwrap();
            let (n, buf) = s.recv_owned(vec![0; 64 * 1024]).await.unwrap();
            file.write_all_at(0, &buf[..n]).await.unwrap();
            file.fsync().await.unwrap();
        }).expect("spawn");
    }
}
```

## Tokio Integration

Recommended model today:

- Run Tokio and Spargio side-by-side.
- Exchange work/results through explicit boundaries (`spargio::boundary`, channels, adapters).
- Move selected hot paths into Spargio without forcing full dependency migration.

Note: uniquely to Spargio, a Tokio-compat readiness shim based on `IORING_OP_POLL_ADD` is possible to build on top of it without sacrificing work-stealing, but building and maintaining a dependency-transparent drop-in lane would be a large investment.

## Inspirations and Further Reading

Using `msg_ring` for coordination is heavily inspired by [`ourio`](https://github.com/rockorager/ourio). We extend that idea to work-stealing.

Wondering whether to build a work-stealing pool using `io_uring` at all was inspired by the following (excellent) blog posts:
- https://emschwartz.me/async-rust-can-be-a-pleasure-to-work-with-without-send-sync-static/
- https://without.boats/blog/thread-per-core/

## Terminology: Shards

In Spargio, a shard is one worker thread + its `io_uring` ring (`SQ` + `CQ`) + a local run/command queue. Internally within Spargio, we pass work from one shard to another by enqueueing work and injecting CQEs across shards, waking up a recipient worker thread to drain pending work from its queue.

## Benchmark Results

All benchmark tables below report Criterion `mean` wall-clock iteration latency
(point estimates from `estimates.json`). Speedup columns use
`baseline_mean / spargio_mean` (higher is better for Spargio).

### Coordination-focused workloads (Tokio vs Spargio)

| Benchmark | Description | Tokio | Spargio | Speedup |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | Two-worker request/ack round-trip loop | `1.409 ms` | `367.09 us` | `3.8x` |
| `steady_one_way_send_drain` | One-way sends, then explicit drain barrier | `64.97 us` | `47.27 us` | `1.4x` |
| `cold_start_ping_pong` | Includes runtime/harness startup and teardown | `467.35 us` | `238.07 us` | `2.0x` |
| `fanout_fanin_balanced` | Even fanout/fanin across shards | `1.421 ms` | `1.185 ms` | `1.2x` |
| `fanout_fanin_skewed` | Skewed fanout/fanin with hotspot pressure | `2.354 ms` | `1.938 ms` | `1.2x` |

Compio is not listed in this coordination-only table because it is share-nothing (thread-per-core), while these cases are focused on cross-shard coordination behavior.

### Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `fs_read_rtt_4k` (`qd=1`) | 4 KiB file read latency, depth 1 | `1.582 ms` | `1.218 ms` | `1.543 ms` | `1.3x` | `1.3x` |
| `fs_read_throughput_4k_qd32` | 4 KiB file reads, queue depth 32 | `14.751 ms` | `6.689 ms` | `5.281 ms` | `2.2x` | `0.8x` |
| `net_echo_rtt_256b` (`qd=1`) | 256-byte TCP echo latency, depth 1 | `7.365 ms` | `5.967 ms` | `6.578 ms` | `1.2x` | `1.1x` |
| `net_stream_throughput_4k_window32` | 4 KiB stream throughput, window 32 | `13.429 ms` | `12.110 ms` | `6.991 ms` | `1.1x` | `0.6x` |

### Imbalanced Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `net_stream_imbalanced_4k_hot1_light7` | 8 streams, 1 static hot + 7 light, 4 KiB frames | `15.558 ms` | `14.192 ms` | `13.772 ms` | `1.1x` | `1.0x` |
| `net_stream_hotspot_rotation_4k` | 8 streams, rotating hotspot each step, I/O-only | `10.093 ms` | `11.004 ms` | `18.784 ms` | `0.9x` | `1.7x` |
| `net_pipeline_hotspot_rotation_4k_window32` | 8 streams, rotating hotspot with recv/CPU/send pipeline | `30.103 ms` | `33.698 ms` | `57.827 ms` | `0.9x` | `1.7x` |
| `net_keyed_hotspot_rotation_4k` | 8 streams, rotating hotspot with keyed ownership routing | `10.598 ms` | `11.148 ms` | `18.499 ms` | `1.0x` | `1.7x` |

## Benchmark Interpretation

TL;DR: As expected, Spargio is strongest on coordination-heavy and low-depth latency workloads; Compio is strongest on sustained balanced stream throughput. Tokio is near parity with Spargio on rotating-hotspot network shapes.

- Spargio leads in coordination-heavy cross-shard cases versus Tokio (`steady_ping_pong_rtt`, `steady_one_way_send_drain`, `cold_start_ping_pong`, `fanout_fanin_*`).
- Spargio leads in low-depth fs/net latency (`fs_read_rtt_4k`, `net_echo_rtt_256b`) versus both Tokio and Compio.
- Compio leads in sustained balanced stream throughput and static-hotspot imbalance (`net_stream_throughput_4k_window32`, `net_stream_imbalanced_4k_hot1_light7`), while Spargio is currently ahead of Tokio in both of those cases.
- Tokio and Spargio are near parity in rotating-hotspot stream/pipeline cases and keyed routing (`net_stream_hotspot_rotation_4k`, `net_pipeline_hotspot_rotation_4k_window32`, `net_keyed_hotspot_rotation_4k`).

For performance, different workload shapes favor different runtimes.

### Exploratory Benchmarks (Subject to Change, May Be Removed)

These workloads focus on mixed coordination/dispatch shapes,
queue-depth-insensitive patterns, and fs+net deadline-churn microservice
paths.

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `net_keyed_hotspot_rotation_4k_window64_cpu` | Keyed rotating hotspot with larger window + CPU tail | `12.958 ms` | `14.280 ms` | `24.636 ms` | `0.9x` | `1.7x` |
| `ingress_dispatch_to_workers_rr_256b_ack` | Ingress dispatch loop with 256B ACK-shaped payloads | `48.631 ms` | `44.360 ms` | `76.128 ms` | `1.1x` | `1.7x` |
| `fs_net_microservice_4k_read_then_256b_reply_qd1` | 4KiB read then 256B reply per request (qd=1) | `15.906 ms` | `11.275 ms` | `13.342 ms` | `1.4x` | `1.2x` |
| `fanout_fanin_rotating_hot_partition_4k_window32` | Rotating hot partition with recv/CPU/send pipeline | `29.807 ms` | `31.997 ms` | `57.288 ms` | `0.9x` | `1.8x` |
| `session_owner_with_spillover_4k` | Session-owned streams under spillover pressure | `39.773 ms` | `41.490 ms` | `75.135 ms` | `1.0x` | `1.8x` |
| `net_burst_flip_imbalance_4k` | Burst-heavy hotspot that flips owner periodically | `92.295 ms` | `91.988 ms` | `72.799 ms` | `1.0x` | `0.8x` |
| `fanin_barrier_micro_batches_1k` | Fan-in barrier with micro-batches | `53.850 ms` | `51.174 ms` | `87.086 ms` | `1.1x` | `1.7x` |
| `serial_dep_chain_rpc_256b` | Serial dependency chain of RPCs (qd=1) | `29.779 ms` | `20.838 ms` | `25.075 ms` | `1.4x` | `1.2x` |
| `keyed_hotspot_flip_p99_4k` | Keyed hotspot ownership flips each phase | `73.335 ms` | `74.969 ms` | `56.894 ms` | `1.0x` | `0.8x` |
| `fanin_barrier_rounds_1k` | Cross-shard fan-in barrier rounds | `54.253 ms` | `48.981 ms` | `77.533 ms` | `1.1x` | `1.6x` |
| `wakeup_sparse_event_rtt_64b` | Sparse wakeups with idle gaps between tiny events | `16.836 ms` | `15.289 ms` | `16.244 ms` | `1.1x` | `1.1x` |
| `timer_cancel_reschedule_storm` | Timer cancel/reschedule churn | `1.097 s` | `14.953 ms` | `14.458 ms` | `73.4x` | `1.0x` |
| `mixed_control_data_plane_4k_plus_64b` | Mixed 4KiB data-plane with 64B control RPCs | `26.742 ms` | `24.522 ms` | `18.396 ms` | `1.1x` | `0.8x` |
| `bounded_pipeline_backpressure_4k_window2` | Bounded pipeline under backpressure (window=2) | `21.725 ms` | `19.725 ms` | `33.063 ms` | `1.1x` | `1.7x` |
| `post_io_cpu_locality_4k_window1` | Post-I/O CPU locality-sensitive pipeline (window=1) | `17.837 ms` | `16.611 ms` | `29.331 ms` | `1.1x` | `1.8x` |
| `fs_net_microservice_deadline_dispatch_4k_read_256b_reply` | 4KiB read + 256B reply with deadline churn and ingress dispatch | `604.230 ms` | `53.017 ms` | `83.323 ms` | `11.4x` | `1.6x` |
| `net_echo_rtt_deadline_routing_256b` | RPC gateway shape: RTT + routing + deadline churn | `481.742 ms` | `56.117 ms` | `80.623 ms` | `8.6x` | `1.4x` |
| `net_stream_multitenant_4k_window8` | Multi-tenant keyed stream with smaller in-flight window | `27.898 ms` | `26.968 ms` | `45.805 ms` | `1.0x` | `1.7x` |
| `net_stream_hotflip_4k` | Hot owner flips quickly across streams | `99.604 ms` | `98.086 ms` | `76.627 ms` | `1.0x` | `0.8x` |
| `net_pipeline_barrier_4k_window4` | Pipeline with explicit barrier rounds (window=4) | `35.477 ms` | `33.237 ms` | `53.151 ms` | `1.1x` | `1.6x` |
| `keyed_router_with_session_owner_spillover_4k` | Keyed owner routing plus spillover pressure | `49.198 ms` | `48.075 ms` | `86.503 ms` | `1.0x` | `1.8x` |
| `fs_metadata_then_reply_qd1` | fstat+read+small reply with deadline churn (qd=1) | `238.254 ms` | `20.541 ms` | `24.149 ms` | `11.6x` | `1.2x` |
| `high_depth_fanout_first_k_cancel_256b_window64` | High-depth fanout with first-K style cancellation pressure | `202.849 ms` | `119.585 ms` | `204.941 ms` | `1.7x` | `1.7x` |
| `high_depth_multitenant_keyed_router_4k_window64` | High-depth multi-tenant keyed router with rotating hotspots | `91.931 ms` | `95.681 ms` | `173.001 ms` | `1.0x` | `1.8x` |
| `high_depth_barriered_pipeline_4k_window64` | High-depth pipeline with barrier synchronization rounds | `63.167 ms` | `68.530 ms` | `117.868 ms` | `0.9x` | `1.7x` |
| `high_depth_deadline_gateway_256b_window64` | High-depth gateway with routing, RPC, and deadline churn | `249.920 ms` | `68.963 ms` | `72.443 ms` | `3.6x` | `1.1x` |
| `high_depth_fs_net_admission_control_4k_read_256b_reply_window64` | High-depth fs+net admission-control path with timer churn | `131.842 ms` | `32.336 ms` | `54.807 ms` | `4.1x` | `1.7x` |

## What's Done

- Sharded runtime with Linux `IoUring` backend.
- Cross-shard typed/raw messaging, nowait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Work-stealing scheduler with adaptive steal gating/backoff, victim probing, batch steals, wake coalescing, backpressure, and runtime stats.
- Runtime primitives: `sleep`, `sleep_until`, `timeout`, `timeout_at`, `Interval`/`interval_at`, `Sleep` (resettable deadline timer), `CancellationToken`, and `TaskGroup` cooperative cancellation.
- Runtime entry ergonomics: async-first `spargio::run(...)`, `spargio::run_with(builder, ...)`, and optional `#[spargio::main(...)]` via `macros`.
- Runtime utility bridge knobs: `RuntimeHandle::spawn_blocking(...)` and `RuntimeBuilder::thread_affinity(...)`.
- Local `!Send` ergonomics: `run_local_on(...)` and `RuntimeHandle::spawn_local_on(...)` for shard-pinned local futures.
- Unbound native API: `RuntimeHandle::uring_native_unbound() -> UringNativeAny` with file ops (`read_at`, `read_at_into`, `write_at`, `fsync`) and stream/socket ops (`recv`, `send`, `send_owned`, `recv_owned`, `send_all_batch`, `recv_multishot_segments`), plus submission-time shard selector, FD affinity leases, and active op route tracking.
- Low-level unsafe native extension API: `UringNativeAny::{submit_unsafe, submit_unsafe_on_shard}` for custom SQE/CQE workflows in external extensions.
- Safe native extension wrapper slice + cookbook: `spargio::extension::fs::{statx, statx_on_shard, statx_or_metadata}` plus `docs/native_extension_cookbook.md`.
- Ergonomic fs/net APIs on top of native I/O: `spargio::fs::{OpenOptions, File}` plus path helpers (`create_dir*`, `rename`, `remove_*`, metadata/link helpers, `read`/`write`), and `spargio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream, UnixDatagram}`.
- Directory traversal + `du` parity helpers: low-level `spargio::extension::fs::read_dir_entries(...)` and high-level `spargio::fs::{read_dir(...), du(...), DuOptions, DuSummary}` with sparse/hardlink/symlink and one-filesystem policy support.
- Measured metadata fast path helper: `spargio::fs::metadata_lite(...)` (`statx`-backed with fallback).
- Native-first fs path-op lane on Linux io_uring for high-value helpers (`create_dir`, `remove_file`, `remove_dir`, `rename`, `hard_link`, `symlink`), with compatibility fallback on unsupported opcode kernels.
- Foundational I/O utility layer: `spargio::io::{AsyncRead, AsyncWrite, split, copy_to_vec, BufReader, BufWriter}` and `io::framed::LengthDelimited`.
- Native setup path on Linux io_uring lane: `open/connect/accept` are nonblocking and routed through native setup ops (no helper-thread `run_blocking` wrappers in public fs/net setup APIs).
- Native timeout path on io_uring lane: `UringNativeAny::sleep(...)` and shard-context `spargio::sleep(...)` route through `IORING_OP_TIMEOUT`.
- Async-first boundary APIs: `call`, `call_with_timeout`, `recv`, `recv_timeout`, and `BoundaryTicket::wait_timeout`.
- Explicit socket-address APIs that bypass DNS resolution: `connect_socket_addr*` and `bind_socket_addr`.
- Benchmark suites: `benches/ping_pong.rs`, `benches/fanout_fanin.rs`, `benches/fs_api.rs` (Tokio/Spargio/Compio), and `benches/net_api.rs` (Tokio/Spargio/Compio).
- Scheduler profiling lane with `callgrind`/`cachegrind`: `scripts/bench_scheduler_profile.sh` and ratio guardrail helper `scripts/scheduler_profile_guardrail.sh`.
- Mixed-runtime boundary API: `spargio::boundary`.
- Companion crate suite: `spargio-process`, `spargio-signal`, `spargio-protocols` (legacy blocking bridge helpers), `spargio-tls` (rustls/futures-rustls adapter), `spargio-ws` (async-tungstenite adapter), and `spargio-quic` with selectable backend mode (`QuicBackend::Native` default dispatch and explicit `QuicBackend::Bridge` compatibility fallback).
- Native-vs-bridge QUIC cutover guardrails: native data path is validated to avoid bridge task spawning, while bridge mode remains explicit compatibility fallback.
- QUIC native default backend now runs on `quinn-proto` driver path (`NativeProtoDriver` + native UDP pump/timers) with stream/datagram operations routed through the driver; bridge mode remains explicit compatibility fallback.
- Companion hardening lane: `scripts/companion_ci_smoke.sh` plus CI `companion-matrix` job.
- QUIC qualification lanes: interop matrix (`scripts/quic_interop_matrix.sh`), soak/fault lane (`scripts/quic_soak_fault.sh`, nightly), and native-vs-bridge perf gate (`scripts/quic_perf_gate.sh`).
- In-repo user-facing `book/` (`mdBook`) covering quick start, task placement (`!Send` + stealable locality-first defaults), I/O API selection, protocol crates, native extensions, performance tuning, operations, migration, and status.
- Reference mixed-mode service example.

## What's Not Done Yet

- Hostname-based `ToSocketAddrs` connect/bind paths can still block for DNS resolution; use explicit `SocketAddr` APIs (`connect_socket_addr*`, `bind_socket_addr`) for strictly non-DNS data-plane paths.
- Remaining fs helper migration to native io_uring where it is not a clear win is deferred: `canonicalize`, `metadata`, `symlink_metadata`, and `set_permissions` currently use compatibility blocking paths (`create_dir_all` is native-first for straightforward paths; `metadata_lite` exists as native-first metadata alternative).
- Work-stealing tuning guidance still needs deeper production case studies and calibration examples on top of the current knob and profiling documentation.
- Work-stealing queue structure is still generic compared with Tokio's specialized scheduler queues; evaluate a better-optimized queue design (owner-fast-path + injection/steal structure) later.
- Continue readability/editorial cleanup across README + `book/`: tighten wording, keep examples minimal but practical, and reduce ambiguous terminology.
- Broaden documentation coverage while refactoring core modules for maintainability: keep API docs/book content aligned as runtime/fs/net surfaces continue to be split into smaller focused units.

## Longer-term Improvement Ideas

- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) is explicitly deprioritized for now (backlog-only, not planned right now).
- Full production-grade higher-level ecosystem parity is still in progress; companion crates now provide practical bridges and qualification lanes, but deeper protocol-specific maturity remains (broader TLS/WS tuning surfaces, richer process stdio orchestration, and deeper long-window failure coverage).
- QUIC backend hardening is still in progress: native default path is driver-backed now, but long-window soak/fault/perf requalification depth and rollout maturity (`rollout_stage`) still need production validation.
- Production hardening beyond smoke lanes: deeper failure-injection/soak coverage, broader observability for companion protocol paths, and long-window p95/p99 gates.
- Further workload-specific work-stealing model calibration is still iterative (the adaptive policy is implemented, but thresholds/weights are expected to continue evolving with production traces).
- Multi-endpoint QUIC sharding/fan-out orchestration is not built in yet: a single `QuicEndpoint` still owns one native transport backend, so multi-core listener scaling is currently a manual multi-endpoint deployment pattern.
- Fully io_uring-submitted directory traversal is still in progress: `read_dir`/`du` APIs are built-in, but (as of 2026-03-03) upstream io_uring userspace/kernel ABIs do not expose a stable `getdents` opcode surface (`IORING_OP_GETDENTS`), so traversal currently uses a blocking-helper lane (`getdents64` with compatibility fallback) instead of pure in-ring submission.

## Contributor Quick Start

```bash
cargo test
cargo test --features uring-native
cargo bench --features uring-native --no-run
cargo test --features macros --test entry_macro_tdd
```

Benchmark helpers:

```bash
./scripts/bench_fanout_smoke.sh
./scripts/bench_ping_guardrail.sh
./scripts/bench_fanout_guardrail.sh
./scripts/bench_kpi_guardrail.sh
./scripts/bench_scheduler_profile.sh
./scripts/bench_scheduler_calibration.sh
./scripts/scheduler_profile_guardrail.sh
./scripts/companion_ci_smoke.sh
./scripts/quic_interop_matrix.sh
./scripts/quic_perf_gate.sh
./scripts/quic_soak_fault.sh
```

Reference app:

```bash
cargo run --example mixed_mode_service
```

## Runtime Entry

Helper-based entry:

```rust
#[tokio::main]
async fn main() -> Result<(), spargio::RuntimeError> {
    spargio::run(|handle| async move {
        let job = handle.spawn_stealable(async { 42usize }).expect("spawn");
        assert_eq!(job.await.expect("join"), 42);
    })
    .await
}
```

Attribute-macro entry (enable with `--features macros`):

```rust
#[spargio::main(shards = 4, backend = "io_uring")]
async fn main() {
    // async body runs on Spargio runtime
}
```

This takes two optional arguments. Without them, `#[spargio::main]` uses sensible defaults: `io_uring` backend and shard count from available CPU parallelism. Use macro arguments only when you need explicit overrides.

## Repository Map

- `src/lib.rs`: runtime implementation.
- `tests/`: TDD coverage.
- `benches/`: Criterion benchmarks.
- `examples/`: mixed-mode reference app.
- `scripts/`: benchmark smoke/guard helpers.
- `.github/workflows/`: CI gates.
- `IMPLEMENTATION_LOG.md`: implementation and benchmark log.
- `architecture_decision_records/`: ADRs.

## Connection Placement Best Practices

- Use `spargio::net::TcpStream::connect(...)` for simple or latency-first paths (few streams, short-lived connections).
- Use `spargio::net::TcpStream::connect_many_round_robin(...)` (or `connect_with_session_policy(..., RoundRobin)`) for sustained multi-stream throughput workloads.
- For per-stream hot I/O loops, pair round-robin stream setup with `stream.spawn_on_session(...)` to keep execution aligned with the stream session shard.
- Use stealable task placement when post-I/O CPU work is dominant and can benefit from migration.
- As a practical starting heuristic: if active stream count is at least `2x` shard count and streams are long-lived, prefer round-robin/distributed mode.

## Engineering Method

Development style is red/green TDD:

1. Add failing tests.
2. Implement minimal passing behavior.
3. Validate with full test and benchmark checks.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE).

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `spargio` by you shall be licensed as MIT, without any
additional terms or conditions.
