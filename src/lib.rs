//! Sharded async runtime API centered around `msg_ring`-style signaling.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, unbounded};
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner};
use futures::future::{Either, select};
use futures::task::LocalSpawnExt;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};
#[cfg(target_os = "linux")]
use slab::Slab;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::fs::File;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::net::{TcpStream, UdpSocket};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub type ShardId = u16;
const EXTERNAL_SENDER: ShardId = ShardId::MAX;
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "linux")]
const MSG_RING_CQE_FLAG: u32 = 1 << 8;
#[cfg(target_os = "linux")]
const IOURING_SUBMIT_BATCH: usize = 64;
#[cfg(target_os = "linux")]
const DOORBELL_TAG: u16 = u16::MAX;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_OP_USER_BIT: u64 = 1 << 63;

pub mod boundary {
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
    use futures::channel::oneshot;
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BoundaryError {
        Closed,
        Overloaded,
        Timeout,
        Canceled,
    }

    struct BoundaryEnvelope<Request, Response> {
        request: Request,
        deadline: Option<Instant>,
        reply: oneshot::Sender<Result<Response, BoundaryError>>,
    }

    pub struct BoundaryRequest<Request, Response> {
        request: Request,
        deadline: Option<Instant>,
        reply: Option<oneshot::Sender<Result<Response, BoundaryError>>>,
    }

    impl<Request, Response> BoundaryRequest<Request, Response> {
        pub fn request(&self) -> &Request {
            &self.request
        }

        pub fn deadline(&self) -> Option<Instant> {
            self.deadline
        }

        pub fn into_request(self) -> Request {
            self.request
        }

        pub fn respond(mut self, response: Response) -> Result<(), BoundaryError> {
            if let Some(deadline) = self.deadline {
                if Instant::now() > deadline {
                    if let Some(reply) = self.reply.take() {
                        let _ = reply.send(Err(BoundaryError::Timeout));
                    }
                    return Err(BoundaryError::Timeout);
                }
            }

            let Some(reply) = self.reply.take() else {
                return Err(BoundaryError::Canceled);
            };

            reply
                .send(Ok(response))
                .map_err(|_| BoundaryError::Canceled)
        }
    }

    #[derive(Clone)]
    pub struct BoundaryClient<Request, Response> {
        tx: Sender<BoundaryEnvelope<Request, Response>>,
    }

    pub struct BoundaryServer<Request, Response> {
        rx: Receiver<BoundaryEnvelope<Request, Response>>,
    }

    pub struct BoundaryTicket<Response> {
        rx: Option<oneshot::Receiver<Result<Response, BoundaryError>>>,
    }

    impl<Response> BoundaryTicket<Response> {
        pub fn wait_timeout_blocking(
            mut self,
            timeout: Duration,
        ) -> Result<Response, BoundaryError> {
            let deadline = Instant::now() + timeout;
            loop {
                let Some(rx) = self.rx.as_mut() else {
                    return Err(BoundaryError::Canceled);
                };

                match rx.try_recv() {
                    Ok(Some(value)) => {
                        self.rx = None;
                        return value;
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            self.rx = None;
                            return Err(BoundaryError::Timeout);
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => {
                        self.rx = None;
                        return Err(BoundaryError::Canceled);
                    }
                }
            }
        }
    }

    impl<Response> Future for BoundaryTicket<Response> {
        type Output = Result<Response, BoundaryError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let Some(rx) = self.rx.as_mut() else {
                return Poll::Ready(Err(BoundaryError::Canceled));
            };

            match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(value)) => {
                    self.rx = None;
                    Poll::Ready(value)
                }
                Poll::Ready(Err(_)) => {
                    self.rx = None;
                    Poll::Ready(Err(BoundaryError::Canceled))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl<Request, Response> BoundaryClient<Request, Response> {
        pub fn call(&self, request: Request) -> Result<BoundaryTicket<Response>, BoundaryError> {
            self.enqueue(request, None, false)
        }

        pub fn call_with_timeout(
            &self,
            request: Request,
            timeout: Duration,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            self.enqueue(request, Some(Instant::now() + timeout), false)
        }

        pub fn try_call(
            &self,
            request: Request,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            self.enqueue(request, None, true)
        }

        fn enqueue(
            &self,
            request: Request,
            deadline: Option<Instant>,
            nonblocking: bool,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            let (reply_tx, reply_rx) = oneshot::channel();
            let msg = BoundaryEnvelope {
                request,
                deadline,
                reply: reply_tx,
            };

            if nonblocking {
                match self.tx.try_send(msg) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => return Err(BoundaryError::Overloaded),
                    Err(TrySendError::Disconnected(_)) => return Err(BoundaryError::Closed),
                }
            } else {
                self.tx.send(msg).map_err(|_| BoundaryError::Closed)?;
            }

            Ok(BoundaryTicket { rx: Some(reply_rx) })
        }
    }

    impl<Request, Response> BoundaryServer<Request, Response> {
        pub fn recv(&self) -> Result<BoundaryRequest<Request, Response>, BoundaryError> {
            self.rx
                .recv()
                .map(boundary_request)
                .map_err(|_| BoundaryError::Closed)
        }

        pub fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Result<BoundaryRequest<Request, Response>, BoundaryError> {
            self.rx
                .recv_timeout(timeout)
                .map(boundary_request)
                .map_err(|err| match err {
                    crossbeam_channel::RecvTimeoutError::Timeout => BoundaryError::Timeout,
                    crossbeam_channel::RecvTimeoutError::Disconnected => BoundaryError::Closed,
                })
        }
    }

    fn boundary_request<Request, Response>(
        msg: BoundaryEnvelope<Request, Response>,
    ) -> BoundaryRequest<Request, Response> {
        BoundaryRequest {
            request: msg.request,
            deadline: msg.deadline,
            reply: Some(msg.reply),
        }
    }

    pub fn channel<Request, Response>(
        capacity: usize,
    ) -> (
        BoundaryClient<Request, Response>,
        BoundaryServer<Request, Response>,
    ) {
        let (tx, rx) = bounded(capacity.max(1));
        (BoundaryClient { tx }, BoundaryServer { rx })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutError;

pub async fn sleep(duration: Duration) {
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

pub async fn timeout<F>(duration: Duration, fut: F) -> Result<F::Output, TimeoutError>
where
    F: Future,
{
    let mut fut = Box::pin(fut);
    let mut timer = Box::pin(sleep(duration));
    match select(fut.as_mut(), timer.as_mut()).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right((_, _)) => Err(TimeoutError),
    }
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    canceled: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if self.inner.canceled.swap(true, Ordering::SeqCst) {
            return;
        }
        let waiters = {
            let mut waiters = self.inner.waiters.lock().expect("cancel waiters poisoned");
            waiters.drain(..).collect::<Vec<_>>()
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.load(Ordering::SeqCst)
    }

    pub fn cancelled(&self) -> CancellationFuture {
        CancellationFuture {
            token: self.clone(),
        }
    }
}

pub struct CancellationFuture {
    token: CancellationToken,
}

impl Future for CancellationFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_canceled() {
            return Poll::Ready(());
        }

        let mut waiters = self
            .token
            .inner
            .waiters
            .lock()
            .expect("cancel waiters poisoned");
        if self.token.is_canceled() {
            return Poll::Ready(());
        }
        if !waiters.iter().any(|w| w.will_wake(cx.waker())) {
            waiters.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

pub struct TaskGroup {
    handle: RuntimeHandle,
    token: CancellationToken,
}

impl TaskGroup {
    pub fn new(handle: RuntimeHandle) -> Self {
        Self {
            handle,
            token: CancellationToken::new(),
        }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn spawn_with_placement<F, T>(
        &self,
        placement: TaskPlacement,
        fut: F,
    ) -> Result<TaskGroupJoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let token = self.token.clone();
        let join = self.handle.spawn_with_placement(placement, async move {
            let mut task = Box::pin(fut);
            let mut canceled = Box::pin(token.cancelled());
            match select(task.as_mut(), canceled.as_mut()).await {
                Either::Left((value, _)) => Some(value),
                Either::Right((_, _)) => None,
            }
        })?;
        Ok(TaskGroupJoinHandle { inner: join })
    }
}

pub struct TaskGroupJoinHandle<T> {
    inner: JoinHandle<Option<T>>,
}

impl<T> Future for TaskGroupJoinHandle<T> {
    type Output = Result<Option<T>, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    RingMsg { from: ShardId, tag: u16, val: u32 },
}

pub trait RingMsg: Copy + Send + 'static {
    fn encode(self) -> (u16, u32);
    fn decode(tag: u16, val: u32) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Queue,
    IoUring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPlacement {
    Pinned(ShardId),
    RoundRobin,
    Sticky(u64),
    Stealable,
    StealablePreferred(ShardId),
}

#[derive(Debug, Clone)]
pub struct RuntimeStats {
    pub shard_command_depths: Vec<usize>,
    pub spawn_pinned_submitted: u64,
    pub spawn_stealable_submitted: u64,
    pub stealable_executed: u64,
    pub stealable_stolen: u64,
    pub stealable_backpressure: u64,
    pub steal_attempts: u64,
    pub steal_success: u64,
    pub ring_msgs_submitted: u64,
    pub ring_msgs_completed: u64,
    pub ring_msgs_failed: u64,
    pub ring_msgs_backpressure: u64,
    pub native_affinity_violations: u64,
    pub pending_native_ops: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct IoUringBuildConfig {
    sqpoll_idle_ms: Option<u32>,
    sqpoll_cpu: Option<u32>,
    single_issuer: bool,
    coop_taskrun: bool,
}

#[cfg(target_os = "linux")]
impl Default for IoUringBuildConfig {
    fn default() -> Self {
        Self {
            sqpoll_idle_ms: None,
            sqpoll_cpu: None,
            single_issuer: false,
            coop_taskrun: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    shards: usize,
    thread_prefix: String,
    idle_wait: Duration,
    backend: BackendKind,
    ring_entries: u32,
    msg_ring_queue_capacity: usize,
    stealable_queue_capacity: usize,
    steal_budget: usize,
    #[cfg(target_os = "linux")]
    io_uring: IoUringBuildConfig,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            shards: std::thread::available_parallelism().map_or(1, usize::from),
            thread_prefix: "spargio-shard".to_owned(),
            idle_wait: Duration::from_millis(1),
            backend: BackendKind::Queue,
            ring_entries: 256,
            msg_ring_queue_capacity: 4096,
            stealable_queue_capacity: 4096,
            steal_budget: 64,
            #[cfg(target_os = "linux")]
            io_uring: IoUringBuildConfig::default(),
        }
    }
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shards(mut self, count: usize) -> Self {
        self.shards = count;
        self
    }

    pub fn thread_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.thread_prefix = prefix.into();
        self
    }

    pub fn idle_wait(mut self, wait: Duration) -> Self {
        self.idle_wait = wait;
        self
    }

    pub fn backend(mut self, backend: BackendKind) -> Self {
        self.backend = backend;
        self
    }

    pub fn ring_entries(mut self, entries: u32) -> Self {
        self.ring_entries = entries.max(1);
        self
    }

    pub fn msg_ring_queue_capacity(mut self, capacity: usize) -> Self {
        self.msg_ring_queue_capacity = capacity.max(1);
        self
    }

    pub fn stealable_queue_capacity(mut self, capacity: usize) -> Self {
        self.stealable_queue_capacity = capacity.max(1);
        self
    }

    pub fn steal_budget(mut self, budget: usize) -> Self {
        self.steal_budget = budget.max(1);
        self
    }

    #[cfg(target_os = "linux")]
    pub fn io_uring_sqpoll(mut self, idle_ms: Option<u32>) -> Self {
        self.io_uring.sqpoll_idle_ms = idle_ms;
        if idle_ms.is_none() {
            self.io_uring.sqpoll_cpu = None;
        }
        self
    }

    #[cfg(target_os = "linux")]
    pub fn io_uring_sqpoll_cpu(mut self, cpu: Option<u32>) -> Self {
        self.io_uring.sqpoll_cpu = cpu;
        if cpu.is_some() && self.io_uring.sqpoll_idle_ms.is_none() {
            self.io_uring.sqpoll_idle_ms = Some(2_000);
        }
        self
    }

    #[cfg(target_os = "linux")]
    pub fn io_uring_single_issuer(mut self, enable: bool) -> Self {
        self.io_uring.single_issuer = enable;
        self
    }

    #[cfg(target_os = "linux")]
    pub fn io_uring_coop_taskrun(mut self, enable: bool) -> Self {
        self.io_uring.coop_taskrun = enable;
        self
    }

    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.shards == 0 {
            return Err(RuntimeError::InvalidConfig("shards must be > 0"));
        }
        if self.shards > usize::from(ShardId::MAX) {
            return Err(RuntimeError::InvalidConfig(
                "shard count exceeds supported ShardId range",
            ));
        }

        let runtime_id = NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
        let mut senders = Vec::with_capacity(self.shards);
        let mut receivers = Vec::with_capacity(self.shards);
        for _ in 0..self.shards {
            let (tx, rx) = unbounded();
            senders.push(tx);
            receivers.push(rx);
        }
        let stealable_inboxes = build_stealable_inboxes(self.shards);
        let stats = Arc::new(RuntimeStatsInner::new(self.shards));

        let shared = Arc::new(RuntimeShared {
            runtime_id,
            backend: self.backend,
            command_txs: senders.clone(),
            stealable_inboxes: stealable_inboxes.clone(),
            stealable_queue_capacity: self.stealable_queue_capacity,
            stats: stats.clone(),
        });
        let remotes: Vec<RemoteShard> = (0..self.shards)
            .map(|i| RemoteShard {
                id: i as ShardId,
                shared: shared.clone(),
            })
            .collect();

        let mut joins = Vec::with_capacity(self.shards);
        match self.backend {
            BackendKind::Queue => {
                for (idx, rx) in receivers.into_iter().enumerate() {
                    let remotes_for_shard = remotes.clone();
                    let stealable_deques = stealable_inboxes.clone();
                    let thread_name = format!("{}-{}", self.thread_prefix, idx);
                    let idle_wait = self.idle_wait;
                    let runtime_id = shared.runtime_id;
                    let backend = ShardBackend::Queue;
                    let stats = stats.clone();

                    let join = match thread::Builder::new().name(thread_name).spawn(move || {
                        run_shard(
                            runtime_id,
                            idx as ShardId,
                            rx,
                            remotes_for_shard,
                            stealable_deques,
                            self.steal_budget,
                            idle_wait,
                            backend,
                            stats,
                        )
                    }) {
                        Ok(j) => j,
                        Err(err) => {
                            shutdown_spawned(&shared.command_txs, &shared.stats, &mut joins);
                            return Err(RuntimeError::ThreadSpawn(err));
                        }
                    };

                    joins.push(join);
                }
            }
            BackendKind::IoUring => {
                #[cfg(target_os = "linux")]
                {
                    let mut rings = Vec::with_capacity(self.shards);
                    let mut ring_fds = Vec::with_capacity(self.shards);
                    let payload_queues = build_payload_queues(self.shards);
                    for _ in 0..self.shards {
                        let mut ring_builder = IoUring::builder();
                        if let Some(idle_ms) = self.io_uring.sqpoll_idle_ms {
                            ring_builder.setup_sqpoll(idle_ms);
                            if let Some(cpu) = self.io_uring.sqpoll_cpu {
                                ring_builder.setup_sqpoll_cpu(cpu);
                            }
                        }
                        if self.io_uring.single_issuer {
                            ring_builder.setup_single_issuer();
                        }
                        if self.io_uring.coop_taskrun {
                            ring_builder.setup_coop_taskrun();
                        }

                        let ring = ring_builder
                            .build(self.ring_entries)
                            .map_err(RuntimeError::IoUringInit)?;
                        ring_fds.push(ring.as_raw_fd());
                        rings.push(ring);
                    }
                    let ring_fds = Arc::new(ring_fds);

                    for (idx, (rx, ring)) in
                        receivers.into_iter().zip(rings.into_iter()).enumerate()
                    {
                        let remotes_for_shard = remotes.clone();
                        let stealable_deques = stealable_inboxes.clone();
                        let thread_name = format!("{}-{}", self.thread_prefix, idx);
                        let idle_wait = self.idle_wait;
                        let runtime_id = shared.runtime_id;
                        let backend = ShardBackend::IoUring(IoUringDriver::new(
                            idx as ShardId,
                            ring,
                            ring_fds.clone(),
                            payload_queues.clone(),
                            stats.clone(),
                            self.msg_ring_queue_capacity,
                        ));
                        let stats = stats.clone();

                        let join = match thread::Builder::new().name(thread_name).spawn(move || {
                            run_shard(
                                runtime_id,
                                idx as ShardId,
                                rx,
                                remotes_for_shard,
                                stealable_deques,
                                self.steal_budget,
                                idle_wait,
                                backend,
                                stats,
                            )
                        }) {
                            Ok(j) => j,
                            Err(err) => {
                                shutdown_spawned(&shared.command_txs, &shared.stats, &mut joins);
                                return Err(RuntimeError::ThreadSpawn(err));
                            }
                        };

                        joins.push(join);
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(RuntimeError::UnsupportedBackend(
                        "io_uring backend requires Linux",
                    ));
                }
            }
        }

        Ok(Runtime {
            shared,
            remotes,
            joins,
            is_shutdown: false,
        })
    }
}

fn shutdown_spawned(
    command_txs: &[Sender<Command>],
    stats: &RuntimeStatsInner,
    joins: &mut Vec<thread::JoinHandle<()>>,
) {
    for (idx, tx) in command_txs.iter().enumerate() {
        stats.increment_command_depth(idx as ShardId);
        let _ = tx.send(Command::Shutdown);
    }
    for join in joins.drain(..) {
        let _ = join.join();
    }
}

pub struct Runtime {
    shared: Arc<RuntimeShared>,
    remotes: Vec<RemoteShard>,
    joins: Vec<thread::JoinHandle<()>>,
    is_shutdown: bool,
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    pub fn backend(&self) -> BackendKind {
        self.shared.backend
    }

    pub fn shard_count(&self) -> usize {
        self.remotes.len()
    }

    pub fn remote(&self, shard: ShardId) -> Option<RemoteShard> {
        self.remotes.get(usize::from(shard)).cloned()
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            inner: Arc::new(RuntimeHandleInner {
                shared: self.shared.clone(),
                remotes: self.remotes.clone(),
                next_shard: AtomicUsize::new(0),
            }),
        }
    }

    pub fn spawn_on<F, T>(&self, shard: ShardId, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        spawn_on_shared(&self.shared, shard, fut)
    }

    pub fn shutdown(&mut self) {
        if self.is_shutdown {
            return;
        }
        self.is_shutdown = true;

        for (idx, tx) in self.shared.command_txs.iter().enumerate() {
            self.shared.stats.increment_command_depth(idx as ShardId);
            let _ = tx.send(Command::Shutdown);
        }

        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<RuntimeHandleInner>,
}

struct RuntimeHandleInner {
    shared: Arc<RuntimeShared>,
    remotes: Vec<RemoteShard>,
    next_shard: AtomicUsize,
}

impl RuntimeHandle {
    pub fn backend(&self) -> BackendKind {
        self.inner.shared.backend
    }

    pub fn shard_count(&self) -> usize {
        self.inner.remotes.len()
    }

    pub fn remote(&self, shard: ShardId) -> Option<RemoteShard> {
        self.inner.remotes.get(usize::from(shard)).cloned()
    }

    pub fn spawn_pinned<F, T>(&self, shard: ShardId, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.inner
            .shared
            .stats
            .spawn_pinned_submitted
            .fetch_add(1, Ordering::Relaxed);
        spawn_on_shared(&self.inner.shared, shard, fut)
    }

    pub fn spawn_stealable<F, T>(&self, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_with_placement(TaskPlacement::Stealable, fut)
    }

    pub fn spawn_stealable_on<F, T>(
        &self,
        preferred_shard: ShardId,
        fut: F,
    ) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_with_placement(TaskPlacement::StealablePreferred(preferred_shard), fut)
    }

    pub fn spawn_with_placement<F, T>(
        &self,
        placement: TaskPlacement,
        fut: F,
    ) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let shards = self.shard_count();
        if shards == 0 {
            return Err(RuntimeError::Closed);
        }

        match placement {
            TaskPlacement::Pinned(shard) => self.spawn_pinned(shard, fut),
            TaskPlacement::RoundRobin => {
                let next = self.inner.next_shard.fetch_add(1, Ordering::Relaxed);
                let shard = (next % shards) as ShardId;
                self.spawn_pinned(shard, fut)
            }
            TaskPlacement::Sticky(key) => {
                let shard = sticky_key_to_shard(key, shards);
                self.spawn_pinned(shard, fut)
            }
            TaskPlacement::Stealable => {
                let next = self.inner.next_shard.fetch_add(1, Ordering::Relaxed);
                let preferred = (next % shards) as ShardId;
                spawn_stealable_on_shared(&self.inner.shared, preferred, fut)
            }
            TaskPlacement::StealablePreferred(preferred) => {
                if usize::from(preferred) >= shards {
                    return Err(RuntimeError::InvalidShard(preferred));
                }
                spawn_stealable_on_shared(&self.inner.shared, preferred, fut)
            }
        }
    }

    pub fn stats_snapshot(&self) -> RuntimeStats {
        self.inner.shared.stats.snapshot()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    pub fn uring_native_lane(&self, shard: ShardId) -> Result<UringNativeLane, RuntimeError> {
        if self.backend() != BackendKind::IoUring {
            return Err(RuntimeError::UnsupportedBackend(
                "uring-native requires io_uring backend",
            ));
        }
        if usize::from(shard) >= self.inner.remotes.len() {
            return Err(RuntimeError::InvalidShard(shard));
        }

        Ok(UringNativeLane {
            handle: self.clone(),
            shard,
        })
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
pub struct UringNativeLane {
    handle: RuntimeHandle,
    shard: ShardId,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl UringNativeLane {
    pub fn shard(&self) -> ShardId {
        self.shard
    }

    pub async fn read_at(&self, fd: RawFd, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let join = self
            .handle
            .spawn_pinned(self.shard, async move {
                let reply_rx = {
                    let ctx = ShardCtx::current().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "native lane task missing shard context",
                        )
                    })?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    ctx.inner.local_commands.borrow_mut().push_back(
                        LocalCommand::SubmitNativeRead {
                            origin_shard: ctx.shard_id(),
                            fd,
                            offset,
                            len,
                            reply: reply_tx,
                        },
                    );
                    reply_rx
                };

                reply_rx.await.unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native read response channel closed",
                    ))
                })
            })
            .map_err(runtime_error_to_io)?;

        join.await.map_err(join_error_to_io)?
    }

    pub async fn write_at(&self, fd: RawFd, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
        let payload = buf.to_vec();
        let join = self
            .handle
            .spawn_pinned(self.shard, async move {
                let reply_rx = {
                    let ctx = ShardCtx::current().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "native lane task missing shard context",
                        )
                    })?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    ctx.inner.local_commands.borrow_mut().push_back(
                        LocalCommand::SubmitNativeWrite {
                            origin_shard: ctx.shard_id(),
                            fd,
                            offset,
                            buf: payload,
                            reply: reply_tx,
                        },
                    );
                    reply_rx
                };

                reply_rx.await.unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native write response channel closed",
                    ))
                })
            })
            .map_err(runtime_error_to_io)?;

        join.await.map_err(join_error_to_io)?
    }

    pub async fn recv(&self, fd: RawFd, len: usize) -> std::io::Result<Vec<u8>> {
        let (read_len, mut buf) = self.recv_owned(fd, vec![0; len]).await?;
        buf.truncate(read_len.min(buf.len()));
        Ok(buf)
    }

    pub async fn recv_owned(&self, fd: RawFd, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        let mut pending_buf = Some(buf);
        if let Some(reply_rx) = {
            let maybe_ctx = ShardCtx::current().filter(|ctx| {
                ctx.runtime_id() == self.handle.inner.shared.runtime_id
                    && ctx.shard_id() == self.shard
            });
            maybe_ctx.map(|ctx| {
                let (reply_tx, reply_rx) = oneshot::channel();
                ctx.inner.local_commands.borrow_mut().push_back(
                    LocalCommand::SubmitNativeRecvOwned {
                        origin_shard: ctx.shard_id(),
                        fd,
                        buf: pending_buf
                            .take()
                            .expect("native recv pending buffer must exist"),
                        reply: reply_tx,
                    },
                );
                reply_rx
            })
        } {
            return reply_rx.await.unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native recv response channel closed",
                ))
            });
        }

        let buf = pending_buf
            .take()
            .expect("native recv pending buffer must exist");
        let join = self
            .handle
            .spawn_pinned(self.shard, async move {
                let reply_rx = {
                    let ctx = ShardCtx::current().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "native lane task missing shard context",
                        )
                    })?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    ctx.inner.local_commands.borrow_mut().push_back(
                        LocalCommand::SubmitNativeRecvOwned {
                            origin_shard: ctx.shard_id(),
                            fd,
                            buf,
                            reply: reply_tx,
                        },
                    );
                    reply_rx
                };

                reply_rx.await.unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native recv response channel closed",
                    ))
                })
            })
            .map_err(runtime_error_to_io)?;

        join.await.map_err(join_error_to_io)?
    }

    pub async fn send(&self, fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
        let (sent, _) = self.send_owned(fd, buf.to_vec()).await?;
        Ok(sent)
    }

    pub async fn send_owned(&self, fd: RawFd, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        let mut pending_buf = Some(buf);
        if let Some(reply_rx) = {
            let maybe_ctx = ShardCtx::current().filter(|ctx| {
                ctx.runtime_id() == self.handle.inner.shared.runtime_id
                    && ctx.shard_id() == self.shard
            });
            maybe_ctx.map(|ctx| {
                let (reply_tx, reply_rx) = oneshot::channel();
                ctx.inner.local_commands.borrow_mut().push_back(
                    LocalCommand::SubmitNativeSendOwned {
                        origin_shard: ctx.shard_id(),
                        fd,
                        buf: pending_buf
                            .take()
                            .expect("native send pending buffer must exist"),
                        reply: reply_tx,
                    },
                );
                reply_rx
            })
        } {
            return reply_rx.await.unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native send response channel closed",
                ))
            });
        }

        let buf = pending_buf
            .take()
            .expect("native send pending buffer must exist");
        let join = self
            .handle
            .spawn_pinned(self.shard, async move {
                let reply_rx = {
                    let ctx = ShardCtx::current().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "native lane task missing shard context",
                        )
                    })?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    ctx.inner.local_commands.borrow_mut().push_back(
                        LocalCommand::SubmitNativeSendOwned {
                            origin_shard: ctx.shard_id(),
                            fd,
                            buf,
                            reply: reply_tx,
                        },
                    );
                    reply_rx
                };

                reply_rx.await.unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native send response channel closed",
                    ))
                })
            })
            .map_err(runtime_error_to_io)?;

        join.await.map_err(join_error_to_io)?
    }

    pub async fn fsync(&self, fd: RawFd) -> std::io::Result<()> {
        let join = self
            .handle
            .spawn_pinned(self.shard, async move {
                let reply_rx = {
                    let ctx = ShardCtx::current().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "native lane task missing shard context",
                        )
                    })?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    ctx.inner.local_commands.borrow_mut().push_back(
                        LocalCommand::SubmitNativeFsync {
                            origin_shard: ctx.shard_id(),
                            fd,
                            reply: reply_tx,
                        },
                    );
                    reply_rx
                };

                reply_rx.await.unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native fsync response channel closed",
                    ))
                })
            })
            .map_err(runtime_error_to_io)?;

        join.await.map_err(join_error_to_io)?
    }

    pub fn bind_owned_fd(&self, fd: OwnedFd) -> UringBoundFd {
        UringBoundFd {
            lane: self.clone(),
            fd: Arc::new(fd),
        }
    }

    pub fn bind_file(&self, file: File) -> UringBoundFd {
        self.bind_owned_fd(file.into())
    }

    pub fn bind_tcp_stream(&self, stream: TcpStream) -> UringBoundFd {
        self.bind_owned_fd(stream.into())
    }

    pub fn bind_udp_socket(&self, socket: UdpSocket) -> UringBoundFd {
        self.bind_owned_fd(socket.into())
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
pub struct UringBoundFd {
    lane: UringNativeLane,
    fd: Arc<OwnedFd>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl UringBoundFd {
    pub fn shard(&self) -> ShardId {
        self.lane.shard()
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub async fn read_at(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.lane.read_at(self.raw_fd(), offset, len).await
    }

    pub async fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
        self.lane.write_at(self.raw_fd(), offset, buf).await
    }

    pub async fn recv(&self, len: usize) -> std::io::Result<Vec<u8>> {
        self.lane.recv(self.raw_fd(), len).await
    }

    pub async fn recv_owned(&self, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        self.lane.recv_owned(self.raw_fd(), buf).await
    }

    pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.lane.send(self.raw_fd(), buf).await
    }

    pub async fn send_owned(&self, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        self.lane.send_owned(self.raw_fd(), buf).await
    }

    pub async fn fsync(&self) -> std::io::Result<()> {
        self.lane.fsync(self.raw_fd()).await
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn runtime_error_to_io(err: RuntimeError) -> std::io::Error {
    match err {
        RuntimeError::InvalidConfig(msg) => {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
        }
        RuntimeError::ThreadSpawn(io) => io,
        RuntimeError::InvalidShard(shard) => std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("invalid shard {shard}"),
        ),
        RuntimeError::Closed => {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "runtime closed")
        }
        RuntimeError::Overloaded => {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "runtime overloaded")
        }
        RuntimeError::UnsupportedBackend(msg) => {
            std::io::Error::new(std::io::ErrorKind::Unsupported, msg)
        }
        #[cfg(target_os = "linux")]
        RuntimeError::IoUringInit(io) => io,
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn join_error_to_io(_err: JoinError) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "native lane task canceled before completion",
    )
}

fn spawn_on_shared<F, T>(
    shared: &Arc<RuntimeShared>,
    shard: ShardId,
    fut: F,
) -> Result<JoinHandle<T>, RuntimeError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    shared
        .send_to(
            shard,
            Command::Spawn(Box::pin(async move {
                let out = fut.await;
                let _ = tx.send(out);
            })),
        )
        .map_err(|_| RuntimeError::InvalidShard(shard))?;

    Ok(JoinHandle { rx: Some(rx) })
}

fn spawn_stealable_on_shared<F, T>(
    shared: &Arc<RuntimeShared>,
    preferred_shard: ShardId,
    fut: F,
) -> Result<JoinHandle<T>, RuntimeError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    if usize::from(preferred_shard) >= shared.command_txs.len() {
        return Err(RuntimeError::InvalidShard(preferred_shard));
    }
    let target = preferred_shard;

    let (tx, rx) = oneshot::channel();
    shared
        .stats
        .spawn_stealable_submitted
        .fetch_add(1, Ordering::Relaxed);
    let Some(inbox) = shared.stealable_inboxes.get(usize::from(target)) else {
        return Err(RuntimeError::InvalidShard(target));
    };
    let mut queue = inbox.lock().expect("stealable queue lock poisoned");
    if queue.len() >= shared.stealable_queue_capacity {
        shared
            .stats
            .stealable_backpressure
            .fetch_add(1, Ordering::Relaxed);
        return Err(RuntimeError::Overloaded);
    }
    queue.push_back(StealableTask {
        preferred_shard,
        task: Box::pin(async move {
            let out = fut.await;
            let _ = tx.send(out);
        }),
    });
    drop(queue);
    shared.notify_stealable_target(target);

    Ok(JoinHandle { rx: Some(rx) })
}

fn sticky_key_to_shard(key: u64, shards: usize) -> ShardId {
    let mixed = key
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (mixed as usize % shards) as ShardId
}

struct StealableTask {
    preferred_shard: ShardId,
    task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
}

type StealableInboxes = Arc<Vec<Arc<Mutex<VecDeque<StealableTask>>>>>;

struct RuntimeStatsInner {
    shard_command_depths: Vec<AtomicUsize>,
    spawn_pinned_submitted: AtomicU64,
    spawn_stealable_submitted: AtomicU64,
    stealable_executed: AtomicU64,
    stealable_stolen: AtomicU64,
    stealable_backpressure: AtomicU64,
    steal_attempts: AtomicU64,
    steal_success: AtomicU64,
    ring_msgs_submitted: AtomicU64,
    ring_msgs_completed: AtomicU64,
    ring_msgs_failed: AtomicU64,
    ring_msgs_backpressure: AtomicU64,
    native_affinity_violations: AtomicU64,
    pending_native_ops: AtomicU64,
}

impl RuntimeStatsInner {
    fn new(shards: usize) -> Self {
        let mut shard_command_depths = Vec::with_capacity(shards);
        for _ in 0..shards {
            shard_command_depths.push(AtomicUsize::new(0));
        }

        Self {
            shard_command_depths,
            spawn_pinned_submitted: AtomicU64::new(0),
            spawn_stealable_submitted: AtomicU64::new(0),
            stealable_executed: AtomicU64::new(0),
            stealable_stolen: AtomicU64::new(0),
            stealable_backpressure: AtomicU64::new(0),
            steal_attempts: AtomicU64::new(0),
            steal_success: AtomicU64::new(0),
            ring_msgs_submitted: AtomicU64::new(0),
            ring_msgs_completed: AtomicU64::new(0),
            ring_msgs_failed: AtomicU64::new(0),
            ring_msgs_backpressure: AtomicU64::new(0),
            native_affinity_violations: AtomicU64::new(0),
            pending_native_ops: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> RuntimeStats {
        RuntimeStats {
            shard_command_depths: self
                .shard_command_depths
                .iter()
                .map(|depth| depth.load(Ordering::Relaxed))
                .collect(),
            spawn_pinned_submitted: self.spawn_pinned_submitted.load(Ordering::Relaxed),
            spawn_stealable_submitted: self.spawn_stealable_submitted.load(Ordering::Relaxed),
            stealable_executed: self.stealable_executed.load(Ordering::Relaxed),
            stealable_stolen: self.stealable_stolen.load(Ordering::Relaxed),
            stealable_backpressure: self.stealable_backpressure.load(Ordering::Relaxed),
            steal_attempts: self.steal_attempts.load(Ordering::Relaxed),
            steal_success: self.steal_success.load(Ordering::Relaxed),
            ring_msgs_submitted: self.ring_msgs_submitted.load(Ordering::Relaxed),
            ring_msgs_completed: self.ring_msgs_completed.load(Ordering::Relaxed),
            ring_msgs_failed: self.ring_msgs_failed.load(Ordering::Relaxed),
            ring_msgs_backpressure: self.ring_msgs_backpressure.load(Ordering::Relaxed),
            native_affinity_violations: self.native_affinity_violations.load(Ordering::Relaxed),
            pending_native_ops: self.pending_native_ops.load(Ordering::Relaxed),
        }
    }

    fn increment_command_depth(&self, shard: ShardId) {
        if let Some(depth) = self.shard_command_depths.get(usize::from(shard)) {
            depth.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn decrement_command_depth(&self, shard: ShardId) {
        if let Some(depth) = self.shard_command_depths.get(usize::from(shard)) {
            let _ = depth.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(1))
            });
        }
    }
}

#[derive(Clone)]
struct RuntimeShared {
    runtime_id: u64,
    backend: BackendKind,
    command_txs: Vec<Sender<Command>>,
    stealable_inboxes: StealableInboxes,
    stealable_queue_capacity: usize,
    stats: Arc<RuntimeStatsInner>,
}

impl RuntimeShared {
    fn send_to(&self, shard: ShardId, cmd: Command) -> Result<(), ()> {
        let Some(tx) = self.command_txs.get(usize::from(shard)) else {
            return Err(());
        };
        self.stats.increment_command_depth(shard);
        if tx.send(cmd).is_ok() {
            return Ok(());
        }
        self.stats.decrement_command_depth(shard);
        Err(())
    }

    fn notify_stealable_target(&self, target: ShardId) {
        if let Some(ctx) = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.runtime_id) {
            if ctx.shard_id() == target {
                return;
            }
            let _ = ctx.enqueue_local_stealable_wake(target);
            return;
        }

        let _ = self.send_to(target, Command::StealableWake);
    }
}

#[derive(Clone)]
pub struct RemoteShard {
    id: ShardId,
    shared: Arc<RuntimeShared>,
}

impl RemoteShard {
    pub fn id(&self) -> ShardId {
        self.id
    }

    pub fn send_raw(&self, tag: u16, val: u32) -> Result<SendTicket, SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.send_raw_inner(tag, val, Some(ack_tx))?;
        Ok(SendTicket { rx: Some(ack_rx) })
    }

    pub fn send_raw_nowait(&self, tag: u16, val: u32) -> Result<(), SendError> {
        self.send_raw_inner(tag, val, None)
    }

    pub fn send_many_raw_nowait<I>(&self, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        let current = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.shared.runtime_id);
        if let Some(ctx) = current {
            return ctx.enqueue_local_send_many(self.id, msgs);
        }

        for (tag, val) in msgs {
            self.shared
                .send_to(
                    self.id,
                    Command::InjectRawMessage {
                        from: EXTERNAL_SENDER,
                        tag,
                        val,
                        ack: None,
                    },
                )
                .map_err(|_| SendError::Closed)?;
        }
        Ok(())
    }

    fn send_raw_inner(
        &self,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    ) -> Result<(), SendError> {
        let current = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.shared.runtime_id);
        if let Some(ctx) = current {
            return ctx.enqueue_local_send(self.id, tag, val, ack);
        }

        self.shared
            .send_to(
                self.id,
                Command::InjectRawMessage {
                    from: EXTERNAL_SENDER,
                    tag,
                    val,
                    ack,
                },
            )
            .map_err(|_| SendError::Closed)
    }

    pub fn send<M: RingMsg>(&self, msg: M) -> Result<SendTicket, SendError> {
        let (tag, val) = msg.encode();
        self.send_raw(tag, val)
    }

    pub fn send_nowait<M: RingMsg>(&self, msg: M) -> Result<(), SendError> {
        let (tag, val) = msg.encode();
        self.send_raw_nowait(tag, val)
    }

    pub fn send_many_nowait<M, I>(&self, msgs: I) -> Result<(), SendError>
    where
        M: RingMsg,
        I: IntoIterator<Item = M>,
    {
        self.send_many_raw_nowait(msgs.into_iter().map(|msg| msg.encode()))
    }

    pub fn flush(&self) -> Result<SendTicket, SendError> {
        let current = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.shared.runtime_id);
        if let Some(ctx) = current {
            return ctx.flush();
        }

        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(()));
        Ok(SendTicket { rx: Some(rx) })
    }
}

#[derive(Clone)]
pub struct ShardCtx {
    inner: Rc<ShardCtxInner>,
}

struct ShardCtxInner {
    runtime_id: u64,
    shard_id: ShardId,
    event_state: Arc<EventState>,
    spawner: LocalSpawner,
    remotes: Vec<RemoteShard>,
    local_commands: Rc<RefCell<VecDeque<LocalCommand>>>,
}

thread_local! {
    static CURRENT_SHARD: RefCell<Option<ShardCtx>> = const { RefCell::new(None) };
}

impl ShardCtx {
    pub fn current() -> Option<Self> {
        CURRENT_SHARD.with(|ctx| ctx.borrow().clone())
    }

    pub fn shard_id(&self) -> ShardId {
        self.inner.shard_id
    }

    fn runtime_id(&self) -> u64 {
        self.inner.runtime_id
    }

    pub fn remote(&self, target: ShardId) -> Option<RemoteShard> {
        self.inner.remotes.get(usize::from(target)).cloned()
    }

    pub fn send_raw_nowait(&self, target: ShardId, tag: u16, val: u32) -> Result<(), SendError> {
        self.enqueue_local_send(target, tag, val, None)
    }

    pub fn send_many_raw_nowait<I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        self.enqueue_local_send_many(target, msgs)
    }

    pub fn send_many_nowait<M, I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        M: RingMsg,
        I: IntoIterator<Item = M>,
    {
        self.enqueue_local_send_many(target, msgs.into_iter().map(|msg| msg.encode()))
    }

    pub fn send_raw(&self, target: ShardId, tag: u16, val: u32) -> Result<SendTicket, SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.enqueue_local_send(target, tag, val, Some(ack_tx))?;
        Ok(SendTicket { rx: Some(ack_rx) })
    }

    fn enqueue_local_send(
        &self,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    ) -> Result<(), SendError> {
        if usize::from(target) >= self.inner.remotes.len() {
            if let Some(ack) = ack {
                let _ = ack.send(Err(SendError::Closed));
            }
            return Err(SendError::Closed);
        }

        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitRingMsg {
                target,
                tag,
                val,
                ack,
            });
        Ok(())
    }

    fn enqueue_local_send_many<I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        if usize::from(target) >= self.inner.remotes.len() {
            return Err(SendError::Closed);
        }

        let messages = msgs.into_iter().collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }

        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitRingMsgBatch { target, messages });
        Ok(())
    }

    fn enqueue_local_stealable_wake(&self, target: ShardId) -> Result<(), SendError> {
        if usize::from(target) >= self.inner.remotes.len() {
            return Err(SendError::Closed);
        }

        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitStealableWake { target });
        Ok(())
    }

    pub fn flush(&self) -> Result<SendTicket, SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::Flush { ack: ack_tx });
        Ok(SendTicket { rx: Some(ack_rx) })
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    pub async fn native_read_at(
        &self,
        fd: RawFd,
        offset: u64,
        len: usize,
    ) -> std::io::Result<Vec<u8>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitNativeRead {
                origin_shard: self.inner.shard_id,
                fd,
                offset,
                len,
                reply: reply_tx,
            });

        reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native read response channel closed",
            ))
        })
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    pub async fn native_write_at(
        &self,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
    ) -> std::io::Result<usize> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitNativeWrite {
                origin_shard: self.inner.shard_id,
                fd,
                offset,
                buf,
                reply: reply_tx,
            });

        reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native write response channel closed",
            ))
        })
    }

    pub fn spawn_local<F, T>(&self, fut: F) -> LocalJoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (tx, rx) = oneshot::channel();
        if self
            .inner
            .spawner
            .spawn_local(async move {
                let out = fut.await;
                let _ = tx.send(out);
            })
            .is_err()
        {
            return LocalJoinHandle { rx: None };
        }

        LocalJoinHandle { rx: Some(rx) }
    }

    pub fn next_event(&self) -> NextEvent {
        NextEvent {
            state: self.inner.event_state.clone(),
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    InvalidConfig(&'static str),
    ThreadSpawn(std::io::Error),
    InvalidShard(ShardId),
    Closed,
    Overloaded,
    UnsupportedBackend(&'static str),
    #[cfg(target_os = "linux")]
    IoUringInit(std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    Closed,
    Backpressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    Canceled,
}

pub struct JoinHandle<T> {
    rx: Option<oneshot::Receiver<T>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(Err(JoinError::Canceled));
        };

        match Pin::new(rx).poll(cx) {
            Poll::Ready(Ok(v)) => {
                self.rx = None;
                Poll::Ready(Ok(v))
            }
            Poll::Ready(Err(_)) => {
                self.rx = None;
                Poll::Ready(Err(JoinError::Canceled))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct LocalJoinHandle<T> {
    rx: Option<oneshot::Receiver<T>>,
}

impl<T> Future for LocalJoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(Err(JoinError::Canceled));
        };

        match Pin::new(rx).poll(cx) {
            Poll::Ready(Ok(v)) => {
                self.rx = None;
                Poll::Ready(Ok(v))
            }
            Poll::Ready(Err(_)) => {
                self.rx = None;
                Poll::Ready(Err(JoinError::Canceled))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct SendTicket {
    rx: Option<oneshot::Receiver<Result<(), SendError>>>,
}

impl Future for SendTicket {
    type Output = Result<(), SendError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(Err(SendError::Closed));
        };

        match Pin::new(rx).poll(cx) {
            Poll::Ready(Ok(v)) => {
                self.rx = None;
                Poll::Ready(v)
            }
            Poll::Ready(Err(_)) => {
                self.rx = None;
                Poll::Ready(Err(SendError::Closed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct NextEvent {
    state: Arc<EventState>,
}

impl Future for NextEvent {
    type Output = Event;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.poll_next(cx)
    }
}

enum LocalCommand {
    SubmitRingMsg {
        target: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    },
    SubmitRingMsgBatch {
        target: ShardId,
        messages: Vec<(u16, u32)>,
    },
    SubmitStealableWake {
        target: ShardId,
    },
    Flush {
        ack: oneshot::Sender<Result<(), SendError>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeRead {
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
        len: usize,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeWrite {
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<usize>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeRecvOwned {
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeSendOwned {
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeFsync {
        origin_shard: ShardId,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
    },
}

enum Command {
    Spawn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    InjectRawMessage {
        from: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    },
    StealableWake,
    Shutdown,
}

#[derive(Default)]
struct EventState {
    inner: Mutex<EventQueue>,
}

#[derive(Default)]
struct EventQueue {
    queue: VecDeque<Event>,
    waiters: Vec<Waker>,
}

impl EventState {
    fn push(&self, event: Event) {
        let waiters = {
            let mut inner = self.inner.lock().expect("event lock poisoned");
            inner.queue.push_back(event);
            inner.waiters.drain(..).collect::<Vec<_>>()
        };

        for w in waiters {
            w.wake();
        }
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Event> {
        let mut inner = self.inner.lock().expect("event lock poisoned");
        if let Some(event) = inner.queue.pop_front() {
            return Poll::Ready(event);
        }

        if !inner.waiters.iter().any(|w| w.will_wake(cx.waker())) {
            inner.waiters.push(cx.waker().clone());
        }

        Poll::Pending
    }
}

enum ShardBackend {
    Queue,
    #[cfg(target_os = "linux")]
    IoUring(IoUringDriver),
}

impl ShardBackend {
    fn prefers_busy_poll(&self) -> bool {
        #[cfg(target_os = "linux")]
        if matches!(self, Self::IoUring(_)) {
            return true;
        }
        false
    }

    fn poll(&mut self, event_state: &EventState) {
        #[cfg(target_os = "linux")]
        if let Self::IoUring(driver) = self {
            driver.reap(event_state);
        }
    }

    fn submit_ring_msg(
        &mut self,
        from: ShardId,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
        command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        stats.ring_msgs_submitted.fetch_add(1, Ordering::Relaxed);
        match self {
            Self::Queue => {
                let Some(tx) = command_txs.get(usize::from(target)) else {
                    stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
                    if let Some(ack) = ack {
                        let _ = ack.send(Err(SendError::Closed));
                    }
                    return;
                };
                stats.increment_command_depth(target);
                if tx
                    .send(Command::InjectRawMessage {
                        from,
                        tag,
                        val,
                        ack,
                    })
                    .is_err()
                {
                    stats.decrement_command_depth(target);
                    stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            #[cfg(target_os = "linux")]
            Self::IoUring(driver) => {
                if driver.submit_ring_msg(target, tag, val, ack).is_err() {
                    stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
                    // submit_ring_msg already completed ack with an error
                }
            }
        }
    }

    fn submit_stealable_wake(
        &mut self,
        target: ShardId,
        command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        match self {
            Self::Queue => {
                let Some(tx) = command_txs.get(usize::from(target)) else {
                    return;
                };
                stats.increment_command_depth(target);
                if tx.send(Command::StealableWake).is_err() {
                    stats.decrement_command_depth(target);
                }
            }
            #[cfg(target_os = "linux")]
            Self::IoUring(driver) => {
                stats.ring_msgs_submitted.fetch_add(1, Ordering::Relaxed);
                if driver.submit_stealable_wake(target).is_err() {
                    stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn flush(&mut self, ack: oneshot::Sender<Result<(), SendError>>) {
        match self {
            Self::Queue => {
                let _ = ack.send(Ok(()));
            }
            #[cfg(target_os = "linux")]
            Self::IoUring(driver) => {
                if driver.submit_flush(ack).is_err() {
                    // submit_flush already completed ack with an error
                }
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_read(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
        len: usize,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring read violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring read requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_read(fd, offset, len, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_write(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<usize>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring write violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring write requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_write(fd, offset, buf, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_recv(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring recv violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring recv requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_recv(fd, buf, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_send(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring send violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring send requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_send(fd, buf, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_fsync(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring fsync violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring fsync requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_fsync(fd, reply);
            }
        }
    }

    fn shutdown(&mut self) {
        #[cfg(target_os = "linux")]
        if let Self::IoUring(driver) = self {
            driver.shutdown();
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct DoorbellPayload {
    tag: u16,
    val: u32,
}

#[cfg(target_os = "linux")]
type PayloadQueues = Arc<Vec<Vec<Mutex<VecDeque<DoorbellPayload>>>>>;

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum NativeIoOp {
    Read {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    },
    Write {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<usize>>,
    },
    Recv {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    Send {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    Fsync {
        reply: oneshot::Sender<std::io::Result<()>>,
    },
}

#[cfg(target_os = "linux")]
struct IoUringDriver {
    shard_id: ShardId,
    ring: IoUring,
    ring_fds: Arc<Vec<RawFd>>,
    payload_queues: PayloadQueues,
    send_waiters: Slab<oneshot::Sender<Result<(), SendError>>>,
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    native_ops: Slab<NativeIoOp>,
    pending_submit: usize,
    submit_batch_limit: usize,
    payload_queue_capacity: usize,
    stats: Arc<RuntimeStatsInner>,
}

#[cfg(target_os = "linux")]
impl IoUringDriver {
    fn new(
        shard_id: ShardId,
        ring: IoUring,
        ring_fds: Arc<Vec<RawFd>>,
        payload_queues: PayloadQueues,
        stats: Arc<RuntimeStatsInner>,
        payload_queue_capacity: usize,
    ) -> Self {
        Self {
            shard_id,
            ring,
            ring_fds,
            payload_queues,
            send_waiters: Slab::new(),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            native_ops: Slab::new(),
            pending_submit: 0,
            submit_batch_limit: IOURING_SUBMIT_BATCH,
            payload_queue_capacity: payload_queue_capacity.max(1),
            stats,
        }
    }

    fn submit_ring_msg(
        &mut self,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    ) -> Result<(), SendError> {
        if let Some(ack) = ack {
            return self.submit_ring_msg_ticketed(target, tag, val, ack);
        }
        self.submit_ring_msg_nowait(target, tag, val)
    }

    fn submit_ring_msg_ticketed(
        &mut self,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: oneshot::Sender<Result<(), SendError>>,
    ) -> Result<(), SendError> {
        let target_fd = self.target_fd(target)?;
        let waiter_idx = self.send_waiters.insert(ack);
        let payload = pack_msg_userdata(self.shard_id, tag);
        let val_i32 = i32::from_ne_bytes(val.to_ne_bytes());
        let user_data = waiter_to_userdata(waiter_idx);

        let entry = opcode::MsgRingData::new(
            types::Fd(target_fd),
            val_i32,
            payload,
            Some(MSG_RING_CQE_FLAG),
        )
        .build()
        .user_data(user_data);

        if self.push_entry(entry).is_err() {
            self.fail_waiter(waiter_idx);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    fn submit_ring_msg_nowait(
        &mut self,
        target: ShardId,
        tag: u16,
        val: u32,
    ) -> Result<(), SendError> {
        let should_ring = self.enqueue_payload(target, tag, val)?;
        if !should_ring {
            return Ok(());
        }

        if self.submit_doorbell(target).is_err() {
            self.rollback_last_payload(target);
            return Err(SendError::Closed);
        }
        Ok(())
    }

    fn enqueue_payload(&self, target: ShardId, tag: u16, val: u32) -> Result<bool, SendError> {
        let Some(per_source) = self.payload_queues.get(usize::from(target)) else {
            return Err(SendError::Closed);
        };
        let Some(queue) = per_source.get(usize::from(self.shard_id)) else {
            return Err(SendError::Closed);
        };

        let mut queue = queue.lock().expect("payload queue lock poisoned");
        if queue.len() >= self.payload_queue_capacity {
            self.stats
                .ring_msgs_backpressure
                .fetch_add(1, Ordering::Relaxed);
            return Err(SendError::Backpressure);
        }
        let was_empty = queue.is_empty();
        queue.push_back(DoorbellPayload { tag, val });
        Ok(was_empty)
    }

    fn rollback_last_payload(&self, target: ShardId) {
        let Some(per_source) = self.payload_queues.get(usize::from(target)) else {
            return;
        };
        let Some(queue) = per_source.get(usize::from(self.shard_id)) else {
            return;
        };

        let mut queue = queue.lock().expect("payload queue lock poisoned");
        let _ = queue.pop_back();
    }

    fn submit_doorbell(&mut self, target: ShardId) -> Result<(), SendError> {
        let target_fd = self.target_fd(target)?;
        let payload = pack_msg_userdata(self.shard_id, DOORBELL_TAG);
        let entry =
            opcode::MsgRingData::new(types::Fd(target_fd), 0, payload, Some(MSG_RING_CQE_FLAG))
                .build()
                .user_data(0)
                .flags(io_uring::squeue::Flags::SKIP_SUCCESS);
        self.push_entry(entry)?;
        self.mark_submission_pending()
    }

    fn submit_stealable_wake(&mut self, target: ShardId) -> Result<(), SendError> {
        self.submit_doorbell(target)
    }

    fn submit_flush(
        &mut self,
        ack: oneshot::Sender<Result<(), SendError>>,
    ) -> Result<(), SendError> {
        self.flush_submissions()?;

        let waiter_idx = self.send_waiters.insert(ack);
        let entry = opcode::Nop::new()
            .build()
            .user_data(waiter_to_userdata(waiter_idx));

        if self.push_entry(entry).is_err() {
            self.fail_waiter(waiter_idx);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_read(
        &mut self,
        fd: RawFd,
        offset: u64,
        len: usize,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    ) -> Result<(), SendError> {
        let Ok(len_u32) = u32::try_from(len) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native read length exceeds u32::MAX",
            )));
            return Ok(());
        };

        let native_index = self.native_ops.insert(NativeIoOp::Read {
            buf: vec![0; len],
            reply,
        });
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Read { buf, .. }) => buf.as_mut_ptr(),
            _ => unreachable!("native read op kind mismatch"),
        };

        let entry = opcode::Read::new(types::Fd(fd), buf_ptr, len_u32)
            .offset(offset)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_write(
        &mut self,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<usize>>,
    ) -> Result<(), SendError> {
        let Ok(len_u32) = u32::try_from(buf.len()) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native write length exceeds u32::MAX",
            )));
            return Ok(());
        };

        let native_index = self.native_ops.insert(NativeIoOp::Write { buf, reply });
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Write { buf, .. }) => buf.as_ptr(),
            _ => unreachable!("native write op kind mismatch"),
        };

        let entry = opcode::Write::new(types::Fd(fd), buf_ptr, len_u32)
            .offset(offset)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_recv(
        &mut self,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    ) -> Result<(), SendError> {
        let Ok(len_u32) = u32::try_from(buf.len()) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native recv length exceeds u32::MAX",
            )));
            return Ok(());
        };

        let native_index = self.native_ops.insert(NativeIoOp::Recv { buf, reply });
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Recv { buf, .. }) => buf.as_mut_ptr(),
            _ => unreachable!("native recv op kind mismatch"),
        };

        let entry = opcode::Recv::new(types::Fd(fd), buf_ptr, len_u32)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_send(
        &mut self,
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    ) -> Result<(), SendError> {
        let Ok(len_u32) = u32::try_from(buf.len()) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native send length exceeds u32::MAX",
            )));
            return Ok(());
        };

        let native_index = self.native_ops.insert(NativeIoOp::Send { buf, reply });
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Send { buf, .. }) => buf.as_ptr(),
            _ => unreachable!("native send op kind mismatch"),
        };

        let entry = opcode::Send::new(types::Fd(fd), buf_ptr, len_u32)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_fsync(
        &mut self,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Fsync { reply });
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);

        let entry = opcode::Fsync::new(types::Fd(fd))
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    fn mark_submission_pending(&mut self) -> Result<(), SendError> {
        self.pending_submit += 1;
        if self.pending_submit >= self.submit_batch_limit {
            self.flush_submissions()?;
        }
        Ok(())
    }

    fn target_fd(&self, target: ShardId) -> Result<RawFd, SendError> {
        self.ring_fds
            .get(usize::from(target))
            .copied()
            .ok_or(SendError::Closed)
    }

    fn fail_waiter(&mut self, index: usize) {
        if self.send_waiters.contains(index) {
            let waiter = self.send_waiters.remove(index);
            let _ = waiter.send(Err(SendError::Closed));
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn fail_native_op(&mut self, index: usize) {
        if self.native_ops.contains(index) {
            self.stats
                .pending_native_ops
                .fetch_sub(1, Ordering::Relaxed);
            let op = self.native_ops.remove(index);
            match op {
                NativeIoOp::Read { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring read failed",
                    )));
                }
                NativeIoOp::Write { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring write failed",
                    )));
                }
                NativeIoOp::Recv { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv failed",
                    )));
                }
                NativeIoOp::Send { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring send failed",
                    )));
                }
                NativeIoOp::Fsync { reply } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring fsync failed",
                    )));
                }
            }
        }
    }

    fn fail_all_waiters(&mut self) {
        let waiters = std::mem::take(&mut self.send_waiters);
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(SendError::Closed));
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn fail_all_native_ops(&mut self) {
        let ops = std::mem::take(&mut self.native_ops);
        self.stats
            .pending_native_ops
            .fetch_sub(ops.len() as u64, Ordering::Relaxed);
        for (_, op) in ops {
            match op {
                NativeIoOp::Read { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring read canceled",
                    )));
                }
                NativeIoOp::Write { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring write canceled",
                    )));
                }
                NativeIoOp::Recv { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv canceled",
                    )));
                }
                NativeIoOp::Send { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring send canceled",
                    )));
                }
                NativeIoOp::Fsync { reply } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring fsync canceled",
                    )));
                }
            }
        }
    }

    fn push_entry(&mut self, entry: io_uring::squeue::Entry) -> Result<(), SendError> {
        for _ in 0..2 {
            let mut sq = self.ring.submission();
            if unsafe { sq.push(&entry) }.is_ok() {
                return Ok(());
            }
            drop(sq);

            self.flush_submissions()?;
        }

        Err(SendError::Closed)
    }

    fn flush_submissions(&mut self) -> Result<(), SendError> {
        if self.pending_submit == 0 {
            return Ok(());
        }
        self.pending_submit = 0;

        if self.ring.submit().is_err() {
            self.fail_all_waiters();
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            self.fail_all_native_ops();
            return Err(SendError::Closed);
        }

        Ok(())
    }

    fn reap(&mut self, event_state: &EventState) {
        if self.flush_submissions().is_err() {
            return;
        }

        let mut doorbells = Vec::new();
        let mut msg_events = Vec::new();
        let mut waiter_completions = Vec::new();
        {
            let mut cq = self.ring.completion();
            for cqe in &mut cq {
                if cqe.flags() & MSG_RING_CQE_FLAG != 0 {
                    let (from, tag) = unpack_msg_userdata(cqe.user_data());
                    if tag == DOORBELL_TAG {
                        doorbells.push(from);
                    } else {
                        let val = u32::from_ne_bytes(cqe.result().to_ne_bytes());
                        msg_events.push((from, tag, val));
                    }
                    continue;
                }

                waiter_completions.push((cqe.user_data(), cqe.result()));
            }
        }

        for from in doorbells {
            self.drain_payload_queue(from, event_state);
        }
        for (from, tag, val) in msg_events {
            self.stats
                .ring_msgs_completed
                .fetch_add(1, Ordering::Relaxed);
            event_state.push(Event::RingMsg { from, tag, val });
        }
        for (user_data, result) in waiter_completions {
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            if let Some(native_index) = native_from_userdata(user_data) {
                self.complete_native_op(native_index, result);
                continue;
            }

            if let Some(waiter_index) = waiter_from_userdata(user_data) {
                if self.send_waiters.contains(waiter_index) {
                    let waiter = self.send_waiters.remove(waiter_index);
                    let _ = if result >= 0 {
                        waiter.send(Ok(()))
                    } else {
                        waiter.send(Err(SendError::Closed))
                    };
                }
            }
        }
    }

    fn drain_payload_queue(&self, from: ShardId, event_state: &EventState) {
        let Some(per_source) = self.payload_queues.get(usize::from(self.shard_id)) else {
            return;
        };
        let Some(queue) = per_source.get(usize::from(from)) else {
            return;
        };

        let drained = {
            let mut queue = queue.lock().expect("payload queue lock poisoned");
            queue.drain(..).collect::<Vec<_>>()
        };

        self.stats
            .ring_msgs_completed
            .fetch_add(drained.len() as u64, Ordering::Relaxed);
        for payload in drained {
            event_state.push(Event::RingMsg {
                from,
                tag: payload.tag,
                val: payload.val,
            });
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn complete_native_op(&mut self, index: usize, result: i32) {
        if !self.native_ops.contains(index) {
            return;
        }

        self.stats
            .pending_native_ops
            .fetch_sub(1, Ordering::Relaxed);
        let op = self.native_ops.remove(index);
        match op {
            NativeIoOp::Read { mut buf, reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let read_len = result as usize;
                buf.truncate(read_len);
                let _ = reply.send(Ok(buf));
            }
            NativeIoOp::Write { reply, .. } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok(result as usize));
            }
            NativeIoOp::Recv { buf, reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok((result as usize, buf)));
            }
            NativeIoOp::Send { buf, reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok((result as usize, buf)));
            }
            NativeIoOp::Fsync { reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok(()));
            }
        }
    }

    fn shutdown(&mut self) {
        self.fail_all_waiters();
        #[cfg(all(feature = "uring-native", target_os = "linux"))]
        self.fail_all_native_ops();
    }
}

fn build_stealable_inboxes(shards: usize) -> StealableInboxes {
    let mut queues = Vec::with_capacity(shards);
    for _ in 0..shards {
        queues.push(Arc::new(Mutex::new(VecDeque::new())));
    }
    Arc::new(queues)
}

#[cfg(target_os = "linux")]
fn build_payload_queues(shards: usize) -> PayloadQueues {
    let mut by_target = Vec::with_capacity(shards);
    for _ in 0..shards {
        let mut by_source = Vec::with_capacity(shards);
        for _ in 0..shards {
            by_source.push(Mutex::new(VecDeque::new()));
        }
        by_target.push(by_source);
    }
    Arc::new(by_target)
}

#[cfg(target_os = "linux")]
fn waiter_to_userdata(waiter_index: usize) -> u64 {
    waiter_index as u64 + 1
}

#[cfg(target_os = "linux")]
fn waiter_from_userdata(user_data: u64) -> Option<usize> {
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if (user_data & NATIVE_OP_USER_BIT) != 0 {
        return None;
    }
    usize::try_from(user_data.checked_sub(1)?).ok()
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_to_userdata(index: usize) -> u64 {
    NATIVE_OP_USER_BIT | (index as u64 + 1)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_from_userdata(user_data: u64) -> Option<usize> {
    if (user_data & NATIVE_OP_USER_BIT) == 0 {
        return None;
    }
    usize::try_from((user_data & !NATIVE_OP_USER_BIT).checked_sub(1)?).ok()
}

#[cfg(target_os = "linux")]
fn pack_msg_userdata(from: ShardId, tag: u16) -> u64 {
    (u64::from(from) << 16) | u64::from(tag)
}

#[cfg(target_os = "linux")]
fn unpack_msg_userdata(data: u64) -> (ShardId, u16) {
    (
        ((data >> 16) & u64::from(u16::MAX)) as ShardId,
        (data & u64::from(u16::MAX)) as u16,
    )
}

fn run_shard(
    runtime_id: u64,
    shard_id: ShardId,
    rx: Receiver<Command>,
    remotes: Vec<RemoteShard>,
    stealable_deques: StealableInboxes,
    steal_budget: usize,
    idle_wait: Duration,
    mut backend: ShardBackend,
    stats: Arc<RuntimeStatsInner>,
) {
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let event_state = Arc::new(EventState::default());
    let local_commands = Rc::new(RefCell::new(VecDeque::new()));
    let ctx = ShardCtx {
        inner: Rc::new(ShardCtxInner {
            runtime_id,
            shard_id,
            event_state: event_state.clone(),
            spawner: spawner.clone(),
            remotes,
            local_commands: local_commands.clone(),
        }),
    };

    let command_txs = ctx
        .inner
        .remotes
        .iter()
        .map(|remote| remote.shared.command_txs[usize::from(remote.id)].clone())
        .collect::<Vec<_>>();

    CURRENT_SHARD.with(|slot| {
        *slot.borrow_mut() = Some(ctx.clone());
    });

    let mut steal_cursor = (usize::from(shard_id) + 1) % stealable_deques.len().max(1);

    let mut stop = false;
    while !stop {
        pool.run_until_stalled();
        drain_local_commands(
            shard_id,
            &local_commands,
            &mut backend,
            &command_txs,
            &stats,
        );
        let stealable_drained = drain_stealable_tasks(
            shard_id,
            &stealable_deques,
            steal_budget,
            &mut steal_cursor,
            &spawner,
            &stats,
        );
        backend.poll(&event_state);

        let mut drained = stealable_drained;
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    stats.decrement_command_depth(shard_id);
                    drained = true;
                    stop = handle_command(cmd, &spawner, &event_state, &stats);
                    if stop {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }

        if stop {
            break;
        }

        if !drained {
            drain_local_commands(
                shard_id,
                &local_commands,
                &mut backend,
                &command_txs,
                &stats,
            );
            let _ = drain_stealable_tasks(
                shard_id,
                &stealable_deques,
                steal_budget,
                &mut steal_cursor,
                &spawner,
                &stats,
            );
            if backend.prefers_busy_poll() {
                thread::yield_now();
            } else {
                match rx.recv_timeout(idle_wait) {
                    Ok(cmd) => {
                        stats.decrement_command_depth(shard_id);
                        stop = handle_command(cmd, &spawner, &event_state, &stats);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => stop = true,
                }
            }
        }

        drain_local_commands(
            shard_id,
            &local_commands,
            &mut backend,
            &command_txs,
            &stats,
        );
        let _ = drain_stealable_tasks(
            shard_id,
            &stealable_deques,
            steal_budget,
            &mut steal_cursor,
            &spawner,
            &stats,
        );
        backend.poll(&event_state);
    }

    while let Ok(cmd) = rx.try_recv() {
        stats.decrement_command_depth(shard_id);
        match cmd {
            Command::InjectRawMessage { ack, .. } => {
                if let Some(ack) = ack {
                    let _ = ack.send(Err(SendError::Closed));
                }
            }
            Command::Spawn(_) | Command::StealableWake | Command::Shutdown => {}
        }
    }
    backend.shutdown();

    CURRENT_SHARD.with(|slot| {
        slot.borrow_mut().take();
    });
}

fn handle_command(
    cmd: Command,
    spawner: &LocalSpawner,
    event_state: &EventState,
    stats: &RuntimeStatsInner,
) -> bool {
    match cmd {
        Command::Spawn(fut) => {
            let _ = spawner.spawn_local(fut);
            false
        }
        Command::InjectRawMessage {
            from,
            tag,
            val,
            ack,
        } => {
            event_state.push(Event::RingMsg { from, tag, val });
            stats.ring_msgs_completed.fetch_add(1, Ordering::Relaxed);
            if let Some(ack) = ack {
                let _ = ack.send(Ok(()));
            }
            false
        }
        Command::StealableWake => false,
        Command::Shutdown => true,
    }
}

fn drain_stealable_tasks(
    shard_id: ShardId,
    stealable_deques: &StealableInboxes,
    steal_budget: usize,
    steal_cursor: &mut usize,
    spawner: &LocalSpawner,
    stats: &RuntimeStatsInner,
) -> bool {
    fn spawn_task(
        shard_id: ShardId,
        task: StealableTask,
        spawner: &LocalSpawner,
        stats: &RuntimeStatsInner,
    ) {
        stats.stealable_executed.fetch_add(1, Ordering::Relaxed);
        if task.preferred_shard != shard_id {
            stats.stealable_stolen.fetch_add(1, Ordering::Relaxed);
        }
        let _ = spawner.spawn_local(task.task);
    }

    let mut drained = false;
    let budget = steal_budget.max(1);
    let local_idx = usize::from(shard_id);
    let Some(local_deque) = stealable_deques.get(local_idx) else {
        return false;
    };
    let mut remaining = budget;

    while remaining > 0 {
        let task = local_deque
            .lock()
            .expect("stealable queue lock poisoned")
            .pop_front();
        let Some(task) = task else {
            break;
        };
        drained = true;
        remaining -= 1;
        spawn_task(shard_id, task, spawner, stats);
    }

    if remaining == 0 || stealable_deques.len() <= 1 {
        return drained;
    }

    let shard_count = stealable_deques.len();
    let max_attempts = shard_count.saturating_sub(1);
    let mut attempts = 0usize;
    while remaining > 0 && attempts < max_attempts {
        let mut victim_idx = *steal_cursor % shard_count;
        *steal_cursor = (*steal_cursor + 1) % shard_count;
        if victim_idx == local_idx {
            victim_idx = *steal_cursor % shard_count;
            *steal_cursor = (*steal_cursor + 1) % shard_count;
        }
        if victim_idx == local_idx {
            break;
        }

        attempts += 1;
        stats.steal_attempts.fetch_add(1, Ordering::Relaxed);
        let task = stealable_deques[victim_idx]
            .lock()
            .expect("stealable queue lock poisoned")
            .pop_back();
        let Some(task) = task else {
            continue;
        };

        stats.steal_success.fetch_add(1, Ordering::Relaxed);
        drained = true;
        remaining -= 1;
        spawn_task(shard_id, task, spawner, stats);
    }
    drained
}

fn drain_local_commands(
    shard_id: ShardId,
    local_commands: &RefCell<VecDeque<LocalCommand>>,
    backend: &mut ShardBackend,
    command_txs: &[Sender<Command>],
    stats: &RuntimeStatsInner,
) {
    loop {
        let cmd = local_commands.borrow_mut().pop_front();
        let Some(cmd) = cmd else {
            break;
        };

        match cmd {
            LocalCommand::SubmitRingMsg {
                target,
                tag,
                val,
                ack,
            } => backend.submit_ring_msg(shard_id, target, tag, val, ack, command_txs, stats),
            LocalCommand::SubmitRingMsgBatch { target, messages } => {
                for (tag, val) in messages {
                    backend.submit_ring_msg(shard_id, target, tag, val, None, command_txs, stats);
                }
            }
            LocalCommand::SubmitStealableWake { target } => {
                backend.submit_stealable_wake(target, command_txs, stats)
            }
            LocalCommand::Flush { ack } => backend.flush(ack),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeRead {
                origin_shard,
                fd,
                offset,
                len,
                reply,
            } => backend.submit_native_read(shard_id, origin_shard, fd, offset, len, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeWrite {
                origin_shard,
                fd,
                offset,
                buf,
                reply,
            } => backend.submit_native_write(shard_id, origin_shard, fd, offset, buf, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeRecvOwned {
                origin_shard,
                fd,
                buf,
                reply,
            } => backend.submit_native_recv(shard_id, origin_shard, fd, buf, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeSendOwned {
                origin_shard,
                fd,
                buf,
                reply,
            } => backend.submit_native_send(shard_id, origin_shard, fd, buf, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeFsync {
                origin_shard,
                fd,
                reply,
            } => backend.submit_native_fsync(shard_id, origin_shard, fd, reply, stats),
        }
    }
}
