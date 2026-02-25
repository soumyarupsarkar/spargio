use criterion::{Criterion, black_box, criterion_group, criterion_main};
use futures::executor::block_on;
use msg_ring_runtime::{BackendKind, Event, Runtime, ShardCtx};

fn run_msg_ring_runtime(rounds: usize, backend: BackendKind) {
    let rt = Runtime::builder()
        .backend(backend)
        .shards(2)
        .build()
        .expect("runtime");

    let responder = rt
        .spawn_on(1, async move {
            let peer = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(0).expect("peer")
            };

            for _ in 0..rounds {
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                }
                .await;

                let Event::RingMsg { tag, val, .. } = event;
                if tag == 1 {
                    peer.send_raw(2, val).expect("queue").await.expect("send");
                }
            }
        })
        .expect("spawn responder");

    let client = rt
        .spawn_on(0, async move {
            let peer = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("peer")
            };

            let mut checksum = 0u64;
            for i in 0..(rounds as u32) {
                peer.send_raw(1, i).expect("queue").await.expect("send");
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                }
                .await;
                let Event::RingMsg { tag, val, .. } = event;
                if tag == 2 {
                    checksum += u64::from(val);
                }
            }
            checksum
        })
        .expect("spawn client");

    let checksum = block_on(client).expect("join client");
    block_on(responder).expect("join responder");
    black_box(checksum);
}

fn run_tokio_ping_pong(rounds: usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let (tx_req, mut rx_req) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let (tx_ack, mut rx_ack) = tokio::sync::mpsc::unbounded_channel::<u32>();

        let responder = tokio::spawn(async move {
            while let Some(v) = rx_req.recv().await {
                if tx_ack.send(v).is_err() {
                    break;
                }
            }
        });

        let mut checksum = 0u64;
        for i in 0..(rounds as u32) {
            tx_req.send(i).expect("send req");
            let got = rx_ack.recv().await.expect("recv ack");
            checksum += u64::from(got);
        }
        drop(tx_req);
        responder.await.expect("responder task");
        black_box(checksum);
    });
}

#[cfg(all(target_os = "linux", feature = "glommio-bench"))]
fn run_glommio_ping_pong(rounds: usize) {
    use futures::SinkExt;
    use futures::StreamExt;
    use futures::channel::mpsc;
    use glommio::{LocalExecutorBuilder, Placement};

    let executor = LocalExecutorBuilder::new(Placement::Unbound)
        .make()
        .expect("glommio executor");

    executor.run(async move {
        let (mut tx_req, mut rx_req) = mpsc::unbounded::<u32>();
        let (mut tx_ack, mut rx_ack) = mpsc::unbounded::<u32>();

        let responder = glommio::spawn_local(async move {
            while let Some(v) = rx_req.next().await {
                if tx_ack.send(v).await.is_err() {
                    break;
                }
            }
        });

        let mut checksum = 0u64;
        for i in 0..(rounds as u32) {
            tx_req.send(i).await.expect("send req");
            let got = rx_ack.next().await.expect("recv ack");
            checksum += u64::from(got);
        }
        drop(tx_req);
        let _ = responder.await;
        black_box(checksum);
    });
}

fn bench_ping_pong(c: &mut Criterion) {
    let rounds = black_box(256usize);
    let mut group = c.benchmark_group("ping_pong");

    group.bench_function("msg_ring_runtime_queue", |b| {
        b.iter(|| run_msg_ring_runtime(rounds, BackendKind::Queue))
    });

    #[cfg(target_os = "linux")]
    if Runtime::builder()
        .backend(BackendKind::IoUring)
        .shards(1)
        .build()
        .is_ok()
    {
        group.bench_function("msg_ring_runtime_io_uring", |b| {
            b.iter(|| run_msg_ring_runtime(rounds, BackendKind::IoUring))
        });
    }

    group.bench_function("tokio_unbounded_channel", |b| {
        b.iter(|| run_tokio_ping_pong(rounds))
    });

    #[cfg(all(target_os = "linux", feature = "glommio-bench"))]
    group.bench_function("glommio_simple", |b| {
        b.iter(|| run_glommio_ping_pong(rounds))
    });

    group.finish();
}

criterion_group!(benches, bench_ping_pong);
criterion_main!(benches);
