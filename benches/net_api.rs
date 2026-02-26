#[cfg(unix)]
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::StreamExt;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::channel::mpsc;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::executor::block_on;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::{BackendKind, Runtime, RuntimeError};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::sync::mpsc as std_mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
const RTT_ROUNDS: usize = 512;
#[cfg(unix)]
const RTT_PAYLOAD: usize = 256;
#[cfg(unix)]
const THROUGHPUT_FRAMES: usize = 2048;
#[cfg(unix)]
const THROUGHPUT_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const THROUGHPUT_WINDOW: usize = 32;
#[cfg(unix)]
const THROUGHPUT_RECV_SCRATCH: usize = 64 * 1024;

#[cfg(unix)]
fn spawn_echo_peer(client_nonblocking: bool, name: &str) -> (TcpStream, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");

    let client = TcpStream::connect(addr).expect("connect client");
    let (mut server, _) = listener.accept().expect("accept");
    client.set_nodelay(true).expect("nodelay client");
    server.set_nodelay(true).expect("nodelay server");
    client
        .set_nonblocking(client_nonblocking)
        .expect("set nonblocking");

    let join = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match server.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if server.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now();
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn echo thread");

    (client, join)
}

#[cfg(unix)]
enum TokioNetCmd {
    EchoRtt {
        rounds: usize,
        payload: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoWindowed {
        frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(unix)]
struct TokioNetHarness {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TokioNetCmd>,
    thread: Option<thread::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TokioNetHarness {
    fn new() -> Self {
        let (client, echo_thread) = spawn_echo_peer(true, "bench-net-echo-tokio");
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TokioNetCmd>();

        let thread = thread::Builder::new()
            .name("bench-net-tokio".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    let mut stream = tokio::net::TcpStream::from_std(client)
                        .expect("convert std stream to tokio stream");
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioNetCmd::EchoRtt {
                                rounds,
                                payload,
                                reply,
                            } => {
                                let value = tokio_echo_rtt(&mut stream, rounds, payload).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoWindowed {
                                frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value =
                                    tokio_echo_windowed(&mut stream, frames, payload, window).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn tokio net bench thread");

        Self {
            cmd_tx,
            thread: Some(thread),
            echo_thread: Some(echo_thread),
        }
    }

    fn echo_rtt(&mut self, rounds: usize, payload: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoRtt {
                rounds,
                payload,
                reply: tx,
            })
            .expect("send echo rtt cmd");
        rx.recv().expect("echo rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoWindowed {
                frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo windowed cmd");
        rx.recv().expect("echo windowed reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(TokioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();

        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(unix)]
impl Drop for TokioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
async fn tokio_echo_rtt(
    stream: &mut tokio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        stream.write_all(&payload).await.expect("tokio write_all");
        stream
            .read_exact(&mut recv)
            .await
            .expect("tokio read_exact");
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_windowed(
    stream: &mut tokio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; THROUGHPUT_RECV_SCRATCH];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            stream.write_all(&payload).await.expect("tokio write_all");
        }
        let mut remaining = batch * payload_len;
        while remaining > 0 {
            let got = stream.read(&mut recv).await.expect("tokio read");
            if got == 0 {
                panic!("tokio stream closed during throughput receive");
            }
            checksum = checksum.wrapping_add(u64::from(recv[0]));
            remaining = remaining.saturating_sub(got);
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct SpargioNetHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<SpargioNetCmd>,
    worker_join: Option<spargio::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum SpargioNetCmd {
    EchoRtt {
        rounds: usize,
        payload: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoWindowed {
        frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl SpargioNetHarness {
    fn new() -> Option<Self> {
        let runtime = match Runtime::builder()
            .backend(BackendKind::IoUring)
            .shards(2)
            .build()
        {
            Ok(rt) => rt,
            Err(RuntimeError::IoUringInit(_)) => return None,
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        };

        let lane = runtime.handle().uring_native_lane(1).ok()?;
        let (client, echo_thread) = spawn_echo_peer(false, "bench-net-echo-spargio");
        let bound = lane.bind_tcp_stream(client);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SpargioNetCmd>();
        let worker_join = runtime
            .spawn_on(1, async move {
                while let Some(cmd) = cmd_rx.next().await {
                    match cmd {
                        SpargioNetCmd::EchoRtt {
                            rounds,
                            payload,
                            reply,
                        } => {
                            let value = spargio_echo_rtt(&bound, rounds, payload).await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::EchoWindowed {
                            frames,
                            payload,
                            window,
                            reply,
                        } => {
                            let value =
                                spargio_echo_windowed(&bound, frames, payload, window).await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::Shutdown { reply } => {
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
            echo_thread: Some(echo_thread),
        })
    }

    fn echo_rtt(&mut self, rounds: usize, payload_len: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoRtt {
                rounds,
                payload: payload_len,
                reply: tx,
            })
            .expect("send spargio rtt cmd");
        rx.recv().expect("spargio rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload_len: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoWindowed {
                frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio windowed cmd");
        rx.recv().expect("spargio windowed reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(SpargioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.worker_join.take() {
            let _ = block_on(join);
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
        let _ = &self.runtime;
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for SpargioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_rtt(bound: &spargio::UringBoundFd, rounds: usize, payload_len: usize) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        payload = uring_send_all_owned(bound, payload).await.expect("send");
        recv = uring_recv_exact_owned(bound, recv).await.expect("recv");
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_windowed(
    bound: &spargio::UringBoundFd,
    frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; THROUGHPUT_RECV_SCRATCH];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            payload = uring_send_all_owned(bound, payload).await.expect("send");
        }
        let mut remaining = batch * payload_len;
        while remaining > 0 {
            let (got, returned) = bound.recv_owned(recv).await.expect("recv");
            recv = returned;
            if got == 0 {
                panic!("spargio stream closed during throughput receive");
            }
            checksum = checksum.wrapping_add(u64::from(recv[0]));
            remaining = remaining.saturating_sub(got);
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn uring_send_all_owned(
    bound: &spargio::UringBoundFd,
    mut buf: Vec<u8>,
) -> std::io::Result<Vec<u8>> {
    let mut sent = 0usize;
    while sent < buf.len() {
        if sent == 0 {
            let (wrote, returned) = bound.send_owned(buf).await?;
            buf = returned;
            if wrote == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "zero-length send",
                ));
            }
            sent += wrote;
        } else {
            let wrote = bound.send(&buf[sent..]).await?;
            if wrote == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "zero-length send",
                ));
            }
            sent += wrote;
        }
    }
    Ok(buf)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn uring_recv_exact_owned(
    bound: &spargio::UringBoundFd,
    mut dst: Vec<u8>,
) -> std::io::Result<Vec<u8>> {
    let mut received = 0usize;
    let mut first_byte = None;
    while received < dst.len() {
        let (got, returned) = bound.recv_owned(dst).await?;
        dst = returned;
        if got == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "stream closed",
            ));
        }
        if first_byte.is_none() {
            first_byte = Some(dst[0]);
        }
        received += got;
    }
    if let Some(b) = first_byte {
        dst[0] = b;
    }
    Ok(dst)
}

#[cfg(unix)]
fn bench_net_echo_rtt(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_echo_rtt_256b");
    group.throughput(Throughput::Bytes((RTT_ROUNDS * RTT_PAYLOAD) as u64));

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_rtt(32, RTT_PAYLOAD));
    group.bench_function("tokio_tcp_echo_qd1", |b| {
        b.iter(|| black_box(tokio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.echo_rtt(32, RTT_PAYLOAD));
        group.bench_function("spargio_uring_bound_tcp_qd1", |b| {
            b.iter(|| black_box(spargio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_throughput_4k_window32");
    group.throughput(Throughput::Bytes(
        (THROUGHPUT_FRAMES * THROUGHPUT_FRAME_BYTES) as u64,
    ));

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
    group.bench_function("tokio_tcp_echo_window32", |b| {
        b.iter(|| {
            black_box(tokio.echo_windowed(
                THROUGHPUT_FRAMES,
                THROUGHPUT_FRAME_BYTES,
                THROUGHPUT_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
        group.bench_function("spargio_uring_bound_tcp_window32", |b| {
            b.iter(|| {
                black_box(spargio.echo_windowed(
                    THROUGHPUT_FRAMES,
                    THROUGHPUT_FRAME_BYTES,
                    THROUGHPUT_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(benches, bench_net_echo_rtt, bench_net_stream_throughput);
#[cfg(unix)]
criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
