#[cfg(unix)]
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::{StreamExt, channel::mpsc, executor::block_on, future::join_all};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use libc;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::{BackendKind, Runtime, RuntimeError, RuntimeHandle};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{SocketAddr, TcpListener};
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
const IMBALANCED_STREAMS: usize = 8;
#[cfg(unix)]
const IMBALANCED_HEAVY_FRAMES: usize = 2048;
#[cfg(unix)]
const IMBALANCED_LIGHT_FRAMES: usize = 128;
#[cfg(unix)]
const IMBALANCED_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const IMBALANCED_WINDOW: usize = 32;
#[cfg(unix)]
const IMBALANCED_TOTAL_FRAMES: usize =
    IMBALANCED_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * IMBALANCED_LIGHT_FRAMES);
#[cfg(unix)]
const HOTSPOT_ROTATION_STEPS: usize = 64;
#[cfg(unix)]
const HOTSPOT_ROTATION_HEAVY_FRAMES: usize = 32;
#[cfg(unix)]
const HOTSPOT_ROTATION_LIGHT_FRAMES: usize = 2;
#[cfg(unix)]
const HOTSPOT_ROTATION_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const HOTSPOT_ROTATION_WINDOW: usize = 32;
#[cfg(unix)]
const HOTSPOT_ROTATION_TOTAL_FRAMES: usize = HOTSPOT_ROTATION_STEPS
    * (HOTSPOT_ROTATION_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * HOTSPOT_ROTATION_LIGHT_FRAMES));
#[cfg(unix)]
const PIPELINE_STREAMS: usize = IMBALANCED_STREAMS;
#[cfg(unix)]
const PIPELINE_FRAMES_PER_STREAM: usize = 1024;
#[cfg(unix)]
const PIPELINE_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const PIPELINE_WINDOW: usize = 32;
#[cfg(unix)]
const PIPELINE_ROTATE_EVERY: usize = 64;
#[cfg(unix)]
const PIPELINE_HEAVY_CPU_ITERS: usize = 4000;
#[cfg(unix)]
const PIPELINE_LIGHT_CPU_ITERS: usize = 150;
#[cfg(unix)]
const PIPELINE_TOTAL_FRAMES: usize = PIPELINE_STREAMS * PIPELINE_FRAMES_PER_STREAM;

#[cfg(unix)]
fn imbalanced_frames_for_stream(idx: usize, heavy_frames: usize, light_frames: usize) -> usize {
    if idx == 0 { heavy_frames } else { light_frames }
}

#[cfg(unix)]
fn hotspot_rotation_frames_for_step(
    stream_idx: usize,
    step_idx: usize,
    stream_count: usize,
    heavy_frames: usize,
    light_frames: usize,
) -> usize {
    let stream_count = stream_count.max(1);
    let hotspot_stream = step_idx % stream_count;
    if stream_idx == hotspot_stream {
        heavy_frames
    } else {
        light_frames
    }
}

#[cfg(unix)]
fn hotspot_iters_for_frame(
    stream_idx: usize,
    frame_idx: usize,
    stream_count: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> usize {
    let stream_count = stream_count.max(1);
    let rotate_every = rotate_every.max(1);
    let hotspot_stream = (frame_idx / rotate_every) % stream_count;
    if stream_idx == hotspot_stream {
        heavy_iters
    } else {
        light_iters
    }
}

#[cfg(unix)]
fn pipeline_cpu_stage(seed: u8, stream_idx: usize, frame_idx: usize, iters: usize) -> u64 {
    let mut x = ((seed as u64) << 40)
        ^ ((stream_idx as u64) << 17)
        ^ ((frame_idx as u64) << 3)
        ^ 0x9E37_79B9_7F4A_7C15;
    for i in 0..iters {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x = x.wrapping_add((i as u64).wrapping_mul(0x27D4_EB2D));
    }
    std::hint::black_box(x)
}

#[cfg(unix)]
fn spawn_echo_server_with_clients(
    name: &str,
    client_count: usize,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");
    let client_count = client_count.max(1);
    let thread_name = name.to_owned();

    let join = thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            let mut handlers = Vec::with_capacity(client_count);
            for idx in 0..client_count {
                let (mut server, _) = listener.accept().expect("accept");
                server.set_nodelay(true).expect("nodelay server");
                let conn_name = format!("{thread_name}-conn-{idx}");
                let handler = thread::Builder::new()
                    .name(conn_name)
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
                                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                    thread::yield_now();
                                }
                                Err(_) => break,
                            }
                        }
                    })
                    .expect("spawn echo conn thread");
                handlers.push(handler);
            }

            for join in handlers {
                if join.join().is_err() {
                    break;
                }
            }
        })
        .expect("spawn echo thread");

    (addr, join)
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
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
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
        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-tokio", IMBALANCED_STREAMS);
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
                    let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
                    for _ in 0..IMBALANCED_STREAMS {
                        let stream = tokio::net::TcpStream::connect(addr)
                            .await
                            .expect("tokio connect");
                        stream.set_nodelay(true).expect("tokio nodelay");
                        streams.push(stream);
                    }

                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioNetCmd::EchoRtt {
                                rounds,
                                payload,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("tokio primary stream");
                                let value = tokio_echo_rtt(stream, rounds, payload).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoWindowed {
                                frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("tokio primary stream");
                                let value =
                                    tokio_echo_windowed(stream, frames, payload, window).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoImbalanced {
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_imbalanced(
                                    &mut streams,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoHotspotRotation {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoPipelineHotspot {
                                frames_per_stream,
                                payload,
                                window,
                                rotate_every,
                                heavy_iters,
                                light_iters,
                                reply,
                            } => {
                                let value = tokio_echo_pipeline_hotspot(
                                    &mut streams,
                                    frames_per_stream,
                                    payload,
                                    window,
                                    rotate_every,
                                    heavy_iters,
                                    light_iters,
                                )
                                .await;
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

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo imbalanced cmd");
        rx.recv().expect("echo imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo hotspot rotation cmd");
        rx.recv().expect("echo hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send echo pipeline hotspot cmd");
        rx.recv().expect("echo pipeline hotspot reply")
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
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            stream.write_all(&payload).await.expect("tokio write_all");
        }
        for _ in 0..batch {
            stream
                .read_exact(&mut recv)
                .await
                .expect("tokio read_exact");
            checksum = checksum.wrapping_add(u64::from(recv[0]));
        }
        next += batch;
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_imbalanced(
    streams: &mut Vec<tokio::net::TcpStream>,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        joins.spawn(async move {
            let value = tokio_echo_windowed(&mut stream, frames, payload_len, window).await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio imbalanced stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(unix)]
async fn tokio_echo_hotspot_rotation_stream(
    stream: &mut tokio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum =
            checksum.wrapping_add(tokio_echo_windowed(stream, frames, payload_len, window).await);
    }
    checksum
}

#[cfg(unix)]
async fn tokio_echo_hotspot_rotation(
    streams: &mut Vec<tokio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        joins.spawn(async move {
            let value = tokio_echo_hotspot_rotation_stream(
                &mut stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio hotspot rotation stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(unix)]
async fn tokio_echo_pipeline_stream(
    stream: &mut tokio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            stream.write_all(&payload).await.expect("tokio write_all");
        }
        for idx in 0..batch {
            stream
                .read_exact(&mut recv)
                .await
                .expect("tokio read_exact");
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }
        next += batch;
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_pipeline_hotspot(
    streams: &mut Vec<tokio::net::TcpStream>,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        joins.spawn(async move {
            let value = tokio_echo_pipeline_stream(
                &mut stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio pipeline stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
enum SpargioStreamInitMode {
    SingleContext,
    DistributedConnect,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
struct SpargioBenchStream {
    stream: spargio::net::TcpStream,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
enum SpargioRecvMode {
    Multishot,
    ReadExact,
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
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct SpargioNetHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<SpargioNetCmd>,
    worker_join: Option<spargio::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl SpargioNetHarness {
    fn new() -> Option<Self> {
        Self::new_with_stream_init_mode(SpargioStreamInitMode::SingleContext)
    }

    fn new_distributed() -> Option<Self> {
        Self::new_with_stream_init_mode(SpargioStreamInitMode::DistributedConnect)
    }

    fn new_with_stream_init_mode(stream_init: SpargioStreamInitMode) -> Option<Self> {
        let runtime = match Runtime::builder()
            .backend(BackendKind::IoUring)
            .shards(2)
            .io_uring_throughput_mode(None)
            .build()
        {
            Ok(rt) => rt,
            Err(RuntimeError::IoUringInit(_)) => match Runtime::builder()
                .backend(BackendKind::IoUring)
                .shards(2)
                .build()
            {
                Ok(rt) => rt,
                Err(RuntimeError::IoUringInit(_)) => return None,
                Err(err) => panic!("unexpected runtime init error: {err:?}"),
            },
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        };

        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-spargio", IMBALANCED_STREAMS);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SpargioNetCmd>();
        let handle = runtime.handle();
        let worker_handle = handle.clone();
        let worker_join = handle
            .spawn_with_placement(spargio::TaskPlacement::StealablePreferred(1), async move {
                let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
                match stream_init {
                    SpargioStreamInitMode::SingleContext => {
                        for _ in 0..IMBALANCED_STREAMS {
                            let stream =
                                spargio::net::TcpStream::connect(worker_handle.clone(), addr)
                                    .await
                                    .expect("spargio connect");
                            streams.push(SpargioBenchStream { stream });
                        }
                    }
                    SpargioStreamInitMode::DistributedConnect => {
                        let distributed = spargio::net::TcpStream::connect_many_round_robin(
                            worker_handle.clone(),
                            addr,
                            IMBALANCED_STREAMS,
                        )
                        .await
                        .expect("distributed stream connect");
                        for stream in distributed {
                            streams.push(SpargioBenchStream { stream });
                        }
                    }
                }
                while let Some(cmd) = cmd_rx.next().await {
                    match cmd {
                        SpargioNetCmd::EchoRtt {
                            rounds,
                            payload,
                            reply,
                        } => {
                            let stream = &streams.first().expect("spargio primary stream").stream;
                            let value = spargio_echo_rtt(stream, rounds, payload).await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::EchoWindowed {
                            frames,
                            payload,
                            window,
                            reply,
                        } => {
                            let stream = &streams.first().expect("spargio primary stream").stream;
                            let value = spargio_echo_windowed(
                                stream,
                                frames,
                                payload,
                                window,
                                SpargioRecvMode::Multishot,
                            )
                            .await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::EchoImbalanced {
                            heavy_frames,
                            light_frames,
                            payload,
                            window,
                            reply,
                        } => {
                            let value = spargio_echo_imbalanced(
                                worker_handle.clone(),
                                &streams,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                            )
                            .await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::EchoHotspotRotation {
                            steps,
                            heavy_frames,
                            light_frames,
                            payload,
                            window,
                            reply,
                        } => {
                            let value = spargio_echo_hotspot_rotation(
                                worker_handle.clone(),
                                &streams,
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                            )
                            .await;
                            let _ = reply.send(value);
                        }
                        SpargioNetCmd::EchoPipelineHotspot {
                            frames_per_stream,
                            payload,
                            window,
                            rotate_every,
                            heavy_iters,
                            light_iters,
                            reply,
                        } => {
                            let value = spargio_echo_pipeline_hotspot(
                                worker_handle.clone(),
                                &streams,
                                frames_per_stream,
                                payload,
                                window,
                                rotate_every,
                                heavy_iters,
                                light_iters,
                            )
                            .await;
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

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio imbalanced cmd");
        rx.recv().expect("spargio imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio hotspot rotation cmd");
        rx.recv().expect("spargio hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload_len: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload: payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send spargio pipeline hotspot cmd");
        rx.recv().expect("spargio pipeline hotspot reply")
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
async fn spargio_echo_rtt(
    stream: &spargio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        payload = stream
            .write_all_owned(payload)
            .await
            .expect("write_all_owned");
        recv = stream
            .read_exact_owned(recv)
            .await
            .expect("read_exact_owned");
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_windowed(
    stream: &spargio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
    recv_mode: SpargioRecvMode,
) -> u64 {
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window.max(1)).map(|_| vec![0u8; payload_len]).collect();
    let mut multishot_supported = matches!(recv_mode, SpargioRecvMode::Multishot);
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        let mut send_batch = Vec::with_capacity(batch);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            send_batch.push(payload);
        }
        let (_sent, mut returned) = stream
            .send_all_batch(send_batch, batch)
            .await
            .expect("send_all_batch");
        tx_pool.append(&mut returned);

        let mut remaining = batch * payload_len;
        if multishot_supported {
            let buffer_count = ((batch * 2).max(4)).min(u16::MAX as usize) as u16;
            match stream
                .recv_multishot_segments(payload_len, buffer_count, remaining)
                .await
            {
                Ok(multishot) => {
                    for seg in multishot.segments {
                        let end = seg
                            .offset
                            .saturating_add(seg.len)
                            .min(multishot.buffer.len());
                        if seg.offset < end {
                            checksum =
                                checksum.wrapping_add(u64::from(multishot.buffer[seg.offset]));
                            remaining = remaining.saturating_sub(end - seg.offset);
                            if remaining == 0 {
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    let raw = err.raw_os_error().unwrap_or_default();
                    if raw == libc::EINVAL || raw == libc::ENOSYS || raw == libc::EOPNOTSUPP {
                        multishot_supported = false;
                    } else {
                        panic!("recv_multishot_segments failed unexpectedly: {err}");
                    }
                }
            }
        }

        while remaining > 0 {
            recv = stream
                .read_exact_owned(recv)
                .await
                .expect("read_exact_owned");
            checksum = checksum.wrapping_add(u64::from(recv[0]));
            remaining = remaining.saturating_sub(payload_len);
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_imbalanced(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = Vec::with_capacity(streams.len());
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        let fut = async move {
            spargio_echo_windowed(
                &stream,
                frames,
                payload_len,
                window,
                SpargioRecvMode::Multishot,
            )
            .await
        };
        let join = handle
            .spawn_stealable(fut)
            .expect("spawn spargio imbalanced stream (stealable)");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio imbalanced stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_hotspot_rotation_stream(
    stream: &spargio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
    recv_mode: SpargioRecvMode,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum = checksum.wrapping_add(
            spargio_echo_windowed(stream, frames, payload_len, window, recv_mode).await,
        );
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_hotspot_rotation(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut joins = Vec::with_capacity(stream_count);
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let stream_for_spawn = stream.clone();
        let fut = async move {
            spargio_echo_hotspot_rotation_stream(
                &stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
                SpargioRecvMode::ReadExact,
            )
            .await
        };
        let join = stream_for_spawn
            .spawn_on_session(&handle, fut)
            .expect("spawn spargio hotspot rotation stream");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio hotspot rotation stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_pipeline_stream(
    stream: &spargio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            payload = stream
                .write_all_owned(payload)
                .await
                .expect("write_all_owned");
        }
        for idx in 0..batch {
            recv = stream
                .read_exact_owned(recv)
                .await
                .expect("read_exact_owned");
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_pipeline_hotspot(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut joins = Vec::with_capacity(stream_count);
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let stream_for_spawn = stream.clone();
        let fut = async move {
            spargio_echo_pipeline_stream(
                &stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await
        };
        let join = stream_for_spawn
            .spawn_on_session(&handle, fut)
            .expect("spawn spargio pipeline hotspot stream");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio pipeline stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum CompioNetCmd {
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
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct CompioNetHarness {
    cmd_tx: std_mpsc::Sender<CompioNetCmd>,
    thread: Option<thread::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl CompioNetHarness {
    fn new() -> Option<Self> {
        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-compio", IMBALANCED_STREAMS);
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<CompioNetCmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<bool>();

        let thread = thread::Builder::new()
            .name("bench-net-compio".to_owned())
            .spawn(move || {
                let runtime = match compio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => {
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let _ = ready_tx.send(true);

                runtime.block_on(async move {
                    let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
                    for _ in 0..IMBALANCED_STREAMS {
                        let stream = compio::net::TcpStream::connect(addr)
                            .await
                            .expect("compio connect");
                        streams.push(stream);
                    }
                    while let Ok(cmd) = cmd_rx.recv() {
                        match cmd {
                            CompioNetCmd::EchoRtt {
                                rounds,
                                payload,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("compio primary stream");
                                let value = compio_echo_rtt(stream, rounds, payload).await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoWindowed {
                                frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("compio primary stream");
                                let value =
                                    compio_echo_windowed(stream, frames, payload, window).await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoImbalanced {
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_imbalanced(
                                    &mut streams,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoHotspotRotation {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoPipelineHotspot {
                                frames_per_stream,
                                payload,
                                window,
                                rotate_every,
                                heavy_iters,
                                light_iters,
                                reply,
                            } => {
                                let value = compio_echo_pipeline_hotspot(
                                    &mut streams,
                                    frames_per_stream,
                                    payload,
                                    window,
                                    rotate_every,
                                    heavy_iters,
                                    light_iters,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn compio net bench thread");

        if !ready_rx.recv().ok().unwrap_or(false) {
            let _ = thread.join();
            let _ = echo_thread.join();
            return None;
        }

        Some(Self {
            cmd_tx,
            thread: Some(thread),
            echo_thread: Some(echo_thread),
        })
    }

    fn echo_rtt(&mut self, rounds: usize, payload: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoRtt {
                rounds,
                payload,
                reply: tx,
            })
            .expect("send compio rtt cmd");
        rx.recv().expect("compio rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoWindowed {
                frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio windowed cmd");
        rx.recv().expect("compio windowed reply")
    }

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio imbalanced cmd");
        rx.recv().expect("compio imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio hotspot rotation cmd");
        rx.recv().expect("compio hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send compio pipeline hotspot cmd");
        rx.recv().expect("compio pipeline hotspot reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(CompioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for CompioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_rtt(
    stream: &mut compio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        let out = stream.write_all(payload).await;
        out.0.expect("compio write_all");
        payload = out.1;

        let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
        recv = returned;
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_windowed(
    stream: &mut compio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let window = window.max(1);
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window).map(|_| vec![0u8; payload_len]).collect();
    let mut checksum = 0u64;
    let mut next = 0usize;

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            let out = stream.write_all(payload).await;
            out.0.expect("compio write_all");
            tx_pool.push(out.1);
        }

        for _ in 0..batch {
            let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
            recv = returned;
            checksum = checksum.wrapping_add(u64::from(recv[0]));
        }

        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_imbalanced(
    streams: &mut Vec<compio::net::TcpStream>,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        work.push(async move {
            let value = compio_echo_windowed(&mut stream, frames, payload_len, window).await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_hotspot_rotation_stream(
    stream: &mut compio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum =
            checksum.wrapping_add(compio_echo_windowed(stream, frames, payload_len, window).await);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_hotspot_rotation(
    streams: &mut Vec<compio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        work.push(async move {
            let value = compio_echo_hotspot_rotation_stream(
                &mut stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_pipeline_stream(
    stream: &mut compio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let window = window.max(1);
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window).map(|_| vec![0u8; payload_len]).collect();
    let mut checksum = 0u64;
    let mut next = 0usize;

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            let out = stream.write_all(payload).await;
            out.0.expect("compio write_all");
            tx_pool.push(out.1);
        }

        for idx in 0..batch {
            let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
            recv = returned;
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }

        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_pipeline_hotspot(
    streams: &mut Vec<compio::net::TcpStream>,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        work.push(async move {
            let value = compio_echo_pipeline_stream(
                &mut stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
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
        group.bench_function("spargio_tcp_echo_qd1", |b| {
            b.iter(|| black_box(spargio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_rtt(32, RTT_PAYLOAD));
        group.bench_function("compio_tcp_echo_qd1", |b| {
            b.iter(|| black_box(compio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
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
        group.bench_function("spargio_tcp_echo_window32", |b| {
            b.iter(|| {
                black_box(spargio.echo_windowed(
                    THROUGHPUT_FRAMES,
                    THROUGHPUT_FRAME_BYTES,
                    THROUGHPUT_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
        group.bench_function("compio_tcp_echo_window32", |b| {
            b.iter(|| {
                black_box(compio.echo_windowed(
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
fn bench_net_stream_imbalanced(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_imbalanced_4k_hot1_light7");
    group.throughput(Throughput::Bytes(
        (IMBALANCED_TOTAL_FRAMES * IMBALANCED_FRAME_BYTES) as u64,
    ));

    let warmup_heavy = (IMBALANCED_HEAVY_FRAMES / 8).max(1);
    let warmup_light = (IMBALANCED_LIGHT_FRAMES / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_imbalanced(
        warmup_heavy,
        warmup_light,
        IMBALANCED_FRAME_BYTES,
        IMBALANCED_WINDOW,
    ));
    group.bench_function("tokio_tcp_8streams_hotcold", |b| {
        b.iter(|| {
            black_box(tokio.echo_imbalanced(
                IMBALANCED_HEAVY_FRAMES,
                IMBALANCED_LIGHT_FRAMES,
                IMBALANCED_FRAME_BYTES,
                IMBALANCED_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_imbalanced(
            warmup_heavy,
            warmup_light,
            IMBALANCED_FRAME_BYTES,
            IMBALANCED_WINDOW,
        ));
        group.bench_function("spargio_tcp_8streams_hotcold", |b| {
            b.iter(|| {
                black_box(spargio.echo_imbalanced(
                    IMBALANCED_HEAVY_FRAMES,
                    IMBALANCED_LIGHT_FRAMES,
                    IMBALANCED_FRAME_BYTES,
                    IMBALANCED_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_imbalanced(
            warmup_heavy,
            warmup_light,
            IMBALANCED_FRAME_BYTES,
            IMBALANCED_WINDOW,
        ));
        group.bench_function("compio_tcp_8streams_hotcold", |b| {
            b.iter(|| {
                black_box(compio.echo_imbalanced(
                    IMBALANCED_HEAVY_FRAMES,
                    IMBALANCED_LIGHT_FRAMES,
                    IMBALANCED_FRAME_BYTES,
                    IMBALANCED_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_hotspot_rotation(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_hotspot_rotation_4k");
    group.throughput(Throughput::Bytes(
        (HOTSPOT_ROTATION_TOTAL_FRAMES * HOTSPOT_ROTATION_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (HOTSPOT_ROTATION_STEPS / 8).max(1);
    let warmup_heavy = (HOTSPOT_ROTATION_HEAVY_FRAMES / 4).max(1);
    let warmup_light = HOTSPOT_ROTATION_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        HOTSPOT_ROTATION_FRAME_BYTES,
        HOTSPOT_ROTATION_WINDOW,
    ));
    group.bench_function("tokio_tcp_8streams_rotating_hotspot", |b| {
        b.iter(|| {
            black_box(tokio.echo_hotspot_rotation(
                HOTSPOT_ROTATION_STEPS,
                HOTSPOT_ROTATION_HEAVY_FRAMES,
                HOTSPOT_ROTATION_LIGHT_FRAMES,
                HOTSPOT_ROTATION_FRAME_BYTES,
                HOTSPOT_ROTATION_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HOTSPOT_ROTATION_FRAME_BYTES,
            HOTSPOT_ROTATION_WINDOW,
        ));
        group.bench_function("spargio_tcp_8streams_rotating_hotspot", |b| {
            b.iter(|| {
                black_box(spargio.echo_hotspot_rotation(
                    HOTSPOT_ROTATION_STEPS,
                    HOTSPOT_ROTATION_HEAVY_FRAMES,
                    HOTSPOT_ROTATION_LIGHT_FRAMES,
                    HOTSPOT_ROTATION_FRAME_BYTES,
                    HOTSPOT_ROTATION_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HOTSPOT_ROTATION_FRAME_BYTES,
            HOTSPOT_ROTATION_WINDOW,
        ));
        group.bench_function("compio_tcp_8streams_rotating_hotspot", |b| {
            b.iter(|| {
                black_box(compio.echo_hotspot_rotation(
                    HOTSPOT_ROTATION_STEPS,
                    HOTSPOT_ROTATION_HEAVY_FRAMES,
                    HOTSPOT_ROTATION_LIGHT_FRAMES,
                    HOTSPOT_ROTATION_FRAME_BYTES,
                    HOTSPOT_ROTATION_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_pipeline_hotspot_rotation(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_pipeline_hotspot_rotation_4k_window32");
    group.throughput(Throughput::Bytes(
        (PIPELINE_TOTAL_FRAMES * PIPELINE_FRAME_BYTES) as u64,
    ));

    let warmup_frames = (PIPELINE_FRAMES_PER_STREAM / 8).max(1);
    let warmup_heavy = (PIPELINE_HEAVY_CPU_ITERS / 4).max(1);
    let warmup_light = (PIPELINE_LIGHT_CPU_ITERS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_pipeline_hotspot(
        warmup_frames,
        PIPELINE_FRAME_BYTES,
        PIPELINE_WINDOW,
        PIPELINE_ROTATE_EVERY,
        warmup_heavy,
        warmup_light,
    ));
    group.bench_function("tokio_tcp_pipeline_hotspot", |b| {
        b.iter(|| {
            black_box(tokio.echo_pipeline_hotspot(
                PIPELINE_FRAMES_PER_STREAM,
                PIPELINE_FRAME_BYTES,
                PIPELINE_WINDOW,
                PIPELINE_ROTATE_EVERY,
                PIPELINE_HEAVY_CPU_ITERS,
                PIPELINE_LIGHT_CPU_ITERS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_pipeline_hotspot(
            warmup_frames,
            PIPELINE_FRAME_BYTES,
            PIPELINE_WINDOW,
            PIPELINE_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("spargio_tcp_pipeline_hotspot", |b| {
            b.iter(|| {
                black_box(spargio.echo_pipeline_hotspot(
                    PIPELINE_FRAMES_PER_STREAM,
                    PIPELINE_FRAME_BYTES,
                    PIPELINE_WINDOW,
                    PIPELINE_ROTATE_EVERY,
                    PIPELINE_HEAVY_CPU_ITERS,
                    PIPELINE_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_pipeline_hotspot(
            warmup_frames,
            PIPELINE_FRAME_BYTES,
            PIPELINE_WINDOW,
            PIPELINE_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("compio_tcp_pipeline_hotspot", |b| {
            b.iter(|| {
                black_box(compio.echo_pipeline_hotspot(
                    PIPELINE_FRAMES_PER_STREAM,
                    PIPELINE_FRAME_BYTES,
                    PIPELINE_WINDOW,
                    PIPELINE_ROTATE_EVERY,
                    PIPELINE_HEAVY_CPU_ITERS,
                    PIPELINE_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(
    benches,
    bench_net_echo_rtt,
    bench_net_stream_throughput,
    bench_net_stream_imbalanced,
    bench_net_stream_hotspot_rotation,
    bench_net_pipeline_hotspot_rotation
);
#[cfg(unix)]
criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
