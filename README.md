# spargio

`spargio` is a work-stealing `io_uring`-based async runtime for Rust, using `msg_ring` for cross-shared coordination.

Instead of a strict thread-per-core/share-nothing execution model like other `io_uring` runtimes (`glommio`/`monoio`/`compio` and `tokio_uring`), `spargio` uses submission-time steering of stealable tasks across shards.

For coordination-heavy workloads, that gives a useful lever: work and native I/O can be directed to the shard that is best positioned to execute it.

The benchmarks below demonstrate where this helps. In short, as expected, unlike `compio` and other share-nothing runtimes, `spargio` has more predictable response times under imbalanced loads, and unlike `tokio`, we have nonblocking disk I/O and avoid epoll overhead with `io_uring`. `compio` outperforms for sustained, balanced loads.

## Disclaimer

This began as a proof of concept built with Codex to see if the idea is worth pursuing. I have not reviewed all the code yet, and it's not complete. Do not use it in production.

## Quick start

Pre-requisites: Linux 6.0+ recommended (5.18+ for core io_uring + msg_ring paths)

Add `spargio` as a dependency:
```bash
cargo add spargio --features macros,uring-native
```

Then use it for `io_uring` IO operations and stealable task spawning:
```rust
use spargio::{fs::File, net::TcpListener, RuntimeHandle};

#[spargio::main(backend = "io_uring", shards = 4)]
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

## Benchmark Results

### Coordination-focused workloads (Tokio vs Spargio)

| Benchmark | Description | Tokio | Spargio | Speedup |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | Two-worker request/ack round-trip loop | `1.4509-1.4888 ms` | `357.27-378.34 us` | `4.0x` |
| `steady_one_way_send_drain` | One-way sends, then explicit drain barrier | `70.972-75.645 us` | `66.006-66.811 us` | `1.1x` |
| `cold_start_ping_pong` | Includes runtime/harness startup and teardown | `535.65-601.90 us` | `262.24-291.99 us` | `2.1x` |
| `fanout_fanin_balanced` | Even fanout/fanin across shards | `1.4625-1.5346 ms` | `1.3333-1.3496 ms` | `1.1x` |
| `fanout_fanin_skewed` | Skewed fanout/fanin with hotspot pressure | `2.4001-2.7005 ms` | `1.9590-1.9900 ms` | `1.3x` |

Compio is not listed in this coordination-only table because it is share-nothing (thread-per-core), while these cases are focused on cross-shard coordination behavior.

### Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `fs_read_rtt_4k` (`qd=1`) | 4 KiB file read latency, depth 1 | `1.6476-1.7647 ms` | `0.99148-1.0145 ms` | `1.3893-1.4970 ms` | `1.7x` | `1.4x` |
| `fs_read_throughput_4k_qd32` | 4 KiB file reads, queue depth 32 | `7.4895-7.6145 ms` | `5.9790-6.4699 ms` | `5.4749-5.8905 ms` | `1.2x` | `0.9x` |
| `net_echo_rtt_256b` (`qd=1`) | 256-byte TCP echo latency, depth 1 | `7.7059-8.0959 ms` | `5.3708-5.6477 ms` | `6.4743-6.7640 ms` | `1.4x` | `1.2x` |
| `net_stream_throughput_4k_window32` | 4 KiB stream throughput, window 32 | `11.163-11.324 ms` | `10.668-10.719 ms` | `7.2779-7.4795 ms` | `1.1x` | `0.7x` |

### Imbalanced Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `net_stream_imbalanced_4k_hot1_light7` | 8 streams, 1 static hot + 7 light, 4 KiB frames | `13.426-14.098 ms` | `13.510-13.911 ms` | `12.221-12.479 ms` | `1.0x` | `0.9x` |
| `net_stream_hotspot_rotation_4k` | 8 streams, rotating hotspot each step, I/O-only | `8.6480-8.7488 ms` | `11.285-11.811 ms` | `16.346-16.702 ms` | `0.8x` | `1.4x` |
| `net_pipeline_hotspot_rotation_4k_window32` | 8 streams, rotating hotspot with recv/CPU/send pipeline | `26.383-26.937 ms` | `34.962-35.935 ms` | `50.764-51.179 ms` | `0.8x` | `1.4x` |

## Benchmark Interpretation

TL;DR: As expected, Spargio is strongest on coordination-heavy and low-depth latency workloads; Compio is strongest on sustained balanced stream throughput. Somewhat surprisingly, Tokio is currently better optimized for some rotating-hotspot network shapes.

- Spargio leads in coordination-heavy cross-shard cases versus Tokio (`steady_ping_pong_rtt`, `steady_one_way_send_drain`, `cold_start_ping_pong`, `fanout_fanin_*`).
- Spargio leads in low-depth fs/net latency (`fs_read_rtt_4k`, `net_echo_rtt_256b`) versus both Tokio and Compio.
- Compio leads in sustained balanced stream throughput and static-hotspot imbalance (`net_stream_throughput_4k_window32`, `net_stream_imbalanced_4k_hot1_light7`), with Spargio now ahead of Tokio in the balanced throughput case.
- Tokio currently leads in some rotating-hotspot network cases in this suite (`net_stream_hotspot_rotation_4k`, `net_pipeline_hotspot_rotation_4k_window32`).

For performance, different workload shapes favor different runtimes.

## What's Done

- Sharded runtime with Linux `IoUring` backend.
- Cross-shard typed/raw messaging, nowait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Work-stealing scheduler MVP with backpressure and runtime stats.
- Runtime primitives: `sleep`, `timeout`, `CancellationToken`, and `TaskGroup` cooperative cancellation.
- Runtime entry ergonomics: `spargio::run(...)`, `spargio::run_with(builder, ...)`, and optional `#[spargio::main(...)]` via `macros`.
- Unbound native API: `RuntimeHandle::uring_native_unbound() -> UringNativeAny` with file ops (`read_at`, `read_at_into`, `write_at`, `fsync`) and stream/socket ops (`recv`, `send`, `send_owned`, `recv_owned`, `send_all_batch`, `recv_multishot_segments`), plus submission-time shard selector, FD affinity leases, and active op route tracking.
- Ergonomic fs/net APIs on top of native I/O: `spargio::fs::{OpenOptions, File}` and `spargio::net::{TcpListener, TcpStream}`.
- Native setup path on Linux io_uring lane: `open/connect/accept` are nonblocking and routed through native setup ops (no helper-thread `run_blocking` wrappers in public fs/net setup APIs).
- Benchmark suites: `benches/ping_pong.rs`, `benches/fanout_fanin.rs`, `benches/fs_api.rs` (Tokio/Spargio/Compio), and `benches/net_api.rs` (Tokio/Spargio/Compio).
- Mixed-runtime boundary API: `spargio::boundary`.
- Reference mixed-mode service example.

## What's Not Done Yet

- Broader filesystem and network native-op surface (beyond current MVP set, including richer open/metadata/fs management and broader socket operations).
- Production hardening: stress/soak/failure injection, deeper observability, and long-window p95/p99 gates.
- Advanced work-stealing policy tuning beyond current MVP heuristics.
- Deeper documentation (`spargio` book / guides for API selection, placement strategy, and benchmark methodology).
- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) as a separate large-investment track.

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
```

Reference app:

```bash
cargo run --example mixed_mode_service
```

## Runtime Entry

Helper-based entry:

```rust
fn main() -> Result<(), spargio::RuntimeError> {
    spargio::run(|handle| async move {
        let job = handle.spawn_stealable(async { 42usize }).expect("spawn");
        assert_eq!(job.await.expect("join"), 42);
    })
}
```

Attribute-macro entry (enable with `--features macros`):

```rust
#[spargio::main(shards = 4, backend = "io_uring")]
async fn main() {
    // async body runs on Spargio runtime
}
```

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
