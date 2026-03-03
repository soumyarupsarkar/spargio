# spargio

`spargio` is a **work-stealing `io_uring`-based async runtime** for Rust, using `msg_ring` for cross-thread coordination.

Instead of a strict thread-per-core/share-nothing execution model like other `io_uring` runtimes (`glommio`/`monoio`/`compio` and `tokio_uring`), `spargio` uses submission-time steering of stealable tasks across threads (a novel form of work-stealing).

In our benchmarks (detailed below), `spargio` outperforms `compio` (and likely all share-nothing runtimes) in imbalanced or coordination-heavy workloads by up to 80%, and outperforms `tokio` for cases involving high coordination or disk I/O by up to 280%. `compio` leads for sustained, balanced workloads by up to 40%.

Out-of-the-box, we support async disk I/O, network I/O (including TLS/WebSockets/QUIC), process execution, and signal handling, and provide an extension API for additional `io_uring` operations. We support both `tokio`-style stealable tasks and `compio`-style pinned (thread-affine) tasks.

## Disclaimer

`spargio` began as a proof of concept built with Codex to see if the idea is worth pursuing, and remains a work-in-progress. I have not reviewed all the code yet. Treat it as pre-alpha.

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

### Coordination-focused workloads (Tokio vs Spargio)

| Benchmark | Description | Tokio | Spargio | Speedup |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | Two-worker request/ack round-trip loop | `1.4911-1.5024 ms` | `394.83-396.21 us` | `3.8x` |
| `steady_one_way_send_drain` | One-way sends, then explicit drain barrier | `68.607-70.859 us` | `49.232-50.110 us` | `1.4x` |
| `cold_start_ping_pong` | Includes runtime/harness startup and teardown | `553.31-561.83 us` | `284.23-287.50 us` | `2.0x` |
| `fanout_fanin_balanced` | Even fanout/fanin across shards | `1.4534-1.4631 ms` | `1.3426-1.3480 ms` | `1.1x` |
| `fanout_fanin_skewed` | Skewed fanout/fanin with hotspot pressure | `2.4026-2.4220 ms` | `1.9979-2.0032 ms` | `1.2x` |

Compio is not listed in this coordination-only table because it is share-nothing (thread-per-core), while these cases are focused on cross-shard coordination behavior.

### Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `fs_read_rtt_4k` (`qd=1`) | 4 KiB file read latency, depth 1 | `1.6174-1.6565 ms` | `1.0008-1.0188 ms` | `1.4782-1.4978 ms` | `1.6x` | `1.5x` |
| `fs_read_throughput_4k_qd32` | 4 KiB file reads, queue depth 32 | `7.8804-8.1672 ms` | `6.1570-6.2793 ms` | `4.0877-5.0803 ms` | `1.3x` | `0.7x` |
| `net_echo_rtt_256b` (`qd=1`) | 256-byte TCP echo latency, depth 1 | `7.7462-7.9687 ms` | `5.4356-5.5084 ms` | `6.4541-6.5632 ms` | `1.4x` | `1.2x` |
| `net_stream_throughput_4k_window32` | 4 KiB stream throughput, window 32 | `11.142-11.247 ms` | `10.745-10.813 ms` | `7.0631-7.1570 ms` | `1.0x` | `0.7x` |

### Imbalanced Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `net_stream_imbalanced_4k_hot1_light7` | 8 streams, 1 static hot + 7 light, 4 KiB frames | `13.584-13.799 ms` | `13.191-13.375 ms` | `12.283-12.414 ms` | `1.0x` | `0.9x` |
| `net_stream_hotspot_rotation_4k` | 8 streams, rotating hotspot each step, I/O-only | `8.7891-8.8560 ms` | `9.3683-9.4526 ms` | `16.870-16.982 ms` | `0.9x` | `1.8x` |
| `net_pipeline_hotspot_rotation_4k_window32` | 8 streams, rotating hotspot with recv/CPU/send pipeline | `26.415-26.654 ms` | `29.113-29.517 ms` | `50.648-51.210 ms` | `0.9x` | `1.7x` |
| `net_keyed_hotspot_rotation_4k` | 8 streams, rotating hotspot with keyed ownership routing | `9.3152-9.4912 ms` | `9.5691-9.7957 ms` | `16.781-16.994 ms` | `1.0x` | `1.7x` |

## Benchmark Interpretation

TL;DR: As expected, Spargio is strongest on coordination-heavy and low-depth latency workloads; Compio is strongest on sustained balanced stream throughput. Somewhat surprisingly, Tokio remains ahead in some rotating-hotspot network shapes.

- Spargio leads in coordination-heavy cross-shard cases versus Tokio (`steady_ping_pong_rtt`, `steady_one_way_send_drain`, `cold_start_ping_pong`, `fanout_fanin_*`).
- Spargio leads in low-depth fs/net latency (`fs_read_rtt_4k`, `net_echo_rtt_256b`) versus both Tokio and Compio.
- Compio leads in sustained balanced stream throughput and static-hotspot imbalance (`net_stream_throughput_4k_window32`, `net_stream_imbalanced_4k_hot1_light7`), while Spargio is currently ahead of Tokio in both of those cases.
- Tokio currently leads in rotating-hotspot stream/pipeline cases; keyed routing is near parity (`net_stream_hotspot_rotation_4k`, `net_pipeline_hotspot_rotation_4k_window32`, `net_keyed_hotspot_rotation_4k`).

For performance, different workload shapes favor different runtimes.

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
- In-repo long-form docs scaffold: `book/` (`mdBook`) with protocol/API-selection and migration chapters.
- Reference mixed-mode service example.

## What's Not Done Yet

- Full production-grade higher-level ecosystem parity is still in progress; companion crates now provide practical bridges and qualification lanes, but deeper protocol-specific maturity remains (broader TLS/WS tuning surfaces, richer process stdio orchestration, and deeper long-window failure coverage).
- QUIC backend hardening is still in progress: native default path is driver-backed now, but long-window soak/fault/perf requalification depth and rollout maturity (`rollout_stage`) still need production validation.
- Multi-endpoint QUIC sharding/fan-out orchestration is not built in yet: a single `QuicEndpoint` still owns one native transport backend, so multi-core listener scaling is currently a manual multi-endpoint deployment pattern.
- Native directory traversal and full `du` metadata parity are not finished yet: there is no built-in async `getdents`/`read_dir` surface, and `statx` exposure is still a lite subset (for example, allocated-block and hardlink-dedupe oriented fields are not all surfaced yet).
- Hostname-based `ToSocketAddrs` connect/bind paths can still block for DNS resolution; use explicit `SocketAddr` APIs (`connect_socket_addr*`, `bind_socket_addr`) for strictly non-DNS data-plane paths.
- Remaining fs helper migration to native io_uring where it is not a clear win is deferred: `canonicalize`, `metadata`, `symlink_metadata`, and `set_permissions` currently use compatibility blocking paths (`create_dir_all` is native-first for straightforward paths; `metadata_lite` exists as native-first metadata alternative).
- Production hardening beyond smoke lanes: deeper failure-injection/soak coverage, broader observability for companion protocol paths, and long-window p95/p99 gates.
- Further workload-specific work-stealing model calibration is still iterative (the adaptive policy is implemented, but thresholds/weights are expected to continue evolving with production traces).
- Expand `book/` coverage into deeper API-selection, placement, and operations guides.
- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) is explicitly deprioritized for now (backlog-only, not planned right now).

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
