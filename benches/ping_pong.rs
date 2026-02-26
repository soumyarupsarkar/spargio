use criterion::{Criterion, black_box, criterion_group, criterion_main};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use futures::executor::block_on;
use spargio::{BackendKind, Event, Runtime, ShardCtx};
use std::sync::mpsc as std_mpsc;
use std::thread;

const PING_TAG: u16 = 1;
const ACK_TAG: u16 = 2;
const ONE_WAY_TAG: u16 = 3;
const FLUSH_TAG: u16 = 4;
const FLUSH_ACK_TAG: u16 = 5;
const SHUTDOWN_TAG: u16 = 9;

const RTT_ROUNDS: usize = 256;
const ONE_WAY_ROUNDS: usize = 2048;
const COLD_ROUNDS: usize = 64;
const TOKIO_BATCH_SIZE: usize = 64;

enum MsgRingCmd {
    PingPong {
        rounds: usize,
        reply: oneshot::Sender<u64>,
    },
    OneWay {
        rounds: usize,
        reply: oneshot::Sender<u64>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct MsgRingHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<MsgRingCmd>,
    client_join: Option<spargio::JoinHandle<()>>,
    responder_join: Option<spargio::JoinHandle<()>>,
}

impl MsgRingHarness {
    fn new(backend: BackendKind) -> Option<Self> {
        Self::new_with_ring_entries(backend, None)
    }

    fn new_with_ring_entries(backend: BackendKind, ring_entries: Option<u32>) -> Option<Self> {
        let mut builder = Runtime::builder().backend(backend).shards(2);
        if let Some(entries) = ring_entries {
            builder = builder.ring_entries(entries);
        }
        let runtime = builder.build().ok()?;
        let backend_kind = backend;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<MsgRingCmd>();

        let responder_join = runtime
            .spawn_on(1, async move {
                let peer = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.remote(0).expect("peer")
                };
                let mut one_way_acc = 0u32;

                loop {
                    let event = {
                        let ctx = ShardCtx::current().expect("on shard");
                        ctx.next_event()
                    }
                    .await;
                    let Event::RingMsg { tag, val, .. } = event;

                    match tag {
                        PING_TAG => {
                            peer.send_raw(ACK_TAG, val)
                                .expect("send ack")
                                .await
                                .expect("ack");
                        }
                        ONE_WAY_TAG => {
                            one_way_acc = one_way_acc.wrapping_add(val);
                        }
                        FLUSH_TAG => {
                            peer.send_raw(FLUSH_ACK_TAG, one_way_acc)
                                .expect("send flush ack")
                                .await
                                .expect("flush ack");
                            one_way_acc = 0;
                        }
                        SHUTDOWN_TAG => break,
                        _ => {}
                    }
                }
            })
            .ok()?;

        let client_join = runtime
            .spawn_on(0, async move {
                let peer = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.remote(1).expect("peer")
                };

                while let Some(cmd) = cmd_rx.next().await {
                    match cmd {
                        MsgRingCmd::PingPong { rounds, reply } => {
                            let mut checksum = 0u64;
                            for i in 0..(rounds as u32) {
                                peer.send_raw(PING_TAG, i)
                                    .expect("send ping")
                                    .await
                                    .expect("ping");

                                loop {
                                    let event = {
                                        let ctx = ShardCtx::current().expect("on shard");
                                        ctx.next_event()
                                    }
                                    .await;
                                    let Event::RingMsg { tag, val, .. } = event;
                                    if tag == ACK_TAG {
                                        checksum += u64::from(val);
                                        break;
                                    }
                                }
                            }
                            let _ = reply.send(checksum);
                        }
                        MsgRingCmd::OneWay { rounds, reply } => {
                            if backend_kind == BackendKind::IoUring {
                                peer.send_many_raw_nowait(
                                    (0..(rounds as u32)).map(|i| (ONE_WAY_TAG, i)),
                                )
                                .expect("send one-way batch");
                                peer.flush()
                                    .expect("flush nowait sends")
                                    .await
                                    .expect("flush");
                            } else {
                                for i in 0..(rounds as u32) {
                                    peer.send_raw_nowait(ONE_WAY_TAG, i).expect("send one-way");
                                }
                            }
                            peer.send_raw(FLUSH_TAG, rounds as u32)
                                .expect("send flush")
                                .await
                                .expect("flush");

                            let sum = loop {
                                let event = {
                                    let ctx = ShardCtx::current().expect("on shard");
                                    ctx.next_event()
                                }
                                .await;
                                let Event::RingMsg { tag, val, .. } = event;
                                if tag == FLUSH_ACK_TAG {
                                    break u64::from(val);
                                }
                            };
                            let _ = reply.send(sum);
                        }
                        MsgRingCmd::Shutdown { reply } => {
                            if let Ok(ticket) = peer.send_raw(SHUTDOWN_TAG, 0) {
                                let _ = ticket.await;
                            }
                            let _ = reply.send(());
                            break;
                        }
                    }
                }
            })
            .ok()?;

        Some(Self {
            runtime,
            cmd_tx,
            client_join: Some(client_join),
            responder_join: Some(responder_join),
        })
    }

    fn ping_pong(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(MsgRingCmd::PingPong { rounds, reply: tx })
            .expect("send command");
        block_on(rx).expect("reply")
    }

    fn one_way(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(MsgRingCmd::OneWay { rounds, reply: tx })
            .expect("send command");
        block_on(rx).expect("reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(MsgRingCmd::Shutdown { reply: tx });
        let _ = block_on(rx);

        if let Some(join) = self.client_join.take() {
            let _ = block_on(join);
        }
        if let Some(join) = self.responder_join.take() {
            let _ = block_on(join);
        }
    }
}

impl Drop for MsgRingHarness {
    fn drop(&mut self) {
        self.shutdown();
        let _ = &self.runtime;
    }
}

enum TokioWire {
    Ping(u32),
    OneWay(u32),
    OneWayBatch(Vec<u32>),
    Flush,
    Shutdown,
}

enum TokioAck {
    Ping(u32),
    Flush(u32),
}

enum TokioCmd {
    PingPong {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    OneWay {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    OneWayBatched {
        rounds: usize,
        batch: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

struct TokioHarness {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TokioCmd>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TokioHarness {
    fn new() -> Self {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TokioCmd>();

        let thread = thread::Builder::new()
            .name("bench-tokio".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    let (wire_tx, mut wire_rx) =
                        tokio::sync::mpsc::unbounded_channel::<TokioWire>();
                    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<TokioAck>();

                    let responder = tokio::spawn(async move {
                        let mut one_way_acc = 0u32;
                        while let Some(msg) = wire_rx.recv().await {
                            match msg {
                                TokioWire::Ping(v) => {
                                    if ack_tx.send(TokioAck::Ping(v)).is_err() {
                                        break;
                                    }
                                }
                                TokioWire::OneWay(v) => {
                                    one_way_acc = one_way_acc.wrapping_add(v);
                                }
                                TokioWire::OneWayBatch(batch) => {
                                    for v in batch {
                                        one_way_acc = one_way_acc.wrapping_add(v);
                                    }
                                }
                                TokioWire::Flush => {
                                    if ack_tx.send(TokioAck::Flush(one_way_acc)).is_err() {
                                        break;
                                    }
                                    one_way_acc = 0;
                                }
                                TokioWire::Shutdown => break,
                            }
                        }
                    });

                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioCmd::PingPong { rounds, reply } => {
                                let mut checksum = 0u64;
                                for i in 0..(rounds as u32) {
                                    wire_tx.send(TokioWire::Ping(i)).expect("wire ping");
                                    loop {
                                        match ack_rx.recv().await {
                                            Some(TokioAck::Ping(v)) => {
                                                checksum += u64::from(v);
                                                break;
                                            }
                                            Some(TokioAck::Flush(_)) => {}
                                            None => break,
                                        }
                                    }
                                }
                                let _ = reply.send(checksum);
                            }
                            TokioCmd::OneWay { rounds, reply } => {
                                for i in 0..(rounds as u32) {
                                    wire_tx.send(TokioWire::OneWay(i)).expect("wire one-way");
                                }
                                wire_tx.send(TokioWire::Flush).expect("wire flush");
                                let sum = loop {
                                    match ack_rx.recv().await {
                                        Some(TokioAck::Flush(v)) => {
                                            break u64::from(v);
                                        }
                                        Some(TokioAck::Ping(_)) => {}
                                        None => break 0,
                                    }
                                };
                                let _ = reply.send(sum);
                            }
                            TokioCmd::OneWayBatched {
                                rounds,
                                batch,
                                reply,
                            } => {
                                let batch = batch.max(1);
                                let mut chunk = Vec::with_capacity(batch.min(rounds.max(1)));
                                for i in 0..(rounds as u32) {
                                    chunk.push(i);
                                    if chunk.len() == batch {
                                        wire_tx
                                            .send(TokioWire::OneWayBatch(std::mem::take(
                                                &mut chunk,
                                            )))
                                            .expect("wire one-way batch");
                                    }
                                }
                                if !chunk.is_empty() {
                                    wire_tx
                                        .send(TokioWire::OneWayBatch(chunk))
                                        .expect("wire one-way final batch");
                                }

                                wire_tx.send(TokioWire::Flush).expect("wire flush");
                                let sum = loop {
                                    match ack_rx.recv().await {
                                        Some(TokioAck::Flush(v)) => {
                                            break u64::from(v);
                                        }
                                        Some(TokioAck::Ping(_)) => {}
                                        None => break 0,
                                    }
                                };
                                let _ = reply.send(sum);
                            }
                            TokioCmd::Shutdown { reply } => {
                                let _ = wire_tx.send(TokioWire::Shutdown);
                                let _ = responder.await;
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn tokio bench thread");

        Self {
            cmd_tx,
            thread: Some(thread),
        }
    }

    fn ping_pong(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioCmd::PingPong { rounds, reply: tx })
            .expect("send command");
        rx.recv().expect("recv reply")
    }

    fn one_way(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioCmd::OneWay { rounds, reply: tx })
            .expect("send command");
        rx.recv().expect("recv reply")
    }

    fn one_way_batched(&mut self, rounds: usize, batch: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioCmd::OneWayBatched {
                rounds,
                batch,
                reply: tx,
            })
            .expect("send command");
        rx.recv().expect("recv reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(TokioCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
    }
}

impl Drop for TokioHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_spargio_cold(rounds: usize, backend: BackendKind) {
    let mut harness = MsgRingHarness::new(backend).expect("runtime harness");
    black_box(harness.ping_pong(rounds));
}

fn run_tokio_cold(rounds: usize) {
    let mut harness = TokioHarness::new();
    black_box(harness.ping_pong(rounds));
}

fn bench_steady_ping_pong(c: &mut Criterion) {
    let mut group = c.benchmark_group("steady_ping_pong_rtt");

    let mut queue = MsgRingHarness::new(BackendKind::Queue).expect("queue harness");
    black_box(queue.ping_pong(16));
    group.bench_function("spargio_queue", |b| {
        b.iter(|| black_box(queue.ping_pong(RTT_ROUNDS)))
    });

    #[cfg(target_os = "linux")]
    if let Some(mut uring) = MsgRingHarness::new(BackendKind::IoUring) {
        black_box(uring.ping_pong(16));
        group.bench_function("spargio_io_uring", |b| {
            b.iter(|| black_box(uring.ping_pong(RTT_ROUNDS)))
        });
    }

    let mut tokio = TokioHarness::new();
    black_box(tokio.ping_pong(16));
    group.bench_function("tokio_two_worker", |b| {
        b.iter(|| black_box(tokio.ping_pong(RTT_ROUNDS)))
    });

    group.finish();
}

fn bench_steady_one_way(c: &mut Criterion) {
    let mut group = c.benchmark_group("steady_one_way_send_drain");

    let mut queue = MsgRingHarness::new(BackendKind::Queue).expect("queue harness");
    black_box(queue.one_way(64));
    group.bench_function("spargio_queue", |b| {
        b.iter(|| black_box(queue.one_way(ONE_WAY_ROUNDS)))
    });

    #[cfg(target_os = "linux")]
    if let Some(mut uring) = MsgRingHarness::new_with_ring_entries(BackendKind::IoUring, Some(4096))
    {
        black_box(uring.one_way(64));
        group.bench_function("spargio_io_uring", |b| {
            b.iter(|| black_box(uring.one_way(ONE_WAY_ROUNDS)))
        });
    }

    let mut tokio = TokioHarness::new();
    black_box(tokio.one_way(64));
    group.bench_function("tokio_two_worker", |b| {
        b.iter(|| black_box(tokio.one_way(ONE_WAY_ROUNDS)))
    });

    let mut tokio_batched = TokioHarness::new();
    black_box(tokio_batched.one_way_batched(64, TOKIO_BATCH_SIZE));
    group.bench_function("tokio_two_worker_batched_64", |b| {
        b.iter(|| black_box(tokio_batched.one_way_batched(ONE_WAY_ROUNDS, TOKIO_BATCH_SIZE)))
    });

    let mut tokio_batched_all = TokioHarness::new();
    black_box(tokio_batched_all.one_way_batched(64, 64));
    group.bench_function("tokio_two_worker_batched_all", |b| {
        b.iter(|| black_box(tokio_batched_all.one_way_batched(ONE_WAY_ROUNDS, ONE_WAY_ROUNDS)))
    });

    group.finish();
}

fn bench_cold_start_ping_pong(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_start_ping_pong");

    group.bench_function("spargio_queue", |b| {
        b.iter(|| run_spargio_cold(COLD_ROUNDS, BackendKind::Queue))
    });

    #[cfg(target_os = "linux")]
    if MsgRingHarness::new(BackendKind::IoUring).is_some() {
        group.bench_function("spargio_io_uring", |b| {
            b.iter(|| run_spargio_cold(COLD_ROUNDS, BackendKind::IoUring))
        });
    }

    group.bench_function("tokio_two_worker", |b| {
        b.iter(|| run_tokio_cold(COLD_ROUNDS))
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_steady_ping_pong,
    bench_steady_one_way,
    bench_cold_start_ping_pong
);
criterion_main!(benches);
