#[cfg(unix)]
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
#[cfg(unix)]
use futures::future::join_all;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::{StreamExt, channel::mpsc, executor::block_on};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::{BackendKind, Runtime, RuntimeError};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::mpsc as std_mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const BLOCK_SIZE: usize = 4096;
#[cfg(unix)]
const FILE_BLOCKS: u32 = 4096; // 16 MiB
#[cfg(unix)]
const RTT_ROUNDS: usize = 256;
#[cfg(unix)]
const THROUGHPUT_ROUNDS: usize = 4096;
#[cfg(unix)]
const THROUGHPUT_QD: usize = 32;

#[cfg(unix)]
struct DiskFixture {
    path: PathBuf,
    blocks: u32,
}

#[cfg(unix)]
impl DiskFixture {
    fn new(blocks: u32) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spargio_fs_api_bench_{}_{}.dat",
            process::id(),
            unique
        ));

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("create fixture file");
        let pattern = [0xA5u8; BLOCK_SIZE];
        for _ in 0..blocks {
            file.write_all(&pattern).expect("seed fixture");
        }
        file.sync_all().expect("sync fixture");

        Self { path, blocks }
    }
}

#[cfg(unix)]
impl Drop for DiskFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
enum TokioFsCmd {
    ReadRtt {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    ReadQd {
        rounds: usize,
        qd: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(unix)]
struct TokioFsHarness {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TokioFsCmd>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TokioFsHarness {
    fn new(path: &Path, blocks: u32) -> Self {
        let path = path.to_path_buf();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TokioFsCmd>();

        let thread = thread::Builder::new()
            .name("bench-tokio-fs-api".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let file = Arc::new(File::open(path).expect("open fixture"));

                rt.block_on(async move {
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioFsCmd::ReadRtt { rounds, reply } => {
                                let mut checksum = 0u64;
                                for i in 0..rounds {
                                    let block = (i as u32) % blocks;
                                    checksum = checksum
                                        .wrapping_add(tokio_read_at(file.clone(), block).await);
                                }
                                let _ = reply.send(checksum);
                            }
                            TokioFsCmd::ReadQd { rounds, qd, reply } => {
                                let mut checksum = 0u64;
                                let mut next = 0usize;
                                while next < rounds {
                                    let batch = (rounds - next).min(qd.max(1));
                                    let mut reads = Vec::with_capacity(batch);
                                    for offset in 0..batch {
                                        let block = ((next + offset) as u32) % blocks;
                                        reads.push(tokio_read_at(file.clone(), block));
                                    }
                                    for value in join_all(reads).await {
                                        checksum = checksum.wrapping_add(value);
                                    }
                                    next += batch;
                                }
                                let _ = reply.send(checksum);
                            }
                            TokioFsCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn tokio fs bench thread");

        Self {
            cmd_tx,
            thread: Some(thread),
        }
    }

    fn read_rtt(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioFsCmd::ReadRtt { rounds, reply: tx })
            .expect("send rtt cmd");
        rx.recv().expect("read rtt reply")
    }

    fn read_qd(&mut self, rounds: usize, qd: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioFsCmd::ReadQd {
                rounds,
                qd,
                reply: tx,
            })
            .expect("send qd cmd");
        rx.recv().expect("read qd reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(TokioFsCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(unix)]
impl Drop for TokioFsHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
async fn tokio_read_at(file: Arc<File>, block: u32) -> u64 {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; BLOCK_SIZE];
        let offset = u64::from(block) * BLOCK_SIZE as u64;
        let got = file.read_at(&mut buf, offset).expect("pread");
        assert_eq!(got, BLOCK_SIZE, "short read in tokio bench");
        u64::from(block) ^ u64::from(buf[0])
    })
    .await
    .expect("spawn_blocking join")
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum SpargioFsCmd {
    ReadRtt {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    ReadQd {
        rounds: usize,
        qd: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct SpargioFsHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<SpargioFsCmd>,
    worker_join: Option<spargio::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl SpargioFsHarness {
    fn new(path: &Path, blocks: u32) -> Option<Self> {
        let runtime = match Runtime::builder()
            .shards(2)
            .backend(BackendKind::IoUring)
            .io_uring_throughput_mode(None)
            .build()
        {
            Ok(rt) => rt,
            Err(RuntimeError::IoUringInit(_)) => match Runtime::builder()
                .shards(2)
                .backend(BackendKind::IoUring)
                .build()
            {
                Ok(rt) => rt,
                Err(RuntimeError::IoUringInit(_)) => return None,
                Err(err) => panic!("unexpected runtime init error: {err:?}"),
            },
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        };

        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .expect("open fixture");
        let file = spargio::fs::File::from_std(runtime.handle(), file).ok()?;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SpargioFsCmd>();
        let worker_join = runtime
            .handle()
            .spawn_with_placement(spargio::TaskPlacement::StealablePreferred(1), async move {
                let mut rtt_buf = vec![0u8; BLOCK_SIZE];
                while let Some(cmd) = cmd_rx.next().await {
                    match cmd {
                        SpargioFsCmd::ReadRtt { rounds, reply } => {
                            let mut checksum = 0u64;
                            for i in 0..rounds {
                                let block = (i as u32) % blocks;
                                let offset = u64::from(block) * BLOCK_SIZE as u64;
                                let buf = std::mem::take(&mut rtt_buf);
                                let (got, returned) =
                                    file.read_at_into(offset, buf).await.expect("read_at_into");
                                rtt_buf = returned;
                                assert_eq!(got, BLOCK_SIZE, "short read in spargio bench");
                                checksum =
                                    checksum.wrapping_add(u64::from(block) ^ u64::from(rtt_buf[0]));
                            }
                            let _ = reply.send(checksum);
                        }
                        SpargioFsCmd::ReadQd { rounds, qd, reply } => {
                            let mut checksum = 0u64;
                            let mut next = 0usize;
                            while next < rounds {
                                let batch = (rounds - next).min(qd.max(1));
                                let mut reads = Vec::with_capacity(batch);
                                for offset in 0..batch {
                                    let block = ((next + offset) as u32) % blocks;
                                    let file = file.clone();
                                    reads.push(async move {
                                        let off = u64::from(block) * BLOCK_SIZE as u64;
                                        let bytes =
                                            file.read_at(off, BLOCK_SIZE).await.expect("read_at");
                                        assert_eq!(
                                            bytes.len(),
                                            BLOCK_SIZE,
                                            "short read in spargio bench"
                                        );
                                        u64::from(block) ^ u64::from(bytes[0])
                                    });
                                }
                                for value in join_all(reads).await {
                                    checksum = checksum.wrapping_add(value);
                                }
                                next += batch;
                            }
                            let _ = reply.send(checksum);
                        }
                        SpargioFsCmd::Shutdown { reply } => {
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

    fn read_rtt(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioFsCmd::ReadRtt { rounds, reply: tx })
            .expect("send spargio rtt cmd");
        rx.recv().expect("spargio rtt reply")
    }

    fn read_qd(&mut self, rounds: usize, qd: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioFsCmd::ReadQd {
                rounds,
                qd,
                reply: tx,
            })
            .expect("send spargio qd cmd");
        rx.recv().expect("spargio qd reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(SpargioFsCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.worker_join.take() {
            let _ = block_on(join);
        }
        let _ = &self.runtime;
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for SpargioFsHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum CompioFsCmd {
    ReadRtt {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    ReadQd {
        rounds: usize,
        qd: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct CompioFsHarness {
    cmd_tx: std_mpsc::Sender<CompioFsCmd>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl CompioFsHarness {
    fn new(path: &Path, blocks: u32) -> Option<Self> {
        let path = path.to_path_buf();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<CompioFsCmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<bool>();

        let thread = thread::Builder::new()
            .name("bench-compio-fs-api".to_owned())
            .spawn(move || {
                use compio::io::AsyncReadAt;

                let runtime = match compio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => {
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let _ = ready_tx.send(true);

                runtime.block_on(async move {
                    let file = compio::fs::File::open(path).await.expect("open fixture");
                    let mut rtt_buf = vec![0u8; BLOCK_SIZE];
                    while let Ok(cmd) = cmd_rx.recv() {
                        match cmd {
                            CompioFsCmd::ReadRtt { rounds, reply } => {
                                let mut checksum = 0u64;
                                for i in 0..rounds {
                                    let block = (i as u32) % blocks;
                                    let offset = u64::from(block) * BLOCK_SIZE as u64;
                                    let out = file.read_at(rtt_buf, offset).await;
                                    let got = out.0.expect("compio read_at");
                                    rtt_buf = out.1;
                                    assert_eq!(got, BLOCK_SIZE, "short read in compio bench");
                                    checksum = checksum
                                        .wrapping_add(u64::from(block) ^ u64::from(rtt_buf[0]));
                                }
                                let _ = reply.send(checksum);
                            }
                            CompioFsCmd::ReadQd { rounds, qd, reply } => {
                                let mut checksum = 0u64;
                                let mut next = 0usize;
                                while next < rounds {
                                    let batch = (rounds - next).min(qd.max(1));
                                    let mut reads = Vec::with_capacity(batch);
                                    for offset in 0..batch {
                                        let block = ((next + offset) as u32) % blocks;
                                        let off = u64::from(block) * BLOCK_SIZE as u64;
                                        let file = file.clone();
                                        reads.push(async move {
                                            let out =
                                                file.read_at(vec![0u8; BLOCK_SIZE], off).await;
                                            let got = out.0.expect("compio read_at");
                                            assert_eq!(
                                                got, BLOCK_SIZE,
                                                "short read in compio bench"
                                            );
                                            u64::from(block) ^ u64::from(out.1[0])
                                        });
                                    }
                                    for value in join_all(reads).await {
                                        checksum = checksum.wrapping_add(value);
                                    }
                                    next += batch;
                                }
                                let _ = reply.send(checksum);
                            }
                            CompioFsCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn compio fs bench thread");

        if !ready_rx.recv().ok().unwrap_or(false) {
            let _ = thread.join();
            return None;
        }

        Some(Self {
            cmd_tx,
            thread: Some(thread),
        })
    }

    fn read_rtt(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioFsCmd::ReadRtt { rounds, reply: tx })
            .expect("send compio rtt cmd");
        rx.recv().expect("compio rtt reply")
    }

    fn read_qd(&mut self, rounds: usize, qd: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioFsCmd::ReadQd {
                rounds,
                qd,
                reply: tx,
            })
            .expect("send compio qd cmd");
        rx.recv().expect("compio qd reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(CompioFsCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for CompioFsHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
fn bench_fs_read_rtt(c: &mut Criterion) {
    let fixture = DiskFixture::new(FILE_BLOCKS);
    let mut group = c.benchmark_group("fs_read_rtt_4k");
    group.throughput(Throughput::Bytes((RTT_ROUNDS * BLOCK_SIZE) as u64));

    let mut tokio = TokioFsHarness::new(&fixture.path, fixture.blocks);
    black_box(tokio.read_rtt(32));
    group.bench_function("tokio_spawn_blocking_pread_qd1", |b| {
        b.iter(|| black_box(tokio.read_rtt(RTT_ROUNDS)))
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioFsHarness::new(&fixture.path, fixture.blocks) {
        black_box(spargio.read_rtt(32));
        group.bench_function("spargio_fs_read_at_qd1", |b| {
            b.iter(|| black_box(spargio.read_rtt(RTT_ROUNDS)))
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioFsHarness::new(&fixture.path, fixture.blocks) {
        black_box(compio.read_rtt(32));
        group.bench_function("compio_fs_read_at_qd1", |b| {
            b.iter(|| black_box(compio.read_rtt(RTT_ROUNDS)))
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fs_read_throughput(c: &mut Criterion) {
    let fixture = DiskFixture::new(FILE_BLOCKS);
    let mut group = c.benchmark_group("fs_read_throughput_4k_qd32");
    group.throughput(Throughput::Bytes((THROUGHPUT_ROUNDS * BLOCK_SIZE) as u64));

    let mut tokio = TokioFsHarness::new(&fixture.path, fixture.blocks);
    black_box(tokio.read_qd(256, THROUGHPUT_QD));
    group.bench_function("tokio_spawn_blocking_pread_qd32", |b| {
        b.iter(|| black_box(tokio.read_qd(THROUGHPUT_ROUNDS, THROUGHPUT_QD)))
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioFsHarness::new(&fixture.path, fixture.blocks) {
        black_box(spargio.read_qd(256, THROUGHPUT_QD));
        group.bench_function("spargio_fs_read_at_qd32", |b| {
            b.iter(|| black_box(spargio.read_qd(THROUGHPUT_ROUNDS, THROUGHPUT_QD)))
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioFsHarness::new(&fixture.path, fixture.blocks) {
        black_box(compio.read_qd(256, THROUGHPUT_QD));
        group.bench_function("compio_fs_read_at_qd32", |b| {
            b.iter(|| black_box(compio.read_qd(THROUGHPUT_ROUNDS, THROUGHPUT_QD)))
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(benches, bench_fs_read_rtt, bench_fs_read_throughput);
#[cfg(unix)]
criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
