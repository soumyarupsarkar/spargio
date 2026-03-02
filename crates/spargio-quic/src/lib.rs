//! QUIC companion APIs for spargio runtimes.
//!
//! This crate keeps a practical `quinn` integration path while moving toward a
//! native long-term design. It now provides:
//! - a persistent Tokio bridge executor (no per-call runtime creation),
//! - endpoint/connection wrappers with stream/datagram helpers,
//! - per-endpoint metrics snapshots and in-flight backpressure limits,
//! - explicit local (`!Send`) and send-handoff connection wrappers.

use spargio::{RuntimeError, RuntimeHandle};
use std::collections::{HashMap, VecDeque};
use std::future::{Future, IntoFuture};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;

pub use quinn;
pub use quinn_proto;

const DEFAULT_MAX_INFLIGHT_OPS: usize = 1024;
const BRIDGE_WORKER_THREADS: usize = 2;
const NATIVE_EVENT_CAPACITY: usize = 1024;
static NEXT_NATIVE_ENDPOINT_ID: AtomicU64 = AtomicU64::new(1);
static BRIDGE_RUNTIME_SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);
static BRIDGE_RUNTIME_CONTEXT_ENTER_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn bridge_runtime_spawn_count() -> u64 {
    BRIDGE_RUNTIME_SPAWN_COUNT.load(Ordering::Relaxed)
}

pub fn reset_bridge_runtime_spawn_count() {
    BRIDGE_RUNTIME_SPAWN_COUNT.store(0, Ordering::Relaxed);
}

pub fn bridge_runtime_context_enter_count() -> u64 {
    BRIDGE_RUNTIME_CONTEXT_ENTER_COUNT.load(Ordering::Relaxed)
}

pub fn reset_bridge_runtime_context_enter_count() {
    BRIDGE_RUNTIME_CONTEXT_ENTER_COUNT.store(0, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuicOptions {
    timeout: Option<Duration>,
}

impl QuicOptions {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn timeout(self) -> Option<Duration> {
        self.timeout
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuicBridge {
    options: QuicOptions,
}

impl QuicBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(mut self, options: QuicOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(self) -> QuicOptions {
        self.options
    }

    pub async fn run<T, F, Fut>(&self, handle: &RuntimeHandle, f: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<T>> + Send + 'static,
    {
        run_with_options(handle, self.options, f).await
    }

    pub async fn with_endpoint<T, B, F, Fut>(
        &self,
        handle: &RuntimeHandle,
        build_endpoint: B,
        f: F,
    ) -> io::Result<T>
    where
        T: Send + 'static,
        B: FnOnce() -> io::Result<quinn::Endpoint> + Send + 'static,
        F: FnOnce(quinn::Endpoint) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<T>> + Send + 'static,
    {
        run_with_options(handle, self.options, move || async move {
            let endpoint = build_endpoint()?;
            f(endpoint).await
        })
        .await
    }
}

pub async fn run<T, F, Fut>(handle: &RuntimeHandle, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    run_with_options(handle, QuicOptions::default(), f).await
}

pub async fn run_with_options<T, F, Fut>(
    _handle: &RuntimeHandle,
    options: QuicOptions,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    spawn_on_bridge_runtime(options.timeout(), "quic bridge operation timed out", f).await
}

#[derive(Debug, Clone)]
pub struct NativeProtoDriverOptions {
    owner_shard: spargio::ShardId,
    max_pending_transmits: usize,
    server_config: Option<quinn::ServerConfig>,
}

impl Default for NativeProtoDriverOptions {
    fn default() -> Self {
        Self {
            owner_shard: 0,
            max_pending_transmits: DEFAULT_MAX_INFLIGHT_OPS,
            server_config: None,
        }
    }
}

impl NativeProtoDriverOptions {
    pub fn with_owner_shard(mut self, owner_shard: spargio::ShardId) -> Self {
        self.owner_shard = owner_shard;
        self
    }

    pub fn owner_shard(&self) -> spargio::ShardId {
        self.owner_shard
    }

    pub fn with_max_pending_transmits(mut self, max_pending_transmits: usize) -> Self {
        self.max_pending_transmits = max_pending_transmits;
        self
    }

    pub fn max_pending_transmits(&self) -> usize {
        self.max_pending_transmits
    }

    pub fn with_server_config(mut self, server_config: quinn::ServerConfig) -> Self {
        self.server_config = Some(server_config);
        self
    }

    pub fn server_config(&self) -> Option<quinn::ServerConfig> {
        self.server_config.clone()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NativeProtoDriverProbe {
    pub endpoint_id: u64,
    pub owner_shard: spargio::ShardId,
    pub executing_shard: spargio::ShardId,
    pub commands_processed: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NativeProtoTransmit {
    pub destination: SocketAddr,
    pub ecn: Option<quinn::EcnCodepoint>,
    pub size: usize,
    pub segment_size: Option<usize>,
    pub src_ip: Option<IpAddr>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NativeProtoIngressReport {
    pub generated_transmits: usize,
    pub queued_transmits: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NativeProtoTimerState {
    pub now: Duration,
    pub next_deadline: Option<Duration>,
    pub timeout_fires: u64,
    pub last_fired_generation: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NativeProtoStreamState {
    pub finished: bool,
    pub reset: bool,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NativeProtoConnectionState {
    pub closed: bool,
    pub streams_opened_uni: u64,
    pub streams_opened_bi: u64,
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NativeProtoTransportTuning {
    pub max_datagram_size: usize,
    pub send_window: u64,
    pub receive_window: u64,
    pub keep_alive_interval: Option<Duration>,
    pub mtu_discovery_enabled: bool,
}

impl Default for NativeProtoTransportTuning {
    fn default() -> Self {
        Self {
            max_datagram_size: 1200,
            send_window: 1024 * 1024,
            receive_window: 1024 * 1024,
            keep_alive_interval: None,
            mtu_discovery_enabled: true,
        }
    }
}

impl NativeProtoTransportTuning {
    pub fn with_max_datagram_size(mut self, max_datagram_size: usize) -> Self {
        self.max_datagram_size = max_datagram_size;
        self
    }

    pub fn with_send_window(mut self, send_window: u64) -> Self {
        self.send_window = send_window;
        self
    }

    pub fn with_receive_window(mut self, receive_window: u64) -> Self {
        self.receive_window = receive_window;
        self
    }

    pub fn with_keep_alive_interval(mut self, keep_alive_interval: Option<Duration>) -> Self {
        self.keep_alive_interval = keep_alive_interval;
        self
    }

    pub fn with_mtu_discovery_enabled(mut self, mtu_discovery_enabled: bool) -> Self {
        self.mtu_discovery_enabled = mtu_discovery_enabled;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NativeProtoStats {
    pub operations_total: u64,
    pub connections_registered: u64,
    pub connections_closed: u64,
    pub streams_opened_uni: u64,
    pub streams_opened_bi: u64,
    pub datagrams_ingested: u64,
    pub datagrams_oversized: u64,
    pub backpressure_hits: u64,
    pub timeouts_fired: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NativeProtoEvent {
    ConnectionRegistered { connection_id: u64 },
    ConnectionClosed { connection_id: u64 },
    TimeoutFired { generation: u64 },
    OversizedDatagram { size: usize, max_size: usize },
    Backpressure { scope: &'static str },
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NativeProtoFaultSpec {
    pub drop_inbound: bool,
    pub drop_egress: bool,
    pub reorder_egress: bool,
}

impl NativeProtoFaultSpec {
    pub fn with_drop_inbound(mut self, drop_inbound: bool) -> Self {
        self.drop_inbound = drop_inbound;
        self
    }

    pub fn with_drop_egress(mut self, drop_egress: bool) -> Self {
        self.drop_egress = drop_egress;
        self
    }

    pub fn with_reorder_egress(mut self, reorder_egress: bool) -> Self {
        self.reorder_egress = reorder_egress;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NativeProtoFaultStats {
    pub inbound_dropped: u64,
    pub egress_dropped: u64,
    pub egress_reorders: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NativeProtoRolloutStage {
    Experimental,
    Candidate,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeProtoPerfGate {
    pub max_p95_regression_pct: f64,
    pub max_p99_regression_pct: f64,
}

impl Default for NativeProtoPerfGate {
    fn default() -> Self {
        Self {
            max_p95_regression_pct: 15.0,
            max_p99_regression_pct: 20.0,
        }
    }
}

impl NativeProtoPerfGate {
    pub fn with_max_p95_regression_pct(mut self, value: f64) -> Self {
        self.max_p95_regression_pct = value;
        self
    }

    pub fn with_max_p99_regression_pct(mut self, value: f64) -> Self {
        self.max_p99_regression_pct = value;
        self
    }

    pub fn evaluate(
        self,
        baseline_p95: f64,
        sample_p95: f64,
        baseline_p99: f64,
        sample_p99: f64,
    ) -> NativeProtoPerfVerdict {
        let p95_regression_pct = if baseline_p95 <= 0.0 {
            0.0
        } else {
            ((sample_p95 - baseline_p95) / baseline_p95) * 100.0
        };
        let p99_regression_pct = if baseline_p99 <= 0.0 {
            0.0
        } else {
            ((sample_p99 - baseline_p99) / baseline_p99) * 100.0
        };
        let pass = p95_regression_pct <= self.max_p95_regression_pct
            && p99_regression_pct <= self.max_p99_regression_pct;
        NativeProtoPerfVerdict {
            pass,
            p95_regression_pct,
            p99_regression_pct,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeProtoPerfVerdict {
    pub pass: bool,
    pub p95_regression_pct: f64,
    pub p99_regression_pct: f64,
}

#[derive(Clone)]
pub struct NativeProtoDriver {
    endpoint_id: u64,
    owner_shard: spargio::ShardId,
    closed: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<NativeProtoCommand>,
}

impl NativeProtoDriver {
    pub fn rollout_stage() -> NativeProtoRolloutStage {
        NativeProtoRolloutStage::Experimental
    }

    pub async fn start(
        handle: &RuntimeHandle,
        options: NativeProtoDriverOptions,
    ) -> io::Result<Self> {
        if usize::from(options.owner_shard()) >= handle.shard_count() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("invalid native proto owner shard {}", options.owner_shard()),
            ));
        }
        if options.max_pending_transmits() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native proto max_pending_transmits must be > 0",
            ));
        }

        let endpoint_id = NEXT_NATIVE_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_task = closed.clone();
        let owner_shard = options.owner_shard();
        let max_pending_transmits = options.max_pending_transmits();
        let server_config = options.server_config();

        handle
            .spawn_local_on(owner_shard, move |ctx| async move {
                native_proto_driver_loop(
                    endpoint_id,
                    owner_shard,
                    ctx.shard_id(),
                    rx,
                    max_pending_transmits,
                    server_config,
                    closed_for_task,
                )
                .await;
            })
            .map_err(runtime_error_to_io)?;

        Ok(Self {
            endpoint_id,
            owner_shard,
            closed,
            tx,
        })
    }

    pub fn endpoint_id(&self) -> u64 {
        self.endpoint_id
    }

    pub fn owner_shard(&self) -> spargio::ShardId {
        self.owner_shard
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn to_local(&self) -> NativeProtoDriverLocal {
        NativeProtoDriverLocal {
            inner: Rc::new(self.clone()),
        }
    }

    pub fn to_send_handle(&self) -> NativeProtoDriverSend {
        NativeProtoDriverSend {
            inner: self.clone(),
        }
    }

    pub async fn probe(&self) -> io::Result<NativeProtoDriverProbe> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::Probe { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn allocate_connection_id(&self) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::AllocateConnectionId { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn allocate_stream_id(&self) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::AllocateStreamId { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn submit_datagram(
        &self,
        remote: SocketAddr,
        payload: Vec<u8>,
    ) -> io::Result<NativeProtoIngressReport> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::SubmitDatagram {
            remote,
            payload,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn drain_transmits(&self, max: usize) -> io::Result<Vec<NativeProtoTransmit>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::DrainTransmits {
            max,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn enqueue_transmit_for_test(&self, transmit: NativeProtoTransmit) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::EnqueueTransmitForTest {
            transmit,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn schedule_timeout(&self, after: Duration) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::ScheduleTimeout {
            after,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn advance_clock_for_test(&self, by: Duration) -> io::Result<NativeProtoTimerState> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::AdvanceClockForTest {
            by,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn timer_state(&self) -> io::Result<NativeProtoTimerState> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::TimerState { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn set_transport_tuning(&self, tuning: NativeProtoTransportTuning) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::SetTransportTuning {
            tuning,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn transport_tuning(&self) -> io::Result<NativeProtoTransportTuning> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::TransportTuning { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn stats(&self) -> io::Result<NativeProtoStats> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::Stats { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn drain_events(&self, max: usize) -> io::Result<Vec<NativeProtoEvent>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::DrainEvents {
            max,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn set_fault_spec(&self, spec: NativeProtoFaultSpec) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::SetFaultSpec {
            spec,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn fault_stats(&self) -> io::Result<NativeProtoFaultStats> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::FaultStats { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn register_connection_for_test(&self) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::RegisterConnectionForTest { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }

    pub async fn connect_for_test(
        &self,
        config: quinn::ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::ConnectForTest {
            config,
            remote,
            server_name: server_name.to_owned(),
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn close_connection_for_test(&self, connection_id: u64) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::CloseConnectionForTest {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn connection_state(
        &self,
        connection_id: u64,
    ) -> io::Result<NativeProtoConnectionState> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::ConnectionState {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn send_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
        payload: Vec<u8>,
    ) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::SendDatagramOnConnectionForTest {
            connection_id,
            payload,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn recv_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
    ) -> io::Result<Vec<u8>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::RecvDatagramOnConnectionForTest {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn open_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::OpenUniOnConnection {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn accept_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::AcceptUniOnConnection {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn open_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::OpenBiOnConnection {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn accept_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::AcceptBiOnConnection {
            connection_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn finish_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::FinishStream {
            connection_id,
            stream_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn reset_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::ResetStream {
            connection_id,
            stream_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn stream_state(
        &self,
        connection_id: u64,
        stream_id: u64,
    ) -> io::Result<NativeProtoStreamState> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::StreamState {
            connection_id,
            stream_id,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?
    }

    pub async fn shutdown(&self) -> io::Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeProtoCommand::Shutdown { reply: reply_tx })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))?;
        self.closed.store(true, Ordering::Release);
        self.tx.send(NativeProtoCommand::Closed).ok();
        Ok(())
    }

    fn send_command(&self, cmd: NativeProtoCommand) -> io::Result<()> {
        if self.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native proto driver closed",
            ));
        }
        self.tx
            .send(cmd)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native proto driver closed"))
    }
}

#[derive(Clone)]
pub struct NativeProtoDriverSend {
    inner: NativeProtoDriver,
}

impl NativeProtoDriverSend {
    pub fn endpoint_id(&self) -> u64 {
        self.inner.endpoint_id()
    }

    pub fn owner_shard(&self) -> spargio::ShardId {
        self.inner.owner_shard()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub async fn probe(&self) -> io::Result<NativeProtoDriverProbe> {
        self.inner.probe().await
    }

    pub async fn drain_transmits(&self, max: usize) -> io::Result<Vec<NativeProtoTransmit>> {
        self.inner.drain_transmits(max).await
    }

    pub async fn shutdown(&self) -> io::Result<()> {
        self.inner.shutdown().await
    }

    pub async fn register_connection_for_test(&self) -> io::Result<u64> {
        self.inner.register_connection_for_test().await
    }

    pub async fn connect_for_test(
        &self,
        config: quinn::ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> io::Result<u64> {
        self.inner.connect_for_test(config, remote, server_name).await
    }

    pub async fn close_connection_for_test(&self, connection_id: u64) -> io::Result<()> {
        self.inner.close_connection_for_test(connection_id).await
    }

    pub async fn connection_state(
        &self,
        connection_id: u64,
    ) -> io::Result<NativeProtoConnectionState> {
        self.inner.connection_state(connection_id).await
    }

    pub async fn send_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
        payload: Vec<u8>,
    ) -> io::Result<()> {
        self.inner
            .send_datagram_on_connection_for_test(connection_id, payload)
            .await
    }

    pub async fn recv_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
    ) -> io::Result<Vec<u8>> {
        self.inner
            .recv_datagram_on_connection_for_test(connection_id)
            .await
    }

    pub async fn open_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        self.inner.open_uni_on_connection(connection_id).await
    }

    pub async fn accept_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        self.inner.accept_uni_on_connection(connection_id).await
    }

    pub async fn open_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        self.inner.open_bi_on_connection(connection_id).await
    }

    pub async fn accept_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        self.inner.accept_bi_on_connection(connection_id).await
    }

    pub async fn finish_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        self.inner.finish_stream(connection_id, stream_id).await
    }

    pub async fn reset_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        self.inner.reset_stream(connection_id, stream_id).await
    }

    pub async fn stream_state(
        &self,
        connection_id: u64,
        stream_id: u64,
    ) -> io::Result<NativeProtoStreamState> {
        self.inner.stream_state(connection_id, stream_id).await
    }

    pub async fn set_transport_tuning(&self, tuning: NativeProtoTransportTuning) -> io::Result<()> {
        self.inner.set_transport_tuning(tuning).await
    }

    pub async fn transport_tuning(&self) -> io::Result<NativeProtoTransportTuning> {
        self.inner.transport_tuning().await
    }

    pub async fn stats(&self) -> io::Result<NativeProtoStats> {
        self.inner.stats().await
    }

    pub async fn drain_events(&self, max: usize) -> io::Result<Vec<NativeProtoEvent>> {
        self.inner.drain_events(max).await
    }

    pub async fn set_fault_spec(&self, spec: NativeProtoFaultSpec) -> io::Result<()> {
        self.inner.set_fault_spec(spec).await
    }

    pub async fn fault_stats(&self) -> io::Result<NativeProtoFaultStats> {
        self.inner.fault_stats().await
    }
}

#[derive(Clone)]
pub struct NativeProtoDriverLocal {
    inner: Rc<NativeProtoDriver>,
}

impl NativeProtoDriverLocal {
    pub fn endpoint_id(&self) -> u64 {
        self.inner.endpoint_id()
    }

    pub fn owner_shard(&self) -> spargio::ShardId {
        self.inner.owner_shard()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub fn to_send_handle(&self) -> NativeProtoDriverSend {
        self.inner.to_send_handle()
    }

    pub fn driver(&self) -> NativeProtoDriver {
        (*self.inner).clone()
    }

    pub async fn probe(&self) -> io::Result<NativeProtoDriverProbe> {
        self.inner.probe().await
    }

    pub async fn drain_transmits(&self, max: usize) -> io::Result<Vec<NativeProtoTransmit>> {
        self.inner.drain_transmits(max).await
    }

    pub async fn shutdown(&self) -> io::Result<()> {
        self.inner.shutdown().await
    }

    pub async fn register_connection_for_test(&self) -> io::Result<u64> {
        self.inner.register_connection_for_test().await
    }

    pub async fn connect_for_test(
        &self,
        config: quinn::ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> io::Result<u64> {
        self.inner.connect_for_test(config, remote, server_name).await
    }

    pub async fn close_connection_for_test(&self, connection_id: u64) -> io::Result<()> {
        self.inner.close_connection_for_test(connection_id).await
    }

    pub async fn connection_state(
        &self,
        connection_id: u64,
    ) -> io::Result<NativeProtoConnectionState> {
        self.inner.connection_state(connection_id).await
    }

    pub async fn send_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
        payload: Vec<u8>,
    ) -> io::Result<()> {
        self.inner
            .send_datagram_on_connection_for_test(connection_id, payload)
            .await
    }

    pub async fn recv_datagram_on_connection_for_test(
        &self,
        connection_id: u64,
    ) -> io::Result<Vec<u8>> {
        self.inner
            .recv_datagram_on_connection_for_test(connection_id)
            .await
    }

    pub async fn open_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        self.inner.open_uni_on_connection(connection_id).await
    }

    pub async fn accept_uni_on_connection(&self, connection_id: u64) -> io::Result<u64> {
        self.inner.accept_uni_on_connection(connection_id).await
    }

    pub async fn open_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        self.inner.open_bi_on_connection(connection_id).await
    }

    pub async fn accept_bi_on_connection(&self, connection_id: u64) -> io::Result<(u64, u64)> {
        self.inner.accept_bi_on_connection(connection_id).await
    }

    pub async fn finish_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        self.inner.finish_stream(connection_id, stream_id).await
    }

    pub async fn reset_stream(&self, connection_id: u64, stream_id: u64) -> io::Result<()> {
        self.inner.reset_stream(connection_id, stream_id).await
    }

    pub async fn stream_state(
        &self,
        connection_id: u64,
        stream_id: u64,
    ) -> io::Result<NativeProtoStreamState> {
        self.inner.stream_state(connection_id, stream_id).await
    }

    pub async fn set_transport_tuning(&self, tuning: NativeProtoTransportTuning) -> io::Result<()> {
        self.inner.set_transport_tuning(tuning).await
    }

    pub async fn transport_tuning(&self) -> io::Result<NativeProtoTransportTuning> {
        self.inner.transport_tuning().await
    }

    pub async fn stats(&self) -> io::Result<NativeProtoStats> {
        self.inner.stats().await
    }

    pub async fn drain_events(&self, max: usize) -> io::Result<Vec<NativeProtoEvent>> {
        self.inner.drain_events(max).await
    }

    pub async fn set_fault_spec(&self, spec: NativeProtoFaultSpec) -> io::Result<()> {
        self.inner.set_fault_spec(spec).await
    }

    pub async fn fault_stats(&self) -> io::Result<NativeProtoFaultStats> {
        self.inner.fault_stats().await
    }
}

enum NativeProtoCommand {
    Probe {
        reply: tokio::sync::oneshot::Sender<NativeProtoDriverProbe>,
    },
    AllocateConnectionId {
        reply: tokio::sync::oneshot::Sender<u64>,
    },
    AllocateStreamId {
        reply: tokio::sync::oneshot::Sender<u64>,
    },
    SubmitDatagram {
        remote: SocketAddr,
        payload: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<io::Result<NativeProtoIngressReport>>,
    },
    DrainTransmits {
        max: usize,
        reply: tokio::sync::oneshot::Sender<Vec<NativeProtoTransmit>>,
    },
    EnqueueTransmitForTest {
        transmit: NativeProtoTransmit,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    ScheduleTimeout {
        after: Duration,
        reply: tokio::sync::oneshot::Sender<u64>,
    },
    AdvanceClockForTest {
        by: Duration,
        reply: tokio::sync::oneshot::Sender<NativeProtoTimerState>,
    },
    TimerState {
        reply: tokio::sync::oneshot::Sender<NativeProtoTimerState>,
    },
    RegisterConnectionForTest {
        reply: tokio::sync::oneshot::Sender<u64>,
    },
    ConnectForTest {
        config: quinn::ClientConfig,
        remote: SocketAddr,
        server_name: String,
        reply: tokio::sync::oneshot::Sender<io::Result<u64>>,
    },
    CloseConnectionForTest {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    ConnectionState {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<NativeProtoConnectionState>>,
    },
    SendDatagramOnConnectionForTest {
        connection_id: u64,
        payload: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    RecvDatagramOnConnectionForTest {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<Vec<u8>>>,
    },
    OpenUniOnConnection {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<u64>>,
    },
    AcceptUniOnConnection {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<u64>>,
    },
    OpenBiOnConnection {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<(u64, u64)>>,
    },
    AcceptBiOnConnection {
        connection_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<(u64, u64)>>,
    },
    FinishStream {
        connection_id: u64,
        stream_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    ResetStream {
        connection_id: u64,
        stream_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    StreamState {
        connection_id: u64,
        stream_id: u64,
        reply: tokio::sync::oneshot::Sender<io::Result<NativeProtoStreamState>>,
    },
    SetTransportTuning {
        tuning: NativeProtoTransportTuning,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    TransportTuning {
        reply: tokio::sync::oneshot::Sender<NativeProtoTransportTuning>,
    },
    Stats {
        reply: tokio::sync::oneshot::Sender<NativeProtoStats>,
    },
    DrainEvents {
        max: usize,
        reply: tokio::sync::oneshot::Sender<Vec<NativeProtoEvent>>,
    },
    SetFaultSpec {
        spec: NativeProtoFaultSpec,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    FaultStats {
        reply: tokio::sync::oneshot::Sender<NativeProtoFaultStats>,
    },
    Shutdown {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    Closed,
}

#[derive(Default)]
struct NativeProtoConnectionPump {
    pending_uni_accept: VecDeque<u64>,
    pending_bi_accept: VecDeque<(u64, u64)>,
    pending_datagrams: VecDeque<Vec<u8>>,
    streams: HashMap<u64, NativeProtoStreamState>,
    state: NativeProtoConnectionState,
}

async fn native_proto_driver_loop(
    endpoint_id: u64,
    owner_shard: spargio::ShardId,
    executing_shard: spargio::ShardId,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<NativeProtoCommand>,
    max_pending_transmits: usize,
    server_config: Option<quinn::ServerConfig>,
    closed: Arc<AtomicBool>,
) {
    let allow_incoming_accept = server_config.is_some();
    let mut endpoint = quinn_proto::Endpoint::new(
        Arc::new(quinn_proto::EndpointConfig::default()),
        server_config.map(Arc::new),
        true,
        None,
    );
    let mut scratch = Vec::new();
    let mut pending_transmits: VecDeque<NativeProtoTransmit> = VecDeque::new();
    let mut commands_processed = 0u64;
    let mut next_connection_id = 1u64;
    let mut next_stream_id = 1u64;
    let mut now = Duration::ZERO;
    let epoch = std::time::Instant::now();
    let mut next_deadline: Option<(Duration, u64)> = None;
    let mut next_timer_generation = 1u64;
    let mut timeout_fires = 0u64;
    let mut last_fired_generation = None;
    let mut connections: HashMap<u64, NativeProtoConnectionPump> = HashMap::new();
    let mut proto_connections: HashMap<quinn_proto::ConnectionHandle, quinn_proto::Connection> =
        HashMap::new();
    let mut proto_connection_events: HashMap<
        quinn_proto::ConnectionHandle,
        VecDeque<quinn_proto::ConnectionEvent>,
    > = HashMap::new();
    let mut handle_by_connection_id: HashMap<u64, quinn_proto::ConnectionHandle> = HashMap::new();
    let mut connection_id_by_handle: HashMap<quinn_proto::ConnectionHandle, u64> = HashMap::new();
    let mut tuning = NativeProtoTransportTuning::default();
    let mut stats = NativeProtoStats::default();
    let mut events: VecDeque<NativeProtoEvent> = VecDeque::new();
    let mut fault_spec = NativeProtoFaultSpec::default();
    let mut fault_stats = NativeProtoFaultStats::default();

    while let Some(cmd) = rx.recv().await {
        stats.operations_total = stats.operations_total.saturating_add(1);
        match cmd {
            NativeProtoCommand::Probe { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let _ = reply.send(NativeProtoDriverProbe {
                    endpoint_id,
                    owner_shard,
                    executing_shard,
                    commands_processed,
                });
            }
            NativeProtoCommand::AllocateConnectionId { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let id = next_connection_id;
                next_connection_id = next_connection_id.saturating_add(1);
                let _ = reply.send(id);
            }
            NativeProtoCommand::AllocateStreamId { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let id = next_stream_id;
                next_stream_id = next_stream_id.saturating_add(1);
                let _ = reply.send(id);
            }
            NativeProtoCommand::SubmitDatagram {
                remote,
                payload,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut generated_transmits = 0usize;
                if fault_spec.drop_inbound {
                    fault_stats.inbound_dropped = fault_stats.inbound_dropped.saturating_add(1);
                    let _ = reply.send(Ok(NativeProtoIngressReport {
                        generated_transmits: 0,
                        queued_transmits: pending_transmits.len(),
                    }));
                    continue;
                }
                stats.datagrams_ingested = stats.datagrams_ingested.saturating_add(1);
                let now_std = native_proto_now(epoch, now);
                let event = endpoint.handle(
                    now_std,
                    remote,
                    None,
                    None,
                    bytes::BytesMut::from(payload.as_slice()),
                    &mut scratch,
                );
                let initial = match event {
                    Some(quinn_proto::DatagramEvent::Response(tx)) => {
                        generated_transmits = 1;
                        if fault_spec.drop_egress {
                            fault_stats.egress_dropped =
                                fault_stats.egress_dropped.saturating_add(1);
                            Ok(())
                        } else {
                            let payload = transmit_payload(scratch.as_slice(), tx.size);
                            match push_native_transmit(
                                &mut pending_transmits,
                                tx,
                                payload,
                                max_pending_transmits,
                            ) {
                                Ok(()) => Ok(()),
                                Err(err) => {
                                    if err.kind() == io::ErrorKind::WouldBlock {
                                        stats.backpressure_hits =
                                            stats.backpressure_hits.saturating_add(1);
                                        push_native_event(
                                            &mut events,
                                            NativeProtoEvent::Backpressure {
                                                scope: "egress_queue",
                                            },
                                        );
                                    }
                                    Err(err)
                                }
                            }
                        }
                    }
                    Some(quinn_proto::DatagramEvent::ConnectionEvent(handle, event)) => {
                        proto_connection_events
                            .entry(handle)
                            .or_default()
                            .push_back(event);
                        Ok(())
                    }
                    Some(quinn_proto::DatagramEvent::NewConnection(incoming)) => {
                        if allow_incoming_accept {
                            match endpoint.accept(incoming, now_std, &mut scratch, None) {
                                Ok((handle, connection)) => {
                                    let connection_id = next_connection_id;
                                    next_connection_id = next_connection_id.saturating_add(1);
                                    connections.entry(connection_id).or_default();
                                    handle_by_connection_id.insert(connection_id, handle);
                                    connection_id_by_handle.insert(handle, connection_id);
                                    proto_connections.insert(handle, connection);
                                    stats.connections_registered =
                                        stats.connections_registered.saturating_add(1);
                                    push_native_event(
                                        &mut events,
                                        NativeProtoEvent::ConnectionRegistered { connection_id },
                                    );
                                    Ok(())
                                }
                                Err(err) => {
                                    if let Some(tx) = err.response {
                                        generated_transmits = generated_transmits.saturating_add(1);
                                        if fault_spec.drop_egress {
                                            fault_stats.egress_dropped =
                                                fault_stats.egress_dropped.saturating_add(1);
                                            Ok(())
                                        } else {
                                            let payload = transmit_payload(scratch.as_slice(), tx.size);
                                            match push_native_transmit(
                                                &mut pending_transmits,
                                                tx,
                                                payload,
                                                max_pending_transmits,
                                            ) {
                                                Ok(()) => Ok(()),
                                                Err(err) => {
                                                    if err.kind() == io::ErrorKind::WouldBlock {
                                                        stats.backpressure_hits =
                                                            stats.backpressure_hits.saturating_add(1);
                                                        push_native_event(
                                                            &mut events,
                                                            NativeProtoEvent::Backpressure {
                                                                scope: "egress_queue",
                                                            },
                                                        );
                                                    }
                                                    Err(err)
                                                }
                                            }
                                        }
                                    } else {
                                        Ok(())
                                    }
                                }
                            }
                        } else {
                            let tx = endpoint.refuse(incoming, &mut scratch);
                            generated_transmits = generated_transmits.saturating_add(1);
                            if fault_spec.drop_egress {
                                fault_stats.egress_dropped =
                                    fault_stats.egress_dropped.saturating_add(1);
                                Ok(())
                            } else {
                                let payload = transmit_payload(scratch.as_slice(), tx.size);
                                match push_native_transmit(
                                    &mut pending_transmits,
                                    tx,
                                    payload,
                                    max_pending_transmits,
                                ) {
                                    Ok(()) => Ok(()),
                                    Err(err) => {
                                        if err.kind() == io::ErrorKind::WouldBlock {
                                            stats.backpressure_hits =
                                                stats.backpressure_hits.saturating_add(1);
                                            push_native_event(
                                                &mut events,
                                                NativeProtoEvent::Backpressure {
                                                    scope: "egress_queue",
                                                },
                                            );
                                        }
                                        Err(err)
                                    }
                                }
                            }
                        }
                    }
                    None => Ok(()),
                };
                scratch.clear();
                let result = match initial {
                    Ok(()) => match drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        Ok(drive_generated) => {
                            generated_transmits = generated_transmits.saturating_add(drive_generated);
                            Ok(NativeProtoIngressReport {
                                generated_transmits,
                                queued_transmits: pending_transmits.len(),
                            })
                        }
                        Err(err) => Err(err),
                    },
                    Err(err) => Err(err),
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::DrainTransmits { max, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let take = max.min(pending_transmits.len());
                let mut drained = pending_transmits.drain(..take).collect::<Vec<_>>();
                if fault_spec.reorder_egress && drained.len() > 1 {
                    drained.reverse();
                    fault_stats.egress_reorders = fault_stats.egress_reorders.saturating_add(1);
                }
                let _ = reply.send(drained);
            }
            NativeProtoCommand::EnqueueTransmitForTest { transmit, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if fault_spec.drop_egress {
                    fault_stats.egress_dropped = fault_stats.egress_dropped.saturating_add(1);
                    Ok(())
                } else if pending_transmits.len() >= max_pending_transmits {
                    stats.backpressure_hits = stats.backpressure_hits.saturating_add(1);
                    push_native_event(
                        &mut events,
                        NativeProtoEvent::Backpressure {
                            scope: "egress_queue",
                        },
                    );
                    Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "native proto egress queue full",
                    ))
                } else {
                    pending_transmits.push_back(transmit);
                    Ok(())
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::ScheduleTimeout { after, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let generation = next_timer_generation;
                next_timer_generation = next_timer_generation.saturating_add(1);
                let deadline = now.saturating_add(after);
                next_deadline = Some((deadline, generation));
                let _ = reply.send(generation);
            }
            NativeProtoCommand::AdvanceClockForTest { by, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                now = now.saturating_add(by);
                let now_std = native_proto_now(epoch, now);
                if let Some((deadline, generation)) = next_deadline {
                    if now >= deadline {
                        timeout_fires = timeout_fires.saturating_add(1);
                        last_fired_generation = Some(generation);
                        next_deadline = None;
                        stats.timeouts_fired = stats.timeouts_fired.saturating_add(1);
                        push_native_event(
                            &mut events,
                            NativeProtoEvent::TimeoutFired { generation },
                        );
                    }
                }
                for connection in proto_connections.values_mut() {
                    if let Some(deadline) = connection.poll_timeout() {
                        if deadline <= now_std {
                            connection.handle_timeout(now_std);
                        }
                    }
                }
                let _ = drive_native_proto_connections(
                    now_std,
                    &mut endpoint,
                    &mut connections,
                    &mut handle_by_connection_id,
                    &mut connection_id_by_handle,
                    &mut proto_connections,
                    &mut proto_connection_events,
                    &mut pending_transmits,
                    max_pending_transmits,
                    fault_spec,
                    &mut fault_stats,
                    &mut stats,
                    &mut events,
                    &mut scratch,
                );
                let _ = reply.send(NativeProtoTimerState {
                    now,
                    next_deadline: next_deadline.map(|(deadline, _)| deadline),
                    timeout_fires,
                    last_fired_generation,
                });
            }
            NativeProtoCommand::TimerState { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let _ = reply.send(NativeProtoTimerState {
                    now,
                    next_deadline: next_deadline.map(|(deadline, _)| deadline),
                    timeout_fires,
                    last_fired_generation,
                });
            }
            NativeProtoCommand::RegisterConnectionForTest { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let id = next_connection_id;
                next_connection_id = next_connection_id.saturating_add(1);
                connections.entry(id).or_default();
                stats.connections_registered = stats.connections_registered.saturating_add(1);
                push_native_event(
                    &mut events,
                    NativeProtoEvent::ConnectionRegistered { connection_id: id },
                );
                let _ = reply.send(id);
            }
            NativeProtoCommand::ConnectForTest {
                config,
                remote,
                server_name,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let now_std = native_proto_now(epoch, now);
                let result =
                    match endpoint.connect(now_std, config, remote, &server_name) {
                        Ok((handle, connection)) => {
                            let connection_id = next_connection_id;
                            next_connection_id = next_connection_id.saturating_add(1);
                            connections.entry(connection_id).or_default();
                            handle_by_connection_id.insert(connection_id, handle);
                            connection_id_by_handle.insert(handle, connection_id);
                            proto_connections.insert(handle, connection);
                            stats.connections_registered =
                                stats.connections_registered.saturating_add(1);
                            push_native_event(
                                &mut events,
                                NativeProtoEvent::ConnectionRegistered { connection_id },
                            );
                            match drive_native_proto_connections(
                                now_std,
                                &mut endpoint,
                                &mut connections,
                                &mut handle_by_connection_id,
                                &mut connection_id_by_handle,
                                &mut proto_connections,
                                &mut proto_connection_events,
                                &mut pending_transmits,
                                max_pending_transmits,
                                fault_spec,
                                &mut fault_stats,
                                &mut stats,
                                &mut events,
                                &mut scratch,
                            ) {
                                Ok(_) => Ok(connection_id),
                                Err(err) => Err(err),
                            }
                        }
                        Err(err) => Err(quinn_connect_error_to_io(err)),
                    };
                let _ = reply.send(result);
            }
            NativeProtoCommand::CloseConnectionForTest {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut emit_connection_closed = false;
                let mut result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if !conn.state.closed {
                        emit_connection_closed = true;
                    }
                    conn.state.closed = true;
                    conn.pending_uni_accept.clear();
                    conn.pending_bi_accept.clear();
                    conn.pending_datagrams.clear();
                    if let Some(handle) = handle_by_connection_id.get(&connection_id).copied() {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            let now_std = native_proto_now(epoch, now);
                            proto_connection.close(
                                now_std,
                                quinn_proto::VarInt::from_u32(0),
                                bytes::Bytes::new(),
                            );
                            should_drive = true;
                        }
                    }
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if emit_connection_closed {
                    stats.connections_closed = stats.connections_closed.saturating_add(1);
                    push_native_event(
                        &mut events,
                        NativeProtoEvent::ConnectionClosed { connection_id },
                    );
                }
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                if let Some(handle) = handle_by_connection_id.remove(&connection_id) {
                    connection_id_by_handle.remove(&handle);
                    proto_connections.remove(&handle);
                    proto_connection_events.remove(&handle);
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::ConnectionState {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if let Some(conn) = connections.get(&connection_id) {
                    Ok(conn.state)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::SendDatagramOnConnectionForTest {
                connection_id,
                payload,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut result = if payload.len() > tuning.max_datagram_size {
                    stats.datagrams_oversized = stats.datagrams_oversized.saturating_add(1);
                    push_native_event(
                        &mut events,
                        NativeProtoEvent::OversizedDatagram {
                            size: payload.len(),
                            max_size: tuning.max_datagram_size,
                        },
                    );
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "datagram size {} exceeds max_datagram_size {}",
                            payload.len(),
                            tuning.max_datagram_size
                        ),
                    ))
                } else if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            match proto_connection
                                .datagrams()
                                .send(bytes::Bytes::copy_from_slice(&payload), false)
                            {
                                Ok(()) => {
                                    conn.state.datagrams_sent =
                                        conn.state.datagrams_sent.saturating_add(1);
                                    should_drive = true;
                                    Ok(())
                                }
                                Err(err) => Err(proto_send_datagram_error_to_io(err)),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        conn.pending_datagrams.push_back(payload);
                        conn.state.datagrams_sent = conn.state.datagrams_sent.saturating_add(1);
                        Ok(())
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::RecvDatagramOnConnectionForTest {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            if let Some(payload) = proto_connection.datagrams().recv() {
                                conn.state.datagrams_received =
                                    conn.state.datagrams_received.saturating_add(1);
                                Ok(payload.to_vec())
                            } else {
                                Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "no pending datagram for connection",
                                ))
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else if let Some(payload) = conn.pending_datagrams.pop_front() {
                        conn.state.datagrams_received =
                            conn.state.datagrams_received.saturating_add(1);
                        Ok(payload)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "no pending datagram for connection",
                        ))
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::OpenUniOnConnection {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            match proto_connection.streams().open(quinn_proto::Dir::Uni) {
                                Some(stream_id) => {
                                    let stream_id = u64::from(stream_id);
                                    conn.streams.entry(stream_id).or_default();
                                    conn.state.streams_opened_uni =
                                        conn.state.streams_opened_uni.saturating_add(1);
                                    stats.streams_opened_uni =
                                        stats.streams_opened_uni.saturating_add(1);
                                    should_drive = true;
                                    Ok(stream_id)
                                }
                                None => Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "native proto uni stream quota exhausted",
                                )),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        let stream_id = next_stream_id;
                        next_stream_id = next_stream_id.saturating_add(1);
                        conn.pending_uni_accept.push_back(stream_id);
                        conn.streams.entry(stream_id).or_default();
                        conn.state.streams_opened_uni =
                            conn.state.streams_opened_uni.saturating_add(1);
                        stats.streams_opened_uni = stats.streams_opened_uni.saturating_add(1);
                        Ok(stream_id)
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::AcceptUniOnConnection {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            match proto_connection.streams().accept(quinn_proto::Dir::Uni) {
                                Some(stream_id) => {
                                    let stream_id = u64::from(stream_id);
                                    conn.streams.entry(stream_id).or_default();
                                    Ok(stream_id)
                                }
                                None => Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "no pending uni stream for accept",
                                )),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        conn.pending_uni_accept.pop_front().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "no pending uni stream for accept",
                            )
                        })
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::OpenBiOnConnection {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            match proto_connection.streams().open(quinn_proto::Dir::Bi) {
                                Some(stream_id) => {
                                    let stream_id = u64::from(stream_id);
                                    let pair = (stream_id, stream_id);
                                    conn.streams.entry(stream_id).or_default();
                                    conn.state.streams_opened_bi =
                                        conn.state.streams_opened_bi.saturating_add(1);
                                    stats.streams_opened_bi =
                                        stats.streams_opened_bi.saturating_add(1);
                                    should_drive = true;
                                    Ok(pair)
                                }
                                None => Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "native proto bi stream quota exhausted",
                                )),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        let stream_id = next_stream_id;
                        next_stream_id = next_stream_id.saturating_add(1);
                        let pair = (stream_id, stream_id);
                        conn.pending_bi_accept.push_back(pair);
                        conn.streams.entry(stream_id).or_default();
                        conn.state.streams_opened_bi =
                            conn.state.streams_opened_bi.saturating_add(1);
                        stats.streams_opened_bi = stats.streams_opened_bi.saturating_add(1);
                        Ok(pair)
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::AcceptBiOnConnection {
                connection_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            match proto_connection.streams().accept(quinn_proto::Dir::Bi) {
                                Some(stream_id) => {
                                    let stream_id = u64::from(stream_id);
                                    let pair = (stream_id, stream_id);
                                    conn.streams.entry(stream_id).or_default();
                                    Ok(pair)
                                }
                                None => Err(io::Error::new(
                                    io::ErrorKind::WouldBlock,
                                    "no pending bi stream for accept",
                                )),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        conn.pending_bi_accept.pop_front().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "no pending bi stream for accept",
                            )
                        })
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::FinishStream {
                connection_id,
                stream_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            let stream_id_proto = match proto_stream_id_from_u64(stream_id) {
                                Ok(id) => id,
                                Err(err) => {
                                    let _ = reply.send(Err(err));
                                    continue;
                                }
                            };
                            match proto_connection.send_stream(stream_id_proto).finish() {
                                Ok(()) => {
                                    if let Some(state) = conn.streams.get_mut(&stream_id) {
                                        state.finished = true;
                                    }
                                    should_drive = true;
                                    Ok(())
                                }
                                Err(err) => Err(proto_finish_error_to_io(err)),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        if let Some(state) = conn.streams.get_mut(&stream_id) {
                            state.finished = true;
                            Ok(())
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("unknown native proto stream {stream_id}"),
                            ))
                        }
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::ResetStream {
                connection_id,
                stream_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let mut should_drive = false;
                let mut result = if let Some(conn) = connections.get_mut(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else if let Some(handle) = handle_by_connection_id.get(&connection_id).copied()
                    {
                        if let Some(proto_connection) = proto_connections.get_mut(&handle) {
                            let stream_id_proto = match proto_stream_id_from_u64(stream_id) {
                                Ok(id) => id,
                                Err(err) => {
                                    let _ = reply.send(Err(err));
                                    continue;
                                }
                            };
                            match proto_connection
                                .send_stream(stream_id_proto)
                                .reset(quinn_proto::VarInt::from_u32(0))
                            {
                                Ok(()) => {
                                    if let Some(state) = conn.streams.get_mut(&stream_id) {
                                        state.finished = true;
                                        state.reset = true;
                                    }
                                    should_drive = true;
                                    Ok(())
                                }
                                Err(_) => Err(io::Error::new(
                                    io::ErrorKind::NotFound,
                                    format!("unknown native proto stream {stream_id}"),
                                )),
                            }
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("missing native proto handle for connection {connection_id}"),
                            ))
                        }
                    } else {
                        if let Some(state) = conn.streams.get_mut(&stream_id) {
                            state.finished = true;
                            state.reset = true;
                            Ok(())
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("unknown native proto stream {stream_id}"),
                            ))
                        }
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                if result.is_ok() && should_drive {
                    let now_std = native_proto_now(epoch, now);
                    if let Err(err) = drive_native_proto_connections(
                        now_std,
                        &mut endpoint,
                        &mut connections,
                        &mut handle_by_connection_id,
                        &mut connection_id_by_handle,
                        &mut proto_connections,
                        &mut proto_connection_events,
                        &mut pending_transmits,
                        max_pending_transmits,
                        fault_spec,
                        &mut fault_stats,
                        &mut stats,
                        &mut events,
                        &mut scratch,
                    ) {
                        result = Err(err);
                    }
                }
                let _ = reply.send(result);
            }
            NativeProtoCommand::StreamState {
                connection_id,
                stream_id,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if let Some(conn) = connections.get(&connection_id) {
                    if conn.state.closed {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("native proto connection {connection_id} is closed"),
                        ))
                    } else {
                        conn.streams.get(&stream_id).copied().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("unknown native proto stream {stream_id}"),
                            )
                        })
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("unknown native proto connection {connection_id}"),
                    ))
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::SetTransportTuning {
                tuning: next_tuning,
                reply,
            } => {
                commands_processed = commands_processed.saturating_add(1);
                let result = if next_tuning.max_datagram_size == 0 {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "max_datagram_size must be > 0",
                    ))
                } else {
                    tuning = next_tuning;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            NativeProtoCommand::TransportTuning { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let _ = reply.send(tuning);
            }
            NativeProtoCommand::Stats { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let _ = reply.send(stats);
            }
            NativeProtoCommand::DrainEvents { max, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let take = max.min(events.len());
                let drained = events.drain(..take).collect::<Vec<_>>();
                let _ = reply.send(drained);
            }
            NativeProtoCommand::SetFaultSpec { spec, reply } => {
                commands_processed = commands_processed.saturating_add(1);
                fault_spec = spec;
                let _ = reply.send(Ok(()));
            }
            NativeProtoCommand::FaultStats { reply } => {
                commands_processed = commands_processed.saturating_add(1);
                let _ = reply.send(fault_stats);
            }
            NativeProtoCommand::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
            NativeProtoCommand::Closed => {
                break;
            }
        }
    }

    closed.store(true, Ordering::Release);
}

fn push_native_transmit(
    pending_transmits: &mut VecDeque<NativeProtoTransmit>,
    tx: quinn_proto::Transmit,
    payload: Vec<u8>,
    max_pending_transmits: usize,
) -> io::Result<()> {
    if pending_transmits.len() >= max_pending_transmits {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "native proto egress queue full",
        ));
    }
    pending_transmits.push_back(NativeProtoTransmit {
        destination: tx.destination,
        ecn: tx.ecn,
        size: tx.size,
        segment_size: tx.segment_size,
        src_ip: tx.src_ip,
        payload,
    });
    Ok(())
}

fn transmit_payload(buf: &[u8], size: usize) -> Vec<u8> {
    let end = size.min(buf.len());
    buf[..end].to_vec()
}

fn native_proto_now(epoch: std::time::Instant, now: Duration) -> std::time::Instant {
    epoch.checked_add(now).unwrap_or(epoch)
}

fn drive_native_proto_connections(
    now: std::time::Instant,
    endpoint: &mut quinn_proto::Endpoint,
    connections: &mut HashMap<u64, NativeProtoConnectionPump>,
    handle_by_connection_id: &mut HashMap<u64, quinn_proto::ConnectionHandle>,
    connection_id_by_handle: &mut HashMap<quinn_proto::ConnectionHandle, u64>,
    proto_connections: &mut HashMap<quinn_proto::ConnectionHandle, quinn_proto::Connection>,
    proto_connection_events: &mut HashMap<
        quinn_proto::ConnectionHandle,
        VecDeque<quinn_proto::ConnectionEvent>,
    >,
    pending_transmits: &mut VecDeque<NativeProtoTransmit>,
    max_pending_transmits: usize,
    fault_spec: NativeProtoFaultSpec,
    fault_stats: &mut NativeProtoFaultStats,
    stats: &mut NativeProtoStats,
    events: &mut VecDeque<NativeProtoEvent>,
    scratch: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut generated = 0usize;
    loop {
        let mut endpoint_events = Vec::new();
        let mut retired_handles = Vec::new();
        let handles = proto_connections.keys().copied().collect::<Vec<_>>();
        for handle in handles {
            let Some(connection) = proto_connections.get_mut(&handle) else {
                continue;
            };
            if let Some(queue) = proto_connection_events.get_mut(&handle) {
                while let Some(event) = queue.pop_front() {
                    connection.handle_event(event);
                }
            }
            while let Some(event) = connection.poll_endpoint_events() {
                endpoint_events.push((handle, event));
            }
            while let Some(tx) = connection.poll_transmit(now, 1, scratch) {
                generated = generated.saturating_add(1);
                if fault_spec.drop_egress {
                    fault_stats.egress_dropped = fault_stats.egress_dropped.saturating_add(1);
                    scratch.clear();
                    continue;
                }
                let payload = transmit_payload(scratch.as_slice(), tx.size);
                if let Err(err) =
                    push_native_transmit(pending_transmits, tx, payload, max_pending_transmits)
                {
                    if err.kind() == io::ErrorKind::WouldBlock {
                        stats.backpressure_hits = stats.backpressure_hits.saturating_add(1);
                        push_native_event(
                            events,
                            NativeProtoEvent::Backpressure {
                                scope: "egress_queue",
                            },
                        );
                    }
                    scratch.clear();
                    return Err(err);
                }
                scratch.clear();
            }
            while let Some(event) = connection.poll() {
                if let quinn_proto::Event::ConnectionLost { .. } = event {
                    if let Some(connection_id) = connection_id_by_handle.get(&handle).copied() {
                        let mut emit_connection_closed = false;
                        if let Some(connection) = connections.get_mut(&connection_id) {
                            if !connection.state.closed {
                                emit_connection_closed = true;
                            }
                            connection.state.closed = true;
                            connection.pending_uni_accept.clear();
                            connection.pending_bi_accept.clear();
                            connection.pending_datagrams.clear();
                        }
                        if emit_connection_closed {
                            stats.connections_closed = stats.connections_closed.saturating_add(1);
                            push_native_event(
                                events,
                                NativeProtoEvent::ConnectionClosed { connection_id },
                            );
                        }
                        handle_by_connection_id.remove(&connection_id);
                    }
                    retired_handles.push(handle);
                    break;
                }
            }
        }
        if endpoint_events.is_empty() {
            for handle in retired_handles {
                proto_connections.remove(&handle);
                proto_connection_events.remove(&handle);
                connection_id_by_handle.remove(&handle);
            }
            break;
        }
        for (handle, endpoint_event) in endpoint_events {
            if let Some(connection_event) = endpoint.handle_event(handle, endpoint_event) {
                if let Some(connection) = proto_connections.get_mut(&handle) {
                    connection.handle_event(connection_event);
                } else {
                    proto_connection_events
                        .entry(handle)
                        .or_default()
                        .push_back(connection_event);
                }
            }
        }
        for handle in retired_handles {
            proto_connections.remove(&handle);
            proto_connection_events.remove(&handle);
            connection_id_by_handle.remove(&handle);
        }
    }
    Ok(generated)
}

fn proto_stream_id_from_u64(stream_id: u64) -> io::Result<quinn_proto::StreamId> {
    let var = quinn_proto::VarInt::from_u64(stream_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid native proto stream id {stream_id}"),
        )
    })?;
    Ok(quinn_proto::StreamId::from(var))
}

fn proto_send_datagram_error_to_io(err: quinn_proto::SendDatagramError) -> io::Error {
    match err {
        quinn_proto::SendDatagramError::UnsupportedByPeer => io::Error::new(
            io::ErrorKind::InvalidData,
            "native proto peer does not support datagrams",
        ),
        quinn_proto::SendDatagramError::Disabled => io::Error::new(
            io::ErrorKind::Unsupported,
            "native proto datagrams are disabled",
        ),
        quinn_proto::SendDatagramError::TooLarge => {
            io::Error::new(io::ErrorKind::InvalidInput, "native proto datagram too large")
        }
        quinn_proto::SendDatagramError::Blocked(_) => io::Error::new(
            io::ErrorKind::WouldBlock,
            "native proto datagram send blocked",
        ),
    }
}

fn proto_finish_error_to_io(err: quinn_proto::FinishError) -> io::Error {
    match err {
        quinn_proto::FinishError::Stopped(code) => io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("native proto stream stopped by peer ({code})"),
        ),
        quinn_proto::FinishError::ClosedStream => {
            io::Error::new(io::ErrorKind::NotFound, "native proto stream is closed")
        }
    }
}

fn push_native_event(events: &mut VecDeque<NativeProtoEvent>, event: NativeProtoEvent) {
    if events.len() >= NATIVE_EVENT_CAPACITY {
        events.pop_front();
    }
    events.push_back(event);
}

#[derive(Debug, Clone, Copy)]
pub struct QuicEndpointOptions {
    connect_timeout: Option<Duration>,
    accept_timeout: Option<Duration>,
    operation_timeout: Option<Duration>,
    max_inflight_ops: usize,
    backend: QuicBackend,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum QuicBackend {
    Native,
    Bridge,
}

impl Default for QuicBackend {
    fn default() -> Self {
        Self::Native
    }
}

impl Default for QuicEndpointOptions {
    fn default() -> Self {
        Self {
            connect_timeout: None,
            accept_timeout: None,
            operation_timeout: None,
            max_inflight_ops: DEFAULT_MAX_INFLIGHT_OPS,
            backend: QuicBackend::default(),
        }
    }
}

impl QuicEndpointOptions {
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    pub fn with_accept_timeout(mut self, timeout: Duration) -> Self {
        self.accept_timeout = Some(timeout);
        self
    }

    pub fn with_operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = Some(timeout);
        self
    }

    pub fn with_max_inflight_ops(mut self, max_inflight_ops: usize) -> Self {
        self.max_inflight_ops = max_inflight_ops;
        self
    }

    pub fn with_backend(mut self, backend: QuicBackend) -> Self {
        self.backend = backend;
        self
    }

    pub fn connect_timeout(self) -> Option<Duration> {
        self.connect_timeout
    }

    pub fn accept_timeout(self) -> Option<Duration> {
        self.accept_timeout
    }

    pub fn operation_timeout(self) -> Option<Duration> {
        self.operation_timeout
    }

    pub fn max_inflight_ops(self) -> usize {
        self.max_inflight_ops
    }

    pub fn backend(self) -> QuicBackend {
        self.backend
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct QuicMetricsSnapshot {
    pub endpoints_created: u64,
    pub connects_started: u64,
    pub connects_succeeded: u64,
    pub connects_failed: u64,
    pub connect_timeouts: u64,
    pub accepts_started: u64,
    pub accepts_succeeded: u64,
    pub accepts_failed: u64,
    pub accept_timeouts: u64,
    pub backpressure_rejections: u64,
    pub connections_opened: u64,
    pub streams_opened_uni: u64,
    pub streams_opened_bi: u64,
    pub streams_accepted_uni: u64,
    pub streams_accepted_bi: u64,
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
    pub endpoint_closes: u64,
    pub connection_closes: u64,
    pub operation_timeouts: u64,
    pub native_ops_dispatched: u64,
    pub bridge_ops_dispatched: u64,
}

#[derive(Debug, Clone, Default)]
pub struct QuicMetrics {
    inner: Arc<QuicMetricsInner>,
}

impl QuicMetrics {
    pub fn snapshot(&self) -> QuicMetricsSnapshot {
        QuicMetricsSnapshot {
            endpoints_created: self.inner.endpoints_created.load(Ordering::Relaxed),
            connects_started: self.inner.connects_started.load(Ordering::Relaxed),
            connects_succeeded: self.inner.connects_succeeded.load(Ordering::Relaxed),
            connects_failed: self.inner.connects_failed.load(Ordering::Relaxed),
            connect_timeouts: self.inner.connect_timeouts.load(Ordering::Relaxed),
            accepts_started: self.inner.accepts_started.load(Ordering::Relaxed),
            accepts_succeeded: self.inner.accepts_succeeded.load(Ordering::Relaxed),
            accepts_failed: self.inner.accepts_failed.load(Ordering::Relaxed),
            accept_timeouts: self.inner.accept_timeouts.load(Ordering::Relaxed),
            backpressure_rejections: self.inner.backpressure_rejections.load(Ordering::Relaxed),
            connections_opened: self.inner.connections_opened.load(Ordering::Relaxed),
            streams_opened_uni: self.inner.streams_opened_uni.load(Ordering::Relaxed),
            streams_opened_bi: self.inner.streams_opened_bi.load(Ordering::Relaxed),
            streams_accepted_uni: self.inner.streams_accepted_uni.load(Ordering::Relaxed),
            streams_accepted_bi: self.inner.streams_accepted_bi.load(Ordering::Relaxed),
            datagrams_sent: self.inner.datagrams_sent.load(Ordering::Relaxed),
            datagrams_received: self.inner.datagrams_received.load(Ordering::Relaxed),
            endpoint_closes: self.inner.endpoint_closes.load(Ordering::Relaxed),
            connection_closes: self.inner.connection_closes.load(Ordering::Relaxed),
            operation_timeouts: self.inner.operation_timeouts.load(Ordering::Relaxed),
            native_ops_dispatched: self.inner.native_ops_dispatched.load(Ordering::Relaxed),
            bridge_ops_dispatched: self.inner.bridge_ops_dispatched.load(Ordering::Relaxed),
        }
    }

    fn inc_endpoints_created(&self) {
        self.inner.endpoints_created.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connects_started(&self) {
        self.inner.connects_started.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connects_succeeded(&self) {
        self.inner
            .connects_succeeded
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connects_failed(&self) {
        self.inner.connects_failed.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connect_timeouts(&self) {
        self.inner.connect_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_accepts_started(&self) {
        self.inner.accepts_started.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_accepts_succeeded(&self) {
        self.inner.accepts_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_accepts_failed(&self) {
        self.inner.accepts_failed.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_accept_timeouts(&self) {
        self.inner.accept_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_backpressure_rejections(&self) {
        self.inner
            .backpressure_rejections
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connections_opened(&self) {
        self.inner
            .connections_opened
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_opened_uni(&self) {
        self.inner
            .streams_opened_uni
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_opened_bi(&self) {
        self.inner.streams_opened_bi.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_accepted_uni(&self) {
        self.inner
            .streams_accepted_uni
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_accepted_bi(&self) {
        self.inner
            .streams_accepted_bi
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_datagrams_sent(&self) {
        self.inner.datagrams_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_datagrams_received(&self) {
        self.inner
            .datagrams_received
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_endpoint_closes(&self) {
        self.inner.endpoint_closes.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connection_closes(&self) {
        self.inner.connection_closes.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_operation_timeouts(&self) {
        self.inner
            .operation_timeouts
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_native_ops_dispatched(&self) {
        self.inner
            .native_ops_dispatched
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_bridge_ops_dispatched(&self) {
        self.inner
            .bridge_ops_dispatched
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct QuicMetricsInner {
    endpoints_created: AtomicU64,
    connects_started: AtomicU64,
    connects_succeeded: AtomicU64,
    connects_failed: AtomicU64,
    connect_timeouts: AtomicU64,
    accepts_started: AtomicU64,
    accepts_succeeded: AtomicU64,
    accepts_failed: AtomicU64,
    accept_timeouts: AtomicU64,
    backpressure_rejections: AtomicU64,
    connections_opened: AtomicU64,
    streams_opened_uni: AtomicU64,
    streams_opened_bi: AtomicU64,
    streams_accepted_uni: AtomicU64,
    streams_accepted_bi: AtomicU64,
    datagrams_sent: AtomicU64,
    datagrams_received: AtomicU64,
    endpoint_closes: AtomicU64,
    connection_closes: AtomicU64,
    operation_timeouts: AtomicU64,
    native_ops_dispatched: AtomicU64,
    bridge_ops_dispatched: AtomicU64,
}

#[derive(Clone)]
struct NativeEndpointDispatch {
    tx: tokio::sync::mpsc::UnboundedSender<NativeEndpointCommand>,
    closed: Arc<AtomicBool>,
}

impl NativeEndpointDispatch {
    fn start(endpoint: quinn::Endpoint) -> io::Result<Self> {
        let executor = bridge_executor()?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_task = closed.clone();
        executor
            .runtime
            .spawn(native_endpoint_dispatch_loop(endpoint, rx, closed_for_task));
        Ok(Self { tx, closed })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn connect(
        &self,
        config: Option<quinn::ClientConfig>,
        addr: SocketAddr,
        server_name: String,
        timeout: Option<Duration>,
    ) -> io::Result<quinn::Connection> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeEndpointCommand::Connect {
            config,
            addr,
            server_name,
            timeout,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native endpoint dispatch closed"))?
    }

    async fn connect_with(
        &self,
        config: quinn::ClientConfig,
        addr: SocketAddr,
        server_name: String,
        timeout: Option<Duration>,
    ) -> io::Result<quinn::Connection> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeEndpointCommand::Connect {
            config: Some(config),
            addr,
            server_name,
            timeout,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native endpoint dispatch closed"))?
    }

    async fn accept(
        &self,
        accept_timeout: Option<Duration>,
        op_timeout: Option<Duration>,
    ) -> io::Result<Option<quinn::Connection>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeEndpointCommand::Accept {
            accept_timeout,
            op_timeout,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native endpoint dispatch closed"))?
    }

    async fn wait_idle(&self, timeout: Option<Duration>) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeEndpointCommand::WaitIdle {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native endpoint dispatch closed"))?
    }

    fn shutdown_nowait(&self) {
        self.closed.store(true, Ordering::Release);
        self.tx.send(NativeEndpointCommand::Shutdown).ok();
    }

    fn send_command(&self, cmd: NativeEndpointCommand) -> io::Result<()> {
        if self.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native endpoint dispatch closed",
            ));
        }
        self.tx
            .send(cmd)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "native endpoint dispatch closed"))
    }
}

enum NativeEndpointCommand {
    Connect {
        config: Option<quinn::ClientConfig>,
        addr: SocketAddr,
        server_name: String,
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<quinn::Connection>>,
    },
    Accept {
        accept_timeout: Option<Duration>,
        op_timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<Option<quinn::Connection>>>,
    },
    WaitIdle {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    Shutdown,
}

async fn run_tokio_timeout<T, F>(
    timeout: Option<Duration>,
    fut: F,
    timeout_msg: &'static str,
) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    match timeout {
        Some(limit) => match tokio::time::timeout(limit, fut).await {
            Ok(out) => out,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg)),
        },
        None => fut.await,
    }
}

async fn native_endpoint_dispatch_loop(
    endpoint: quinn::Endpoint,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<NativeEndpointCommand>,
    closed: Arc<AtomicBool>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            NativeEndpointCommand::Connect {
                config,
                addr,
                server_name,
                timeout,
                reply,
            } => {
                let endpoint = endpoint.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        let connecting = match config {
                            Some(config) => endpoint
                                .connect_with(config, addr, &server_name)
                                .map_err(quinn_connect_error_to_io)?,
                            None => endpoint
                                .connect(addr, &server_name)
                                .map_err(quinn_connect_error_to_io)?,
                        };
                        connecting.await.map_err(quinn_connection_error_to_io)
                    },
                    "quic connect timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeEndpointCommand::Accept {
                accept_timeout,
                op_timeout,
                reply,
            } => {
                let endpoint = endpoint.clone();
                let out = run_tokio_timeout(
                    accept_timeout,
                    async move {
                        let incoming = endpoint.accept().await;
                        let Some(incoming) = incoming else {
                            return Ok(None);
                        };
                        let connected = run_tokio_timeout(
                            op_timeout,
                            async move {
                                incoming
                                    .into_future()
                                    .await
                                    .map_err(quinn_connection_error_to_io)
                            },
                            "quic incoming handshake timed out",
                        )
                        .await?;
                        Ok(Some(connected))
                    },
                    "quic accept timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeEndpointCommand::WaitIdle { timeout, reply } => {
                let endpoint = endpoint.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        endpoint.wait_idle().await;
                        Ok(())
                    },
                    "quic endpoint wait_idle timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeEndpointCommand::Shutdown => break,
        }
    }
    closed.store(true, Ordering::Release);
}

#[derive(Clone)]
struct NativeConnectionDispatch {
    tx: tokio::sync::mpsc::UnboundedSender<NativeConnectionCommand>,
    closed: Arc<AtomicBool>,
}

impl NativeConnectionDispatch {
    fn start(connection: quinn::Connection) -> io::Result<Self> {
        let executor = bridge_executor()?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_task = closed.clone();
        executor
            .runtime
            .spawn(native_connection_dispatch_loop(connection, rx, closed_for_task));
        Ok(Self { tx, closed })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn closed(&self, timeout: Option<Duration>) -> io::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::Closed {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    async fn open_uni(&self, timeout: Option<Duration>) -> io::Result<quinn::SendStream> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::OpenUni {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    async fn open_bi(
        &self,
        timeout: Option<Duration>,
    ) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::OpenBi {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    async fn accept_uni(&self, timeout: Option<Duration>) -> io::Result<quinn::RecvStream> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::AcceptUni {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    async fn accept_bi(
        &self,
        timeout: Option<Duration>,
    ) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::AcceptBi {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    async fn read_datagram(&self, timeout: Option<Duration>) -> io::Result<Vec<u8>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.send_command(NativeConnectionCommand::ReadDatagram {
            timeout,
            reply: reply_tx,
        })?;
        reply_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })?
    }

    fn send_command(&self, cmd: NativeConnectionCommand) -> io::Result<()> {
        if self.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "native connection dispatch closed",
            ));
        }
        self.tx.send(cmd).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "native connection dispatch closed")
        })
    }
}

enum NativeConnectionCommand {
    Closed {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    OpenUni {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<quinn::SendStream>>,
    },
    OpenBi {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<(quinn::SendStream, quinn::RecvStream)>>,
    },
    AcceptUni {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<quinn::RecvStream>>,
    },
    AcceptBi {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<(quinn::SendStream, quinn::RecvStream)>>,
    },
    ReadDatagram {
        timeout: Option<Duration>,
        reply: tokio::sync::oneshot::Sender<io::Result<Vec<u8>>>,
    },
}

async fn native_connection_dispatch_loop(
    connection: quinn::Connection,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<NativeConnectionCommand>,
    closed: Arc<AtomicBool>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            NativeConnectionCommand::Closed { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        let err = connection.closed().await;
                        if matches!(err, quinn::ConnectionError::LocallyClosed) {
                            Ok(())
                        } else {
                            Err(quinn_connection_error_to_io(err))
                        }
                    },
                    "quic connection closed() wait timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeConnectionCommand::OpenUni { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        connection
                            .open_uni()
                            .await
                            .map_err(quinn_connection_error_to_io)
                    },
                    "quic open_uni timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeConnectionCommand::OpenBi { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        connection
                            .open_bi()
                            .await
                            .map_err(quinn_connection_error_to_io)
                    },
                    "quic open_bi timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeConnectionCommand::AcceptUni { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        connection
                            .accept_uni()
                            .await
                            .map_err(quinn_connection_error_to_io)
                    },
                    "quic accept_uni timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeConnectionCommand::AcceptBi { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        connection
                            .accept_bi()
                            .await
                            .map_err(quinn_connection_error_to_io)
                    },
                    "quic accept_bi timed out",
                )
                .await;
                let _ = reply.send(out);
            }
            NativeConnectionCommand::ReadDatagram { timeout, reply } => {
                let connection = connection.clone();
                let out = run_tokio_timeout(
                    timeout,
                    async move {
                        connection
                            .read_datagram()
                            .await
                            .map(|payload| payload.to_vec())
                            .map_err(quinn_connection_error_to_io)
                    },
                    "quic read_datagram timed out",
                )
                .await;
                let _ = reply.send(out);
            }
        }
    }
    closed.store(true, Ordering::Release);
}

#[derive(Clone)]
pub struct QuicEndpoint {
    endpoint: Option<quinn::Endpoint>,
    native_dispatch: Option<NativeEndpointDispatch>,
    default_client_config: Arc<Mutex<Option<quinn::ClientConfig>>>,
    options: QuicEndpointOptions,
    metrics: QuicMetrics,
    limiter: Arc<InflightLimiter>,
}

impl QuicEndpoint {
    pub fn server(server_config: quinn::ServerConfig, bind_addr: SocketAddr) -> io::Result<Self> {
        Self::server_with_options(server_config, bind_addr, QuicEndpointOptions::default())
    }

    pub fn server_with_options(
        server_config: quinn::ServerConfig,
        bind_addr: SocketAddr,
        options: QuicEndpointOptions,
    ) -> io::Result<Self> {
        validate_endpoint_options(options)?;
        let endpoint = match options.backend() {
            QuicBackend::Native => native_server_endpoint(server_config, bind_addr)?,
            QuicBackend::Bridge => {
                with_bridge_runtime_context(|| quinn::Endpoint::server(server_config, bind_addr))?
            }
        };
        Self::from_endpoint_with_options(endpoint, options)
    }

    pub fn client(bind_addr: SocketAddr) -> io::Result<Self> {
        Self::client_with_options(bind_addr, QuicEndpointOptions::default())
    }

    pub fn client_with_options(
        bind_addr: SocketAddr,
        options: QuicEndpointOptions,
    ) -> io::Result<Self> {
        validate_endpoint_options(options)?;
        let endpoint = match options.backend() {
            QuicBackend::Native => native_client_endpoint(bind_addr)?,
            QuicBackend::Bridge => with_bridge_runtime_context(|| quinn::Endpoint::client(bind_addr))?,
        };
        Self::from_endpoint_with_options(endpoint, options)
    }

    pub fn from_endpoint(endpoint: quinn::Endpoint) -> io::Result<Self> {
        Self::from_endpoint_with_options(endpoint, QuicEndpointOptions::default())
    }

    pub fn from_endpoint_with_options(
        endpoint: quinn::Endpoint,
        options: QuicEndpointOptions,
    ) -> io::Result<Self> {
        validate_endpoint_options(options)?;
        let metrics = QuicMetrics::default();
        metrics.inc_endpoints_created();
        let native_dispatch = if options.backend() == QuicBackend::Native {
            Some(NativeEndpointDispatch::start(endpoint.clone())?)
        } else {
            None
        };
        Ok(Self {
            endpoint: Some(endpoint),
            native_dispatch,
            default_client_config: Arc::new(Mutex::new(None)),
            options,
            metrics,
            limiter: Arc::new(InflightLimiter::new(
                options.max_inflight_ops(),
                "quic endpoint",
            )),
        })
    }

    pub fn options(&self) -> QuicEndpointOptions {
        self.options
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint_ref().local_addr()
    }

    pub fn set_default_client_config(&mut self, config: quinn::ClientConfig) {
        {
            let mut default_client_config = self
                .default_client_config
                .lock()
                .expect("quic endpoint client config lock poisoned");
            *default_client_config = Some(config.clone());
        }
        self.endpoint_mut().set_default_client_config(config);
    }

    pub fn set_server_config(&mut self, config: Option<quinn::ServerConfig>) {
        self.endpoint_mut().set_server_config(config);
    }

    pub fn metrics_snapshot(&self) -> QuicMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn metrics(&self) -> QuicMetrics {
        self.metrics.clone()
    }

    pub async fn connect(&self, addr: SocketAddr, server_name: &str) -> io::Result<QuicConnection> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        self.metrics.inc_connects_started();
        let server_name = server_name.to_owned();
        let connected = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                let default_config = self
                    .default_client_config
                    .lock()
                    .expect("quic endpoint client config lock poisoned")
                    .clone();
                self.native_dispatch_ref()?
                    .connect(default_config, addr, server_name, self.options.connect_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let endpoint = self.endpoint_ref().clone();
                spawn_on_bridge_runtime(
                    self.options.connect_timeout(),
                    "quic connect timed out",
                    move || async move {
                        let connecting = endpoint
                            .connect(addr, &server_name)
                            .map_err(quinn_connect_error_to_io)?;
                        connecting.await.map_err(quinn_connection_error_to_io)
                    },
                )
                .await
            }
        };
        match connected {
            Ok(connection) => match self.wrap_connection(connection) {
                Ok(connection) => {
                    self.metrics.inc_connects_succeeded();
                    Ok(connection)
                }
                Err(err) => {
                    self.metrics.inc_connects_failed();
                    Err(err)
                }
            },
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_connect_timeouts();
                    self.metrics.inc_operation_timeouts();
                } else {
                    self.metrics.inc_connects_failed();
                }
                Err(err)
            }
        }
    }

    pub async fn connect_with(
        &self,
        config: quinn::ClientConfig,
        addr: SocketAddr,
        server_name: &str,
    ) -> io::Result<QuicConnection> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        self.metrics.inc_connects_started();
        let server_name = server_name.to_owned();
        let connected = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .connect_with(config, addr, server_name, self.options.connect_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let endpoint = self.endpoint_ref().clone();
                spawn_on_bridge_runtime(
                    self.options.connect_timeout(),
                    "quic connect timed out",
                    move || async move {
                        let connecting = endpoint
                            .connect_with(config, addr, &server_name)
                            .map_err(quinn_connect_error_to_io)?;
                        connecting.await.map_err(quinn_connection_error_to_io)
                    },
                )
                .await
            }
        };
        match connected {
            Ok(connection) => match self.wrap_connection(connection) {
                Ok(connection) => {
                    self.metrics.inc_connects_succeeded();
                    Ok(connection)
                }
                Err(err) => {
                    self.metrics.inc_connects_failed();
                    Err(err)
                }
            },
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_connect_timeouts();
                    self.metrics.inc_operation_timeouts();
                } else {
                    self.metrics.inc_connects_failed();
                }
                Err(err)
            }
        }
    }

    pub async fn accept(&self) -> io::Result<Option<QuicConnection>> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        self.metrics.inc_accepts_started();
        let op_timeout = self.options.operation_timeout();
        let accepted = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .accept(self.options.accept_timeout(), op_timeout)
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let endpoint = self.endpoint_ref().clone();
                spawn_on_bridge_runtime(
                    self.options.accept_timeout(),
                    "quic accept timed out",
                    move || async move {
                        let incoming = endpoint.accept().await;
                        let Some(incoming) = incoming else {
                            return Ok(None);
                        };
                        let connected = await_with_timeout(
                            op_timeout,
                            incoming.into_future(),
                            "quic incoming handshake timed out",
                        )
                        .await?;
                        let connection = connected.map_err(quinn_connection_error_to_io)?;
                        Ok(Some(connection))
                    },
                )
                .await
            }
        };
        match accepted {
            Ok(Some(connection)) => match self.wrap_connection(connection) {
                Ok(connection) => {
                    self.metrics.inc_accepts_succeeded();
                    Ok(Some(connection))
                }
                Err(err) => {
                    self.metrics.inc_accepts_failed();
                    Err(err)
                }
            },
            Ok(None) => Ok(None),
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_accept_timeouts();
                    self.metrics.inc_operation_timeouts();
                } else {
                    self.metrics.inc_accepts_failed();
                }
                Err(err)
            }
        }
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        self.endpoint_ref().close(code.into(), reason);
        self.metrics.inc_endpoint_closes();
    }

    pub async fn wait_idle(&self) -> io::Result<()> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let waited = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .wait_idle(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let endpoint = self.endpoint_ref().clone();
                spawn_on_bridge_runtime(
                    self.options.operation_timeout(),
                    "quic endpoint wait_idle timed out",
                    move || async move {
                        endpoint.wait_idle().await;
                        Ok(())
                    },
                )
                .await
            }
        };
        if let Err(ref err) = waited {
            if err.kind() == io::ErrorKind::TimedOut {
                self.metrics.inc_operation_timeouts();
            }
        }
        waited
    }

    fn wrap_connection(&self, connection: quinn::Connection) -> io::Result<QuicConnection> {
        let native_dispatch = if self.options.backend() == QuicBackend::Native {
            Some(NativeConnectionDispatch::start(connection.clone())?)
        } else {
            None
        };
        self.metrics.inc_connections_opened();
        Ok(QuicConnection {
            connection,
            native_dispatch,
            options: self.options,
            metrics: self.metrics.clone(),
            limiter: self.limiter.clone(),
        })
    }

    fn endpoint_ref(&self) -> &quinn::Endpoint {
        self.endpoint
            .as_ref()
            .expect("quic endpoint has already been dropped")
    }

    fn endpoint_mut(&mut self) -> &mut quinn::Endpoint {
        self.endpoint
            .as_mut()
            .expect("quic endpoint has already been dropped")
    }

    fn native_dispatch_ref(&self) -> io::Result<&NativeEndpointDispatch> {
        self.native_dispatch.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "native endpoint dispatch not initialized",
            )
        })
    }
}

impl Drop for QuicEndpoint {
    fn drop(&mut self) {
        if let Some(dispatch) = self.native_dispatch.take() {
            dispatch.shutdown_nowait();
        }
        let Some(endpoint) = self.endpoint.take() else {
            return;
        };
        match self.options.backend() {
            QuicBackend::Bridge => {
                if let Ok(executor) = bridge_executor() {
                    let _entered = executor.runtime.enter();
                    drop(endpoint);
                } else {
                    drop(endpoint);
                }
            }
            QuicBackend::Native => {
                drop(endpoint);
            }
        }
    }
}

#[derive(Clone)]
pub struct QuicConnection {
    connection: quinn::Connection,
    native_dispatch: Option<NativeConnectionDispatch>,
    options: QuicEndpointOptions,
    metrics: QuicMetrics,
    limiter: Arc<InflightLimiter>,
}

impl QuicConnection {
    pub fn stable_id(&self) -> usize {
        self.connection.stable_id()
    }

    pub fn stats(&self) -> quinn::ConnectionStats {
        self.connection.stats()
    }

    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    pub fn datagram_send_buffer_space(&self) -> usize {
        self.connection.datagram_send_buffer_space()
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        self.connection.close(code.into(), reason);
        self.metrics.inc_connection_closes();
    }

    pub async fn closed(&self) -> io::Result<()> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let out = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .closed(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let closed = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.closed(),
                    "quic connection closed() wait timed out",
                )
                .await?;
                if matches!(closed, quinn::ConnectionError::LocallyClosed) {
                    Ok(())
                } else {
                    Err(quinn_connection_error_to_io(closed))
                }
            }
        };
        if let Err(ref err) = out {
            if err.kind() == io::ErrorKind::TimedOut {
                self.metrics.inc_operation_timeouts();
            }
        }
        out
    }

    pub async fn open_uni(&self) -> io::Result<quinn::SendStream> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let stream = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .open_uni(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let opened = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.open_uni(),
                    "quic open_uni timed out",
                )
                .await?;
                opened.map_err(quinn_connection_error_to_io)
            }
        };
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        self.metrics.inc_streams_opened_uni();
        Ok(stream)
    }

    pub async fn open_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let streams = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .open_bi(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let opened = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.open_bi(),
                    "quic open_bi timed out",
                )
                .await?;
                opened.map_err(quinn_connection_error_to_io)
            }
        };
        let streams = match streams {
            Ok(streams) => streams,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        self.metrics.inc_streams_opened_bi();
        Ok(streams)
    }

    pub async fn accept_uni(&self) -> io::Result<quinn::RecvStream> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let stream = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .accept_uni(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let accepted = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.accept_uni(),
                    "quic accept_uni timed out",
                )
                .await?;
                accepted.map_err(quinn_connection_error_to_io)
            }
        };
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        self.metrics.inc_streams_accepted_uni();
        Ok(stream)
    }

    pub async fn accept_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let streams = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .accept_bi(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let accepted = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.accept_bi(),
                    "quic accept_bi timed out",
                )
                .await?;
                accepted.map_err(quinn_connection_error_to_io)
            }
        };
        let streams = match streams {
            Ok(streams) => streams,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        self.metrics.inc_streams_accepted_bi();
        Ok(streams)
    }

    pub fn send_datagram<D>(&self, data: D) -> io::Result<()>
    where
        D: Into<Vec<u8>>,
    {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        match self.options.backend() {
            QuicBackend::Native => self.metrics.inc_native_ops_dispatched(),
            QuicBackend::Bridge => self.metrics.inc_bridge_ops_dispatched(),
        }
        self.connection
            .send_datagram(data.into().into())
            .map_err(quinn_send_datagram_error_to_io)?;
        self.metrics.inc_datagrams_sent();
        Ok(())
    }

    pub async fn read_datagram(&self) -> io::Result<Vec<u8>> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let payload = match self.options.backend() {
            QuicBackend::Native => {
                self.metrics.inc_native_ops_dispatched();
                self.native_dispatch_ref()?
                    .read_datagram(self.options.operation_timeout())
                    .await
            }
            QuicBackend::Bridge => {
                self.metrics.inc_bridge_ops_dispatched();
                let read = await_with_timeout(
                    self.options.operation_timeout(),
                    self.connection.read_datagram(),
                    "quic read_datagram timed out",
                )
                .await?;
                read.map(|payload| payload.to_vec())
                    .map_err(quinn_connection_error_to_io)
            }
        };
        let payload = match payload {
            Ok(payload) => payload,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        self.metrics.inc_datagrams_received();
        Ok(payload)
    }

    pub fn to_local(&self) -> LocalQuicConnection {
        LocalQuicConnection {
            inner: Rc::new(self.clone()),
        }
    }

    pub fn to_send_handle(&self) -> QuicSendConnection {
        QuicSendConnection {
            inner: self.clone(),
        }
    }

    fn native_dispatch_ref(&self) -> io::Result<&NativeConnectionDispatch> {
        self.native_dispatch.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "native connection dispatch not initialized",
            )
        })
    }
}

#[derive(Clone)]
pub struct QuicSendConnection {
    inner: QuicConnection,
}

impl QuicSendConnection {
    pub fn stable_id(&self) -> usize {
        self.inner.stable_id()
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        self.inner.close(code, reason);
    }

    pub async fn open_uni(&self) -> io::Result<quinn::SendStream> {
        self.inner.open_uni().await
    }

    pub async fn open_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner.open_bi().await
    }

    pub async fn accept_uni(&self) -> io::Result<quinn::RecvStream> {
        self.inner.accept_uni().await
    }

    pub async fn accept_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner.accept_bi().await
    }

    pub fn send_datagram<D>(&self, data: D) -> io::Result<()>
    where
        D: Into<Vec<u8>>,
    {
        self.inner.send_datagram(data)
    }

    pub async fn read_datagram(&self) -> io::Result<Vec<u8>> {
        self.inner.read_datagram().await
    }

    pub fn connection(&self) -> QuicConnection {
        self.inner.clone()
    }
}

#[derive(Clone)]
pub struct LocalQuicConnection {
    inner: Rc<QuicConnection>,
}

impl LocalQuicConnection {
    pub fn stable_id(&self) -> usize {
        self.inner.stable_id()
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        self.inner.close(code, reason);
    }

    pub async fn open_uni(&self) -> io::Result<quinn::SendStream> {
        self.inner.open_uni().await
    }

    pub async fn open_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner.open_bi().await
    }

    pub async fn accept_uni(&self) -> io::Result<quinn::RecvStream> {
        self.inner.accept_uni().await
    }

    pub async fn accept_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        self.inner.accept_bi().await
    }

    pub fn send_datagram<D>(&self, data: D) -> io::Result<()>
    where
        D: Into<Vec<u8>>,
    {
        self.inner.send_datagram(data)
    }

    pub async fn read_datagram(&self) -> io::Result<Vec<u8>> {
        self.inner.read_datagram().await
    }

    pub fn to_send_handle(&self) -> QuicSendConnection {
        self.inner.to_send_handle()
    }

    pub fn connection(&self) -> QuicConnection {
        (*self.inner).clone()
    }
}

#[derive(Debug)]
struct InflightLimiter {
    max: usize,
    in_flight: AtomicUsize,
    label: &'static str,
}

impl InflightLimiter {
    fn new(max: usize, label: &'static str) -> Self {
        Self {
            max,
            in_flight: AtomicUsize::new(0),
            label,
        }
    }

    fn acquire(self: &Arc<Self>) -> io::Result<InflightPermit> {
        if self.max == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} max_inflight_ops must be > 0", self.label),
            ));
        }
        loop {
            let current = self.in_flight.load(Ordering::Acquire);
            if current >= self.max {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} backpressure: max in-flight reached", self.label),
                ));
            }
            if self
                .in_flight
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(InflightPermit {
                    limiter: self.clone(),
                });
            }
        }
    }
}

#[derive(Debug)]
struct InflightPermit {
    limiter: Arc<InflightLimiter>,
}

impl Drop for InflightPermit {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone)]
struct BridgeTokioRuntime {
    handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for BridgeTokioRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeTokioRuntime").finish()
    }
}

impl quinn::Runtime for BridgeTokioRuntime {
    fn new_timer(&self, instant: std::time::Instant) -> Pin<Box<dyn quinn::AsyncTimer>> {
        let _entered = self.handle.enter();
        Box::pin(tokio::time::sleep_until(instant.into()))
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.handle.spawn(future);
    }

    fn wrap_udp_socket(
        &self,
        sock: std::net::UdpSocket,
    ) -> io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
        let _entered = self.handle.enter();
        Ok(Arc::new(BridgeUdpSocket {
            inner: quinn::udp::UdpSocketState::new((&sock).into())?,
            io: tokio::net::UdpSocket::from_std(sock)?,
        }))
    }

    fn now(&self) -> std::time::Instant {
        let _entered = self.handle.enter();
        tokio::time::Instant::now().into_std()
    }
}

#[derive(Debug)]
struct BridgeUdpSocket {
    io: tokio::net::UdpSocket,
    inner: quinn::udp::UdpSocketState,
}

impl quinn::AsyncUdpSocket for BridgeUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(BridgeUdpPoller { socket: self })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        self.io.try_io(tokio::io::Interest::WRITABLE, || {
            self.inner.send((&self.io).into(), transmit)
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            if let Ok(res) = self.io.try_io(tokio::io::Interest::READABLE, || {
                self.inner.recv((&self.io).into(), bufs, meta)
            }) {
                return Poll::Ready(Ok(res));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_gso_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.gro_segments()
    }
}

#[derive(Debug)]
struct BridgeUdpPoller {
    socket: Arc<BridgeUdpSocket>,
}

impl quinn::UdpPoller for BridgeUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().socket.io.poll_send_ready(cx)
    }
}

#[derive(Debug)]
struct BridgeExecutor {
    runtime: tokio::runtime::Runtime,
    limiter: Arc<InflightLimiter>,
}

static BRIDGE_EXECUTOR: OnceLock<Result<BridgeExecutor, String>> = OnceLock::new();

fn bridge_executor() -> io::Result<&'static BridgeExecutor> {
    match BRIDGE_EXECUTOR.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(BRIDGE_WORKER_THREADS)
            .enable_all()
            .build()
            .map(|runtime| BridgeExecutor {
                runtime,
                limiter: Arc::new(InflightLimiter::new(
                    DEFAULT_MAX_INFLIGHT_OPS,
                    "quic bridge executor",
                )),
            })
            .map_err(|err| format!("tokio runtime build failed: {err}"))
    }) {
        Ok(executor) => Ok(executor),
        Err(msg) => Err(io::Error::other(msg.clone())),
    }
}

fn bridge_runtime_for_native_endpoint() -> io::Result<Arc<dyn quinn::Runtime>> {
    let executor = bridge_executor()?;
    Ok(Arc::new(BridgeTokioRuntime {
        handle: executor.runtime.handle().clone(),
    }))
}

fn native_server_endpoint(
    server_config: quinn::ServerConfig,
    bind_addr: SocketAddr,
) -> io::Result<quinn::Endpoint> {
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        bridge_runtime_for_native_endpoint()?,
    )
}

fn native_client_endpoint(bind_addr: SocketAddr) -> io::Result<quinn::Endpoint> {
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        bridge_runtime_for_native_endpoint()?,
    )
}

fn with_bridge_runtime_context<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    BRIDGE_RUNTIME_CONTEXT_ENTER_COUNT.fetch_add(1, Ordering::Relaxed);
    let executor = bridge_executor()?;
    let _bridge_permit = executor.limiter.acquire()?;
    let _entered = executor.runtime.enter();
    f()
}

async fn spawn_on_bridge_runtime<T, F, Fut>(
    timeout: Option<Duration>,
    timeout_msg: &'static str,
    f: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    BRIDGE_RUNTIME_SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
    let executor = bridge_executor()?;
    let _bridge_permit = executor.limiter.acquire()?;
    let mut join = executor.runtime.spawn(async move { f().await });
    let joined = match timeout {
        Some(limit) => match spargio::timeout(limit, async { (&mut join).await }).await {
            Ok(result) => result,
            Err(_) => {
                join.abort();
                return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg));
            }
        },
        None => join.await,
    };
    joined.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "quic bridge task canceled"))?
}

async fn await_with_timeout<T, F>(
    timeout: Option<Duration>,
    fut: F,
    timeout_msg: &'static str,
) -> io::Result<T>
where
    F: Future<Output = T>,
{
    match timeout {
        Some(limit) => match spargio::timeout(limit, fut).await {
            Ok(out) => Ok(out),
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg)),
        },
        None => Ok(fut.await),
    }
}

fn validate_endpoint_options(options: QuicEndpointOptions) -> io::Result<()> {
    if options.max_inflight_ops() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quic max_inflight_ops must be > 0",
        ));
    }
    Ok(())
}

fn acquire_with_metrics(
    limiter: &Arc<InflightLimiter>,
    metrics: &QuicMetrics,
) -> io::Result<InflightPermit> {
    match limiter.acquire() {
        Ok(permit) => Ok(permit),
        Err(err) => {
            if err.kind() == io::ErrorKind::WouldBlock {
                metrics.inc_backpressure_rejections();
            }
            Err(err)
        }
    }
}

fn runtime_error_to_io(err: RuntimeError) -> io::Error {
    match err {
        RuntimeError::InvalidConfig(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
        RuntimeError::ThreadSpawn(io) => io,
        RuntimeError::InvalidShard(shard) => {
            io::Error::new(io::ErrorKind::NotFound, format!("invalid shard {shard}"))
        }
        RuntimeError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"),
        RuntimeError::Overloaded => io::Error::new(io::ErrorKind::WouldBlock, "runtime overloaded"),
        RuntimeError::UnsupportedBackend(msg) => io::Error::new(io::ErrorKind::Unsupported, msg),
        RuntimeError::IoUringInit(io) => io,
    }
}

fn quinn_connect_error_to_io(err: quinn::ConnectError) -> io::Error {
    match err {
        quinn::ConnectError::EndpointStopping => {
            io::Error::new(io::ErrorKind::BrokenPipe, "quic endpoint stopping")
        }
        quinn::ConnectError::CidsExhausted => io::Error::other("quic cids exhausted"),
        quinn::ConnectError::InvalidServerName(msg) => io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid server name: {msg}"),
        ),
        quinn::ConnectError::InvalidRemoteAddress(addr) => io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid remote address: {addr}"),
        ),
        quinn::ConnectError::NoDefaultClientConfig => io::Error::new(
            io::ErrorKind::InvalidInput,
            "no default quic client config set",
        ),
        quinn::ConnectError::UnsupportedVersion => {
            io::Error::new(io::ErrorKind::Unsupported, "unsupported quic version")
        }
    }
}

fn quinn_connection_error_to_io(err: quinn::ConnectionError) -> io::Error {
    io::Error::from(err)
}

fn quinn_send_datagram_error_to_io(err: quinn::SendDatagramError) -> io::Error {
    match err {
        quinn::SendDatagramError::UnsupportedByPeer => io::Error::new(
            io::ErrorKind::Unsupported,
            "peer does not support quic datagrams",
        ),
        quinn::SendDatagramError::Disabled => io::Error::new(
            io::ErrorKind::Unsupported,
            "local quic datagram support disabled",
        ),
        quinn::SendDatagramError::TooLarge => {
            io::Error::new(io::ErrorKind::InvalidInput, "quic datagram too large")
        }
        quinn::SendDatagramError::ConnectionLost(err) => quinn_connection_error_to_io(err),
    }
}
