//! QUIC companion APIs for spargio runtimes.
//!
//! This crate keeps a practical `quinn` integration path while moving toward a
//! native long-term design. It now provides:
//! - a persistent Tokio bridge executor (no per-call runtime creation),
//! - endpoint/connection wrappers with stream/datagram helpers,
//! - per-endpoint metrics snapshots and in-flight backpressure limits,
//! - explicit local (`!Send`) and send-handoff connection wrappers.

use spargio::{RuntimeError, RuntimeHandle};
use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use quinn;
pub use quinn_proto;

const DEFAULT_MAX_INFLIGHT_OPS: usize = 1024;
const BRIDGE_WORKER_THREADS: usize = 2;
static NEXT_NATIVE_ENDPOINT_ID: AtomicU64 = AtomicU64::new(1);

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

#[derive(Debug, Clone, Copy)]
pub struct NativeProtoDriverOptions {
    owner_shard: spargio::ShardId,
}

impl Default for NativeProtoDriverOptions {
    fn default() -> Self {
        Self { owner_shard: 0 }
    }
}

impl NativeProtoDriverOptions {
    pub fn with_owner_shard(mut self, owner_shard: spargio::ShardId) -> Self {
        self.owner_shard = owner_shard;
        self
    }

    pub fn owner_shard(self) -> spargio::ShardId {
        self.owner_shard
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NativeProtoDriverProbe {
    pub endpoint_id: u64,
    pub owner_shard: spargio::ShardId,
    pub executing_shard: spargio::ShardId,
    pub commands_processed: u64,
}

#[derive(Clone)]
pub struct NativeProtoDriver {
    endpoint_id: u64,
    owner_shard: spargio::ShardId,
    closed: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<NativeProtoCommand>,
}

impl NativeProtoDriver {
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

        let endpoint_id = NEXT_NATIVE_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_for_task = closed.clone();
        let owner_shard = options.owner_shard();

        handle
            .spawn_local_on(owner_shard, move |ctx| async move {
                native_proto_driver_loop(
                    endpoint_id,
                    owner_shard,
                    ctx.shard_id(),
                    rx,
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
    Shutdown {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    Closed,
}

async fn native_proto_driver_loop(
    endpoint_id: u64,
    owner_shard: spargio::ShardId,
    executing_shard: spargio::ShardId,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<NativeProtoCommand>,
    closed: Arc<AtomicBool>,
) {
    let _endpoint = quinn_proto::Endpoint::new(
        Arc::new(quinn_proto::EndpointConfig::default()),
        None,
        true,
        None,
    );
    let mut commands_processed = 0u64;
    let mut next_connection_id = 1u64;
    let mut next_stream_id = 1u64;

    while let Some(cmd) = rx.recv().await {
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
            NativeProtoCommand::Shutdown { reply } => {
                commands_processed = commands_processed.saturating_add(1);
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

#[derive(Debug, Clone, Copy)]
pub struct QuicEndpointOptions {
    connect_timeout: Option<Duration>,
    accept_timeout: Option<Duration>,
    operation_timeout: Option<Duration>,
    max_inflight_ops: usize,
}

impl Default for QuicEndpointOptions {
    fn default() -> Self {
        Self {
            connect_timeout: None,
            accept_timeout: None,
            operation_timeout: None,
            max_inflight_ops: DEFAULT_MAX_INFLIGHT_OPS,
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
        }
    }

    fn inc_endpoints_created(&self) {
        self.inner.endpoints_created.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connects_started(&self) {
        self.inner.connects_started.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connects_succeeded(&self) {
        self.inner.connects_succeeded.fetch_add(1, Ordering::Relaxed);
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
        self.inner.connections_opened.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_opened_uni(&self) {
        self.inner.streams_opened_uni.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_opened_bi(&self) {
        self.inner.streams_opened_bi.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_accepted_uni(&self) {
        self.inner.streams_accepted_uni.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_streams_accepted_bi(&self) {
        self.inner.streams_accepted_bi.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_datagrams_sent(&self) {
        self.inner.datagrams_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_datagrams_received(&self) {
        self.inner.datagrams_received.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_endpoint_closes(&self) {
        self.inner.endpoint_closes.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_connection_closes(&self) {
        self.inner.connection_closes.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_operation_timeouts(&self) {
        self.inner.operation_timeouts.fetch_add(1, Ordering::Relaxed);
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
}

#[derive(Clone)]
pub struct QuicEndpoint {
    endpoint: quinn::Endpoint,
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
        let endpoint = with_bridge_runtime_context(|| quinn::Endpoint::server(server_config, bind_addr))?;
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
        let endpoint = with_bridge_runtime_context(|| quinn::Endpoint::client(bind_addr))?;
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
        Ok(Self {
            endpoint,
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
        self.endpoint.local_addr()
    }

    pub fn set_default_client_config(&mut self, config: quinn::ClientConfig) {
        self.endpoint.set_default_client_config(config);
    }

    pub fn set_server_config(&mut self, config: Option<quinn::ServerConfig>) {
        self.endpoint.set_server_config(config);
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
        let endpoint = self.endpoint.clone();
        let server_name = server_name.to_owned();
        let connected = spawn_on_bridge_runtime(
            self.options.connect_timeout(),
            "quic connect timed out",
            move || async move {
                let connecting = endpoint
                    .connect(addr, &server_name)
                    .map_err(quinn_connect_error_to_io)?;
                connecting.await.map_err(quinn_connection_error_to_io)
            },
        )
        .await;
        match connected {
            Ok(connection) => {
                self.metrics.inc_connects_succeeded();
                Ok(self.wrap_connection(connection))
            }
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
        let endpoint = self.endpoint.clone();
        let server_name = server_name.to_owned();
        let connected = spawn_on_bridge_runtime(
            self.options.connect_timeout(),
            "quic connect timed out",
            move || async move {
                let connecting = endpoint
                    .connect_with(config, addr, &server_name)
                    .map_err(quinn_connect_error_to_io)?;
                connecting.await.map_err(quinn_connection_error_to_io)
            },
        )
        .await;
        match connected {
            Ok(connection) => {
                self.metrics.inc_connects_succeeded();
                Ok(self.wrap_connection(connection))
            }
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
        let endpoint = self.endpoint.clone();
        let op_timeout = self.options.operation_timeout();
        let accepted = spawn_on_bridge_runtime(
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
        .await;
        match accepted {
            Ok(Some(connection)) => {
                self.metrics.inc_accepts_succeeded();
                Ok(Some(self.wrap_connection(connection)))
            }
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
        self.endpoint.close(code.into(), reason);
        self.metrics.inc_endpoint_closes();
    }

    pub async fn wait_idle(&self) -> io::Result<()> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let endpoint = self.endpoint.clone();
        let waited = spawn_on_bridge_runtime(
            self.options.operation_timeout(),
            "quic endpoint wait_idle timed out",
            move || async move {
                endpoint.wait_idle().await;
                Ok(())
            },
        )
        .await;
        if let Err(ref err) = waited {
            if err.kind() == io::ErrorKind::TimedOut {
                self.metrics.inc_operation_timeouts();
            }
        }
        waited
    }

    fn wrap_connection(&self, connection: quinn::Connection) -> QuicConnection {
        self.metrics.inc_connections_opened();
        QuicConnection {
            connection,
            options: self.options,
            metrics: self.metrics.clone(),
            limiter: self.limiter.clone(),
        }
    }
}

#[derive(Clone)]
pub struct QuicConnection {
    connection: quinn::Connection,
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
        let closed = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.closed(),
            "quic connection closed() wait timed out",
        )
        .await;
        let err = match closed {
            Ok(err) => err,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        if matches!(err, quinn::ConnectionError::LocallyClosed) {
            return Ok(());
        }
        Err(quinn_connection_error_to_io(err))
    }

    pub async fn open_uni(&self) -> io::Result<quinn::SendStream> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let opened = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.open_uni(),
            "quic open_uni timed out",
        )
        .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        let stream = opened.map_err(quinn_connection_error_to_io)?;
        self.metrics.inc_streams_opened_uni();
        Ok(stream)
    }

    pub async fn open_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let opened = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.open_bi(),
            "quic open_bi timed out",
        )
        .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        let streams = opened.map_err(quinn_connection_error_to_io)?;
        self.metrics.inc_streams_opened_bi();
        Ok(streams)
    }

    pub async fn accept_uni(&self) -> io::Result<quinn::RecvStream> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let accepted = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.accept_uni(),
            "quic accept_uni timed out",
        )
        .await;
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        let stream = accepted.map_err(quinn_connection_error_to_io)?;
        self.metrics.inc_streams_accepted_uni();
        Ok(stream)
    }

    pub async fn accept_bi(&self) -> io::Result<(quinn::SendStream, quinn::RecvStream)> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let accepted = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.accept_bi(),
            "quic accept_bi timed out",
        )
        .await;
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        let streams = accepted.map_err(quinn_connection_error_to_io)?;
        self.metrics.inc_streams_accepted_bi();
        Ok(streams)
    }

    pub fn send_datagram<D>(&self, data: D) -> io::Result<()>
    where
        D: Into<Vec<u8>>,
    {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        self.connection
            .send_datagram(data.into().into())
            .map_err(quinn_send_datagram_error_to_io)?;
        self.metrics.inc_datagrams_sent();
        Ok(())
    }

    pub async fn read_datagram(&self) -> io::Result<Vec<u8>> {
        let _permit = acquire_with_metrics(&self.limiter, &self.metrics)?;
        let read = await_with_timeout(
            self.options.operation_timeout(),
            self.connection.read_datagram(),
            "quic read_datagram timed out",
        )
        .await;
        let read = match read {
            Ok(read) => read,
            Err(err) => {
                if err.kind() == io::ErrorKind::TimedOut {
                    self.metrics.inc_operation_timeouts();
                }
                return Err(err);
            }
        };
        let payload = read.map_err(quinn_connection_error_to_io)?;
        self.metrics.inc_datagrams_received();
        Ok(payload.to_vec())
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

fn with_bridge_runtime_context<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
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
        quinn::ConnectError::InvalidServerName(msg) => {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid server name: {msg}"))
        }
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
