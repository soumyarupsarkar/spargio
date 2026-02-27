# spargio

`spargio` is a work-stealing async runtime for Rust built around `io_uring` and `msg_ring`.

Instead of a strict thread-per-core/share-nothing execution model like other `io_uring` runtimes (`glommio`/`monoio`/`compio` and `tokio_uring`), Spargio uses submission-time steering of stealable tasks across shards.

For coordination-heavy workloads, that gives a useful lever: work and native I/O can be directed to the shard that is best positioned to execute it.

The benchmarks below demonstrate where this helps. In short, as expected, unlike compio and other share-nothing runtimes, `spargio` has more predictable response times under imbalanced loads, and unlike `tokio`, we have nonblocking disk I/O and avoid epoll overhead with `io_uring`. `compio` outperforms for sustained, balanced loads.

## Disclaimer

This began as a proof of concept built with Codex to see if the idea is worth pursuing. I have not reviewed all the code yet, and it's not complete. Do not use it in production.

## Benchmark Results

### Coordination-focused workloads (Tokio vs Spargio)

| Benchmark | Description | Tokio | Spargio | Speedup |
| --- | --- | --- | --- | --- |
| `steady_ping_pong_rtt` | Two-worker request/ack round-trip loop | `1.434-1.553 ms` | `366-381 us` | `4.0x` |
| `steady_one_way_send_drain` | One-way sends, then explicit drain barrier | `68.8-76.2 us` | `70.5-71.7 us` | `1.0x` |
| `cold_start_ping_pong` | Includes runtime/harness startup and teardown | `561-610 us` | `249-267 us` | `2.3x` |
| `fanout_fanin_balanced` | Even fanout/fanin across shards | `1.473-1.621 ms` | `1.387-1.404 ms` | `1.1x` |
| `fanout_fanin_skewed` | Skewed fanout/fanin with hotspot pressure | `2.366-2.437 ms` | `1.993-2.003 ms` | `1.2x` |

Compio is not listed in this coordination-only table because it is share-nothing (thread-per-core), while these cases are focused on cross-shard coordination behavior.

### Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `fs_read_rtt_4k` (`qd=1`) | 4 KiB file read latency, depth 1 | `1.601-1.641 ms` | `1.012-1.026 ms` | `1.388-1.421 ms` | `1.6x` | `1.4x` |
| `fs_read_throughput_4k_qd32` | 4 KiB file reads, queue depth 32 | `7.680-7.767 ms` | `5.971-6.054 ms` | `5.983-6.119 ms` | `1.3x` | `1.0x` |
| `net_echo_rtt_256b` (`qd=1`) | 256-byte TCP echo latency, depth 1 | `7.923-8.118 ms` | `5.410-5.516 ms` | `6.447-6.530 ms` | `1.5x` | `1.2x` |
| `net_stream_throughput_4k_window32` | 4 KiB stream throughput, window 32 | `10.902-11.155 ms` | `11.225-11.441 ms` | `7.007-7.118 ms` | `1.0x` | `0.6x` |

### Imbalanced Native API workloads (Tokio vs Spargio vs Compio)

| Benchmark | Description | Tokio | Spargio | Compio | Spargio vs Tokio | Spargio vs Compio |
| --- | --- | --- | --- | --- | --- | --- |
| `net_stream_imbalanced_4k_hot1_light7` | 8 streams, 1 static hot + 7 light, 4 KiB frames | `14.058-14.331 ms` | `13.300-13.734 ms` | `12.174-12.499 ms` | `1.1x` | `0.9x` |
| `net_stream_hotspot_rotation_4k` | 8 streams, rotating hotspot each step, I/O-only | `8.7249-8.7700 ms` | `11.499-11.600 ms` | `16.637-16.766 ms` | `0.8x` | `1.4x` |
| `net_pipeline_hotspot_rotation_4k_window32` | 8 streams, rotating hotspot with recv/CPU/send pipeline | `26.075-26.308 ms` | `32.686-33.156 ms` | `50.496-51.812 ms` | `0.8x` | `1.5x` |

## Why Spargio Is Faster

- Cross-shard coordination uses `IORING_OP_MSG_RING` directly, which keeps hot signaling paths inside ring-to-ring messaging.
- Submission-time steering and stealable execution help absorb shard imbalance in fanout/fanin-style workloads.
- Unbound native I/O (`UringNativeAny`) allows dispatch decisions at submission time rather than pinning all work to a pre-bound lane.
- Throughput paths now use owned-buffer reuse plus `send_all_batch`/`recv_multishot_segments`, reducing per-frame async/syscall/completion overhead.

In pure sustained stream-throughput cases, thread-per-core runtimes such as Compio can still outperform Spargio today.

## Done

- Sharded runtime with `Queue` and Linux `IoUring` backends.
- Cross-shard typed/raw messaging, nowait sends, batching, and flush tickets.
- Placement APIs: `Pinned`, `RoundRobin`, `Sticky`, `Stealable`, `StealablePreferred`.
- Work-stealing scheduler MVP with backpressure and runtime stats.
- Runtime primitives: `sleep`, `timeout`, `CancellationToken`, and `TaskGroup` cooperative cancellation.
- Runtime entry ergonomics:
  - `spargio::run(...)`
  - `spargio::run_with(builder, ...)`
  - optional `#[spargio::main(...)]` attribute via `macros` feature
- Unbound native API:
  - `RuntimeHandle::uring_native_unbound() -> UringNativeAny`
  - file-style ops (`read_at`, `read_at_into`, `write_at`, `fsync`)
  - stream/socket ops (`recv`, `send`, `send_owned`, `recv_owned`, `send_all_batch`, `recv_multishot_segments`)
  - submission-time shard selector + FD affinity leases + active op route tracking.
- Ergonomic APIs on top of unbound native I/O:
  - `spargio::fs::{OpenOptions, File}`
  - `spargio::net::{TcpListener, TcpStream}`
- Benchmark suites:
  - `benches/ping_pong.rs`
  - `benches/fanout_fanin.rs`
  - `benches/fs_api.rs` (Tokio/Spargio/Compio)
  - `benches/net_api.rs` (Tokio/Spargio/Compio)
- Mixed-runtime boundary API: `spargio::boundary`.
- Reference mixed-mode service example.

## Not Done Yet

- Full native open/accept/connect path on io_uring opcodes (current ergonomic wrappers use blocking helper threads for those setup steps).
- Broader filesystem and network native-op surface (beyond current MVP read/write/send/recv set).
- Production hardening: stress/soak/failure injection, deeper observability, and long-window p95/p99 gates.
- Advanced work-stealing policy tuning beyond current MVP heuristics.
- Optional Tokio-compat readiness emulation shim (`IORING_OP_POLL_ADD`) as a separate large-investment track.

## Quick Start

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
#[spargio::main(shards = 4, backend = "queue")]
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

## Tokio Integration

Recommended model today:

- Run Tokio and Spargio side-by-side.
- Exchange work/results through explicit boundaries (`spargio::boundary`, channels, adapters).
- Move selected hot paths into Spargio without forcing full dependency migration.

A Tokio-compat readiness shim based on `IORING_OP_POLL_ADD` is possible, but building a dependency-transparent drop-in lane is a large investment.

## Connection Placement Best Practices

- Use `spargio::net::TcpStream::connect(...)` for simple or latency-first paths (few streams, short-lived connections).
- Use `spargio::net::TcpStream::connect_many_round_robin(...)` (or `connect_with_session_policy(..., RoundRobin)`) for sustained multi-stream throughput workloads.
- For per-stream hot I/O loops, pair round-robin stream setup with `stream.spawn_on_session(...)` to keep execution aligned with the stream session shard.
- Use stealable task placement when post-I/O CPU work is dominant and can benefit from migration.
- As a practical starting heuristic: if active stream count is at least `2x` shard count and streams are long-lived, prefer round-robin/distributed mode.

## Inspirations and Further Reading

Using `msg_ring` for coordination is heavily inspired by [`ourio`](https://github.com/rockorager/ourio). We extend that idea to work-stealing.

Asking whether to build a work-stealing pool using `io_uring` at all was inspired by the following (excellent) blog posts:
- https://emschwartz.me/async-rust-can-be-a-pleasure-to-work-with-without-send-sync-static/
- https://without.boats/blog/thread-per-core/

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
