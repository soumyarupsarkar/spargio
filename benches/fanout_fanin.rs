use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use futures::executor::block_on;
use spargio::{BackendKind, Runtime};
use std::sync::mpsc as std_mpsc;
use std::thread;

const WORKER_THREADS: usize = 4;
const FANOUT_WIDTH: usize = 16;
const REQUESTS_PER_ITER: usize = 64;
const WARMUP_REQUESTS: usize = 8;
const BALANCED_ITERS: u32 = 2_000;
const SKEW_LIGHT_ITERS: u32 = 500;
const SKEW_HEAVY_ITERS: u32 = 20_000;

#[derive(Clone, Copy)]
struct Workload {
    requests: usize,
    fanout: usize,
    light_iters: u32,
    heavy_iters: u32,
    skew: bool,
}

impl Workload {
    fn balanced() -> Self {
        Self {
            requests: REQUESTS_PER_ITER,
            fanout: FANOUT_WIDTH,
            light_iters: BALANCED_ITERS,
            heavy_iters: BALANCED_ITERS,
            skew: false,
        }
    }

    fn skewed() -> Self {
        Self {
            requests: REQUESTS_PER_ITER,
            fanout: FANOUT_WIDTH,
            light_iters: SKEW_LIGHT_ITERS,
            heavy_iters: SKEW_HEAVY_ITERS,
            skew: true,
        }
    }

    fn warmup(self) -> Self {
        Self {
            requests: WARMUP_REQUESTS,
            ..self
        }
    }

    fn branch_iters(self, request_idx: usize, branch_idx: usize) -> u32 {
        if self.skew && branch_idx == (request_idx % self.fanout) {
            self.heavy_iters
        } else {
            self.light_iters
        }
    }
}

fn synthetic_work(seed: u64, iterations: u32) -> u64 {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    for i in 0..iterations {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407 ^ u64::from(i));
        x ^= x >> 29;
    }
    x
}

enum SpargioCmd {
    Run {
        workload: Workload,
        reply: oneshot::Sender<u64>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct SpargioHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<SpargioCmd>,
    worker_join: Option<spargio::JoinHandle<()>>,
}

impl SpargioHarness {
    fn new(backend: BackendKind) -> Option<Self> {
        let runtime = Runtime::builder()
            .backend(backend)
            .shards(WORKER_THREADS)
            .build()
            .ok()?;
        let handle = runtime.handle();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SpargioCmd>();

        let worker_join = runtime
            .spawn_on(0, async move {
                while let Some(cmd) = cmd_rx.next().await {
                    match cmd {
                        SpargioCmd::Run { workload, reply } => {
                            let mut checksum = 0u64;
                            for req in 0..workload.requests {
                                let mut joins = Vec::with_capacity(workload.fanout);
                                for branch in 0..workload.fanout {
                                    let iters = workload.branch_iters(req, branch);
                                    let seed = ((req as u64) << 32) ^ branch as u64;
                                    let join = handle
                                        .spawn_stealable(async move { synthetic_work(seed, iters) })
                                        .expect("spawn stealable");
                                    joins.push(join);
                                }

                                for join in joins {
                                    checksum = checksum.wrapping_add(join.await.expect("join"));
                                }
                            }
                            let _ = reply.send(checksum);
                        }
                        SpargioCmd::Shutdown { reply } => {
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
            worker_join: Some(worker_join),
        })
    }

    fn run(&mut self, workload: Workload) -> u64 {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(SpargioCmd::Run {
                workload,
                reply: tx,
            })
            .expect("send run cmd");
        block_on(rx).expect("run reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(SpargioCmd::Shutdown { reply: tx });
        let _ = block_on(rx);
        if let Some(join) = self.worker_join.take() {
            let _ = block_on(join);
        }
    }
}

impl Drop for SpargioHarness {
    fn drop(&mut self) {
        self.shutdown();
        let _ = &self.runtime;
    }
}

enum TokioCmd {
    Run {
        workload: Workload,
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
            .name("bench-tokio-fanout".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(WORKER_THREADS)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioCmd::Run { workload, reply } => {
                                let mut checksum = 0u64;
                                for req in 0..workload.requests {
                                    let mut joins = Vec::with_capacity(workload.fanout);
                                    for branch in 0..workload.fanout {
                                        let iters = workload.branch_iters(req, branch);
                                        let seed = ((req as u64) << 32) ^ branch as u64;
                                        joins.push(tokio::spawn(async move {
                                            synthetic_work(seed, iters)
                                        }));
                                    }

                                    for join in joins {
                                        checksum = checksum.wrapping_add(join.await.expect("join"));
                                    }
                                }
                                let _ = reply.send(checksum);
                            }
                            TokioCmd::Shutdown { reply } => {
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

    fn run(&mut self, workload: Workload) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioCmd::Run {
                workload,
                reply: tx,
            })
            .expect("send run cmd");
        rx.recv().expect("recv run reply")
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

fn bench_balanced(c: &mut Criterion) {
    let workload = Workload::balanced();
    let mut group = c.benchmark_group("fanout_fanin_balanced");
    group.throughput(Throughput::Elements(workload.requests as u64));

    let mut tokio = TokioHarness::new();
    black_box(tokio.run(workload.warmup()));
    group.bench_function("tokio_mt_4", |b| b.iter(|| black_box(tokio.run(workload))));

    let mut spargio_queue = SpargioHarness::new(BackendKind::Queue).expect("spargio queue harness");
    black_box(spargio_queue.run(workload.warmup()));
    group.bench_function("spargio_queue", |b| {
        b.iter(|| black_box(spargio_queue.run(workload)))
    });

    #[cfg(target_os = "linux")]
    if let Some(mut spargio_uring) = SpargioHarness::new(BackendKind::IoUring) {
        black_box(spargio_uring.run(workload.warmup()));
        group.bench_function("spargio_io_uring", |b| {
            b.iter(|| black_box(spargio_uring.run(workload)))
        });
    }

    group.finish();
}

fn bench_skew(c: &mut Criterion) {
    let workload = Workload::skewed();
    let mut group = c.benchmark_group("fanout_fanin_skewed");
    group.throughput(Throughput::Elements(workload.requests as u64));

    let mut tokio = TokioHarness::new();
    black_box(tokio.run(workload.warmup()));
    group.bench_function("tokio_mt_4", |b| b.iter(|| black_box(tokio.run(workload))));

    let mut spargio_queue = SpargioHarness::new(BackendKind::Queue).expect("spargio queue harness");
    black_box(spargio_queue.run(workload.warmup()));
    group.bench_function("spargio_queue", |b| {
        b.iter(|| black_box(spargio_queue.run(workload)))
    });

    #[cfg(target_os = "linux")]
    if let Some(mut spargio_uring) = SpargioHarness::new(BackendKind::IoUring) {
        black_box(spargio_uring.run(workload.warmup()));
        group.bench_function("spargio_io_uring", |b| {
            b.iter(|| black_box(spargio_uring.run(workload)))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_balanced, bench_skew);
criterion_main!(benches);
