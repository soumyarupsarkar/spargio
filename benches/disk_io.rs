#[cfg(unix)]
use criterion::{Criterion, black_box, criterion_group, criterion_main};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process;
#[cfg(unix)]
use std::sync::mpsc as std_mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};

#[cfg(unix)]
const BLOCK_SIZE: usize = 4096;
#[cfg(unix)]
const FILE_BLOCKS: u32 = 4096; // 16 MiB
#[cfg(unix)]
const DISK_RTT_ROUNDS: usize = 256;

#[cfg(target_os = "linux")]
const MSG_RING_CQE_FLAG: u32 = 1 << 8;
#[cfg(target_os = "linux")]
const CLIENT_SHARD: u16 = 0;
#[cfg(target_os = "linux")]
const WORKER_SHARD: u16 = 1;
#[cfg(target_os = "linux")]
const READ_REQ_TAG: u16 = 1;
#[cfg(target_os = "linux")]
const READ_ACK_TAG: u16 = 2;
#[cfg(target_os = "linux")]
const SHUTDOWN_TAG: u16 = 9;

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
            "msg_ring_runtime_disk_bench_{}_{}.dat",
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
        let pattern = [0x5Au8; BLOCK_SIZE];
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
enum TokioDiskCmd {
    ReadRtt {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(unix)]
enum TokioDiskWire {
    Read(u32),
    Shutdown,
}

#[cfg(unix)]
struct TokioDiskHarness {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TokioDiskCmd>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TokioDiskHarness {
    fn new(path: &Path, blocks: u32) -> Self {
        let path = path.to_path_buf();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TokioDiskCmd>();

        let thread = thread::Builder::new()
            .name("bench-tokio-disk".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    let (wire_tx, mut wire_rx) =
                        tokio::sync::mpsc::unbounded_channel::<TokioDiskWire>();
                    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
                    let file = File::open(path).expect("open disk fixture");

                    let responder = tokio::spawn(async move {
                        let mut buf = vec![0u8; BLOCK_SIZE];
                        while let Some(msg) = wire_rx.recv().await {
                            match msg {
                                TokioDiskWire::Read(block) => {
                                    let offset = u64::from(block) * BLOCK_SIZE as u64;
                                    let _ = file.read_at(&mut buf, offset).expect("pread");
                                    if ack_tx.send(block).is_err() {
                                        break;
                                    }
                                }
                                TokioDiskWire::Shutdown => break,
                            }
                        }
                    });

                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioDiskCmd::ReadRtt { rounds, reply } => {
                                let mut checksum = 0u64;
                                for i in 0..rounds {
                                    let block = (i as u32) % blocks;
                                    wire_tx.send(TokioDiskWire::Read(block)).expect("send read");
                                    let ack = ack_rx.recv().await.expect("ack");
                                    checksum = checksum.wrapping_add(u64::from(ack));
                                }
                                let _ = reply.send(checksum);
                            }
                            TokioDiskCmd::Shutdown { reply } => {
                                let _ = wire_tx.send(TokioDiskWire::Shutdown);
                                let _ = responder.await;
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn tokio disk bench thread");

        Self {
            cmd_tx,
            thread: Some(thread),
        }
    }

    fn read_rtt(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioDiskCmd::ReadRtt { rounds, reply: tx })
            .expect("send read cmd");
        rx.recv().expect("read reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(TokioDiskCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(unix)]
impl Drop for TokioDiskHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(target_os = "linux")]
enum IoUringDiskCmd {
    ReadRtt {
        rounds: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(target_os = "linux")]
struct IoUringMsgDiskHarness {
    cmd_tx: std_mpsc::Sender<IoUringDiskCmd>,
    client_thread: Option<thread::JoinHandle<()>>,
    worker_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl IoUringMsgDiskHarness {
    fn new(path: &Path, blocks: u32) -> Option<Self> {
        let ring_client = IoUring::new(256).ok()?;
        let ring_worker = IoUring::new(256).ok()?;
        let client_fd = ring_client.as_raw_fd();
        let worker_fd = ring_worker.as_raw_fd();
        let path = path.to_path_buf();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<IoUringDiskCmd>();

        let worker_thread = thread::Builder::new()
            .name("bench-uring-disk-worker".to_owned())
            .spawn(move || run_uring_worker(ring_worker, client_fd, path))
            .ok()?;
        let client_thread = thread::Builder::new()
            .name("bench-uring-disk-client".to_owned())
            .spawn(move || run_uring_client(ring_client, worker_fd, blocks, cmd_rx))
            .ok()?;

        Some(Self {
            cmd_tx,
            client_thread: Some(client_thread),
            worker_thread: Some(worker_thread),
        })
    }

    fn read_rtt(&mut self, rounds: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(IoUringDiskCmd::ReadRtt { rounds, reply: tx })
            .expect("send read cmd");
        rx.recv().expect("read reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(IoUringDiskCmd::Shutdown { reply: tx });
        let _ = rx.recv();

        if let Some(join) = self.client_thread.take() {
            let _ = join.join();
        }
        if let Some(join) = self.worker_thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for IoUringMsgDiskHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(target_os = "linux")]
fn run_uring_client(
    mut ring: IoUring,
    worker_fd: RawFd,
    blocks: u32,
    cmd_rx: std_mpsc::Receiver<IoUringDiskCmd>,
) {
    for cmd in cmd_rx {
        match cmd {
            IoUringDiskCmd::ReadRtt { rounds, reply } => {
                let mut checksum = 0u64;
                for i in 0..rounds {
                    let block = (i as u32) % blocks;
                    submit_msg_ring(
                        &mut ring,
                        worker_fd,
                        CLIENT_SHARD,
                        READ_REQ_TAG,
                        block,
                        0x10,
                    );

                    loop {
                        let _ = ring.submit_and_wait(1);
                        let mut got_ack = false;
                        let mut cq = ring.completion();
                        for cqe in &mut cq {
                            if cqe.flags() & MSG_RING_CQE_FLAG == 0 {
                                continue;
                            }
                            let (_from, tag) = unpack_msg_userdata(cqe.user_data());
                            if tag == READ_ACK_TAG {
                                let ack = u32::from_ne_bytes(cqe.result().to_ne_bytes());
                                checksum = checksum.wrapping_add(u64::from(ack));
                                got_ack = true;
                            }
                        }
                        if got_ack {
                            break;
                        }
                    }
                }
                let _ = reply.send(checksum);
            }
            IoUringDiskCmd::Shutdown { reply } => {
                submit_msg_ring(&mut ring, worker_fd, CLIENT_SHARD, SHUTDOWN_TAG, 0, 0x11);
                let _ = reply.send(());
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_uring_worker(mut ring: IoUring, client_fd: RawFd, path: PathBuf) {
    let file = File::open(path).expect("open disk fixture");
    let file_fd = file.as_raw_fd();
    let mut buf = vec![0u8; BLOCK_SIZE];
    let mut completed_blocks = std::collections::VecDeque::new();

    loop {
        let _ = ring.submit_and_wait(1);
        let mut pending_reads = Vec::new();
        let mut read_completions = 0usize;
        {
            let mut cq = ring.completion();
            for cqe in &mut cq {
                if cqe.flags() & MSG_RING_CQE_FLAG != 0 {
                    let (_from, tag) = unpack_msg_userdata(cqe.user_data());
                    let val = u32::from_ne_bytes(cqe.result().to_ne_bytes());
                    match tag {
                        READ_REQ_TAG => pending_reads.push(val),
                        SHUTDOWN_TAG => return,
                        _ => {}
                    }
                    continue;
                }

                read_completions += 1;
            }
        }

        for block in pending_reads {
            let offset = u64::from(block) * BLOCK_SIZE as u64;
            submit_read(&mut ring, file_fd, &mut buf, offset, 0x20);
            completed_blocks.push_back(block);
        }

        for _ in 0..read_completions {
            if let Some(block) = completed_blocks.pop_front() {
                submit_msg_ring(
                    &mut ring,
                    client_fd,
                    WORKER_SHARD,
                    READ_ACK_TAG,
                    block,
                    0x21,
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn submit_read(ring: &mut IoUring, file_fd: RawFd, buf: &mut [u8], offset: u64, user_data: u64) {
    let entry = opcode::Read::new(types::Fd(file_fd), buf.as_mut_ptr(), buf.len() as u32)
        .offset(offset)
        .build()
        .user_data(user_data);
    push_entry(ring, entry);
    let _ = ring.submit();
}

#[cfg(target_os = "linux")]
fn submit_msg_ring(
    ring: &mut IoUring,
    target_fd: RawFd,
    from: u16,
    tag: u16,
    val: u32,
    user_data: u64,
) {
    let payload = pack_msg_userdata(from, tag);
    let val_i32 = i32::from_ne_bytes(val.to_ne_bytes());
    let entry = opcode::MsgRingData::new(
        types::Fd(target_fd),
        val_i32,
        payload,
        Some(MSG_RING_CQE_FLAG),
    )
    .build()
    .user_data(user_data);
    push_entry(ring, entry);
    let _ = ring.submit();
}

#[cfg(target_os = "linux")]
fn push_entry(ring: &mut IoUring, entry: io_uring::squeue::Entry) {
    loop {
        let mut sq = ring.submission();
        if unsafe { sq.push(&entry) }.is_ok() {
            return;
        }
        drop(sq);
        let _ = ring.submit();
    }
}

#[cfg(target_os = "linux")]
fn pack_msg_userdata(from: u16, tag: u16) -> u64 {
    (u64::from(from) << 16) | u64::from(tag)
}

#[cfg(target_os = "linux")]
fn unpack_msg_userdata(data: u64) -> (u16, u16) {
    (
        ((data >> 16) & u64::from(u16::MAX)) as u16,
        (data & u64::from(u16::MAX)) as u16,
    )
}

#[cfg(unix)]
fn bench_disk_read_rtt(c: &mut Criterion) {
    let fixture = DiskFixture::new(FILE_BLOCKS);
    let mut group = c.benchmark_group("disk_read_rtt_4k");

    let mut tokio = TokioDiskHarness::new(&fixture.path, fixture.blocks);
    black_box(tokio.read_rtt(32));
    group.bench_function("tokio_two_worker_pread", |b| {
        b.iter(|| black_box(tokio.read_rtt(DISK_RTT_ROUNDS)))
    });

    #[cfg(target_os = "linux")]
    if let Some(mut uring) = IoUringMsgDiskHarness::new(&fixture.path, fixture.blocks) {
        black_box(uring.read_rtt(32));
        group.bench_function("io_uring_msg_ring_two_ring_pread", |b| {
            b.iter(|| black_box(uring.read_rtt(DISK_RTT_ROUNDS)))
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(benches, bench_disk_read_rtt);
#[cfg(unix)]
criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
