//! Sharded async runtime API centered around `msg_ring`-style signaling.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, unbounded};
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::future::join_all;
use futures::future::{Either, select};
use futures::task::LocalSpawnExt;
use std::cell::RefCell;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::collections::HashMap;
use std::collections::VecDeque;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::ffi::CString;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::time::Instant;

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};
#[cfg(target_os = "linux")]
use slab::Slab;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub type ShardId = u16;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub type NativeOpId = u64;
const EXTERNAL_SENDER: ShardId = ShardId::MAX;
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "macros")]
pub use spargio_macros::main;

#[cfg(target_os = "linux")]
const MSG_RING_CQE_FLAG: u32 = 1 << 8;
#[cfg(target_os = "linux")]
const IOURING_SUBMIT_BATCH: usize = 64;
#[cfg(target_os = "linux")]
const DOORBELL_TAG: u16 = u16::MAX;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_OP_USER_BIT: u64 = 1 << 63;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_HOUSEKEEPING_USER_BIT: u64 = 1 << 62;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_BATCH_PART_USER_BIT: u64 = 1 << 61;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_WEAK_AFFINITY_TTL: Duration = Duration::from_millis(0);
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_STRONG_AFFINITY_TTL: Duration = Duration::from_millis(200);
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const NATIVE_HARD_AFFINITY_TTL: Duration = Duration::from_secs(5);

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn socket_addr_to_storage(addr: SocketAddr) -> (Box<libc::sockaddr_storage>, libc::socklen_t, i32) {
    let mut storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
    let (len, domain) = match addr {
        SocketAddr::V4(v4) => {
            let raw = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                std::ptr::write(
                    &mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in,
                    raw,
                );
            }
            (
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                libc::AF_INET,
            )
        }
        SocketAddr::V6(v6) => {
            let raw = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            unsafe {
                std::ptr::write(
                    &mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6,
                    raw,
                );
            }
            (
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                libc::AF_INET6,
            )
        }
    };
    (Box::new(storage), len, domain)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn socket_addr_from_storage(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> std::io::Result<SocketAddr> {
    match storage.ss_family as i32 {
        libc::AF_INET => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in>() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid sockaddr_in length from accept completion",
                ));
            }
            let raw = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(raw.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(raw.sin_port);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            if (len as usize) < std::mem::size_of::<libc::sockaddr_in6>() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid sockaddr_in6 length from accept completion",
                ));
            }
            let raw = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(raw.sin6_addr.s6_addr);
            let port = u16::from_be(raw.sin6_port);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                ip,
                port,
                raw.sin6_flowinfo,
                raw.sin6_scope_id,
            )))
        }
        family => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported sockaddr family from accept completion: {family}"),
        )),
    }
}

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

/// Runs a top-level async job on a freshly constructed runtime using default builder settings.
///
/// The provided closure receives a [`RuntimeHandle`] so the job can spawn additional work.
pub fn run<F, Fut, T>(entry: F) -> Result<T, RuntimeError>
where
    F: FnOnce(RuntimeHandle) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    run_with(Runtime::builder(), entry)
}

/// Runs a top-level async job on a freshly constructed runtime built from `builder`.
///
/// This is an ergonomic entry helper equivalent to:
/// 1) `builder.build()`
/// 2) `handle.spawn_stealable(...)`
/// 3) waiting for the returned join handle.
pub fn run_with<F, Fut, T>(builder: RuntimeBuilder, entry: F) -> Result<T, RuntimeError>
where
    F: FnOnce(RuntimeHandle) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = builder.build()?;
    let handle = runtime.handle();
    let job = entry(handle.clone());
    let join = handle.spawn_stealable(job)?;
    futures::executor::block_on(join).map_err(|_| RuntimeError::Closed)
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
    pub pending_native_ops_by_shard: Vec<usize>,
    pub native_any_envelope_submitted: u64,
    pub native_any_local_fastpath_submitted: u64,
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

    #[cfg(target_os = "linux")]
    pub fn io_uring_throughput_mode(mut self, sqpoll_idle_ms: Option<u32>) -> Self {
        self.io_uring.coop_taskrun = true;
        self.io_uring.sqpoll_idle_ms = sqpoll_idle_ms;
        if sqpoll_idle_ms.is_none() {
            self.io_uring.sqpoll_cpu = None;
        }
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
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            native_unbound: Arc::new(NativeUnboundState::new()),
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
    pub fn uring_native_unbound(&self) -> Result<UringNativeAny, RuntimeError> {
        if self.backend() != BackendKind::IoUring {
            return Err(RuntimeError::UnsupportedBackend(
                "uring-native requires io_uring backend",
            ));
        }
        Ok(UringNativeAny {
            handle: self.clone(),
            selector: NativeLaneSelector {
                shared: self.inner.shared.clone(),
            },
            preferred_shard: None,
        })
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NativeAffinityStrength {
    Weak,
    Strong,
    Hard,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Debug, Clone, Copy)]
struct FdAffinityLease {
    shard: ShardId,
    strength: NativeAffinityStrength,
    expires_at: Instant,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct FdAffinityTable {
    entries: HashMap<RawFd, FdAffinityLease>,
    weak_ttl: Duration,
    strong_ttl: Duration,
    hard_ttl: Duration,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl FdAffinityTable {
    fn new(weak_ttl: Duration, strong_ttl: Duration, hard_ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            weak_ttl,
            strong_ttl,
            hard_ttl,
        }
    }

    fn ttl_for(&self, strength: NativeAffinityStrength) -> Duration {
        match strength {
            NativeAffinityStrength::Weak => self.weak_ttl,
            NativeAffinityStrength::Strong => self.strong_ttl,
            NativeAffinityStrength::Hard => self.hard_ttl,
        }
    }

    fn get_active(&mut self, fd: RawFd, now: Instant) -> Option<FdAffinityLease> {
        let lease = self.entries.get(&fd).copied()?;
        if now <= lease.expires_at {
            return Some(lease);
        }
        self.entries.remove(&fd);
        None
    }

    fn upsert(
        &mut self,
        fd: RawFd,
        shard: ShardId,
        strength: NativeAffinityStrength,
        now: Instant,
    ) {
        let upgraded = self
            .entries
            .get(&fd)
            .map(|lease| lease.strength.max(strength))
            .unwrap_or(strength);
        self.entries.insert(
            fd,
            FdAffinityLease {
                shard,
                strength: upgraded,
                expires_at: now + self.ttl_for(upgraded),
            },
        );
    }

    fn release_if(&mut self, fd: RawFd, shard: ShardId) {
        if self
            .entries
            .get(&fd)
            .is_some_and(|lease| lease.shard == shard)
        {
            self.entries.remove(&fd);
        }
    }

    fn current_shard(&mut self, fd: RawFd, now: Instant) -> Option<ShardId> {
        self.get_active(fd, now).map(|lease| lease.shard)
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeUnboundState {
    selector_cursor: AtomicUsize,
    next_op_id: AtomicU64,
    fd_affinity: Mutex<FdAffinityTable>,
    op_routes: Mutex<HashMap<NativeOpId, ShardId>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl NativeUnboundState {
    fn new() -> Self {
        Self {
            selector_cursor: AtomicUsize::new(0),
            next_op_id: AtomicU64::new(1),
            fd_affinity: Mutex::new(FdAffinityTable::new(
                NATIVE_WEAK_AFFINITY_TTL,
                NATIVE_STRONG_AFFINITY_TTL,
                NATIVE_HARD_AFFINITY_TTL,
            )),
            op_routes: Mutex::new(HashMap::new()),
        }
    }

    fn next_op_id(&self) -> NativeOpId {
        self.next_op_id.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
pub struct NativeLaneSelector {
    shared: Arc<RuntimeShared>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl NativeLaneSelector {
    pub fn select(&self, preferred_shard: Option<ShardId>) -> ShardId {
        let shard_count = self.shared.command_txs.len();
        if shard_count == 0 {
            return 0;
        }

        let start = self
            .shared
            .native_unbound
            .selector_cursor
            .fetch_add(1, Ordering::Relaxed)
            % shard_count;

        let mut best_idx = start;
        let mut best_depth = usize::MAX;
        for offset in 0..shard_count {
            let idx = (start + offset) % shard_count;
            let depth = self.shared.stats.pending_native_depth(idx as ShardId);
            if depth < best_depth {
                best_depth = depth;
                best_idx = idx;
            }
        }

        if let Some(preferred) = preferred_shard {
            let preferred_idx = usize::from(preferred);
            if preferred_idx < shard_count {
                let preferred_depth = self.shared.stats.pending_native_depth(preferred);
                if preferred_depth <= best_depth.saturating_add(1) {
                    return preferred;
                }
            }
        }

        best_idx as ShardId
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
pub struct UringNativeAny {
    handle: RuntimeHandle,
    selector: NativeLaneSelector,
    preferred_shard: Option<ShardId>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl UringNativeAny {
    pub fn preferred_shard(&self) -> Option<ShardId> {
        self.preferred_shard
    }

    pub fn with_preferred_shard(&self, preferred_shard: ShardId) -> Result<Self, RuntimeError> {
        if usize::from(preferred_shard) >= self.handle.shard_count() {
            return Err(RuntimeError::InvalidShard(preferred_shard));
        }
        Ok(Self {
            handle: self.handle.clone(),
            selector: self.selector.clone(),
            preferred_shard: Some(preferred_shard),
        })
    }

    pub fn clear_preferred_shard(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            selector: self.selector.clone(),
            preferred_shard: None,
        }
    }

    pub fn select_shard(&self, preferred_shard: Option<ShardId>) -> Result<ShardId, RuntimeError> {
        if let Some(preferred) = preferred_shard {
            if usize::from(preferred) >= self.handle.shard_count() {
                return Err(RuntimeError::InvalidShard(preferred));
            }
        }
        Ok(self
            .selector
            .select(preferred_shard.or(self.preferred_shard)))
    }

    pub fn fd_affinity_shard(&self, fd: RawFd) -> Option<ShardId> {
        let now = Instant::now();
        self.handle
            .inner
            .shared
            .native_unbound
            .fd_affinity
            .lock()
            .expect("native fd affinity lock poisoned")
            .current_shard(fd, now)
    }

    pub fn active_native_op_count(&self) -> usize {
        self.handle
            .inner
            .shared
            .native_unbound
            .op_routes
            .lock()
            .expect("native op route lock poisoned")
            .len()
    }

    pub fn active_native_op_shard(&self, op_id: NativeOpId) -> Option<ShardId> {
        self.handle
            .inner
            .shared
            .native_unbound
            .op_routes
            .lock()
            .expect("native op route lock poisoned")
            .get(&op_id)
            .copied()
    }

    pub async fn read_at(&self, fd: RawFd, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Weak,
            false,
            |reply| NativeAnyCommand::Read {
                fd,
                offset,
                len,
                reply,
            },
            "native unbound read response channel closed",
        )
        .await
    }

    pub async fn read_at_into(
        &self,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
    ) -> std::io::Result<(usize, Vec<u8>)> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Weak,
            false,
            |reply| NativeAnyCommand::ReadOwned {
                fd,
                offset,
                buf,
                reply,
            },
            "native unbound read response channel closed",
        )
        .await
    }

    pub async fn write_at(&self, fd: RawFd, offset: u64, buf: &[u8]) -> std::io::Result<usize> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Weak,
            false,
            |reply| NativeAnyCommand::Write {
                fd,
                offset,
                buf: buf.to_vec(),
                reply,
            },
            "native unbound write response channel closed",
        )
        .await
    }

    pub async fn fsync(&self, fd: RawFd) -> std::io::Result<()> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Weak,
            false,
            |reply| NativeAnyCommand::Fsync { fd, reply },
            "native unbound fsync response channel closed",
        )
        .await
    }

    pub(crate) async fn open_at(
        &self,
        path: CString,
        flags: i32,
        mode: libc::mode_t,
    ) -> std::io::Result<OwnedFd> {
        let shard = self.selector.select(self.effective_preferred_shard());
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::OpenAt {
                path,
                flags,
                mode,
                reply,
            },
            "native unbound open response channel closed",
        )
        .await
    }

    pub(crate) async fn connect_on_shard(
        &self,
        shard: ShardId,
        socket_addr: SocketAddr,
    ) -> std::io::Result<OwnedFd> {
        let (addr, addr_len, domain) = socket_addr_to_storage(socket_addr);
        let raw_fd = unsafe {
            libc::socket(
                domain,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::Connect {
                socket,
                addr,
                addr_len,
                reply,
            },
            "native unbound connect response channel closed",
        )
        .await
    }

    pub(crate) async fn accept_on_shard(
        &self,
        shard: ShardId,
        listener_fd: RawFd,
    ) -> std::io::Result<(OwnedFd, SocketAddr)> {
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::Accept {
                fd: listener_fd,
                reply,
            },
            "native unbound accept response channel closed",
        )
        .await
    }

    pub async fn recv(&self, fd: RawFd, len: usize) -> std::io::Result<Vec<u8>> {
        let (got, mut buf) = self.recv_owned(fd, vec![0; len]).await?;
        buf.truncate(got.min(buf.len()));
        Ok(buf)
    }

    pub async fn recv_owned(&self, fd: RawFd, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Strong,
            false,
            |reply| NativeAnyCommand::RecvOwned { fd, buf, reply },
            "native unbound recv response channel closed",
        )
        .await
    }

    pub async fn recv_into(&self, fd: RawFd, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        self.recv_owned(fd, buf).await
    }

    pub async fn send(&self, fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
        let payload = buf.to_vec();
        let (sent, _) = self.send_owned(fd, payload).await?;
        Ok(sent)
    }

    pub async fn send_owned(&self, fd: RawFd, buf: Vec<u8>) -> std::io::Result<(usize, Vec<u8>)> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Strong,
            false,
            |reply| NativeAnyCommand::SendOwned { fd, buf, reply },
            "native unbound send response channel closed",
        )
        .await
    }

    pub async fn send_batch(
        &self,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
    ) -> std::io::Result<(usize, Vec<Vec<u8>>)> {
        self.send_all_batch(fd, bufs, window).await
    }

    pub async fn send_all_batch(
        &self,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
    ) -> std::io::Result<(usize, Vec<Vec<u8>>)> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Strong,
            false,
            |reply| NativeAnyCommand::SendBatchOwned {
                fd,
                bufs,
                window,
                reply,
            },
            "native unbound send batch response channel closed",
        )
        .await
    }

    pub async fn recv_batch_into(
        &self,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
    ) -> std::io::Result<(usize, Vec<Vec<u8>>)> {
        let mut pending = VecDeque::from(bufs);
        let mut returned = Vec::new();
        let mut total_received = 0usize;
        let window = window.max(1);

        while !pending.is_empty() {
            let mut recvs = Vec::with_capacity(window);
            for _ in 0..window {
                if let Some(buf) = pending.pop_front() {
                    recvs.push(self.recv_into(fd, buf));
                } else {
                    break;
                }
            }
            for out in join_all(recvs).await {
                let (received, buf) = out?;
                total_received = total_received.saturating_add(received);
                returned.push(buf);
            }
        }

        Ok((total_received, returned))
    }

    pub async fn recv_multishot_segments(
        &self,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
    ) -> std::io::Result<UringRecvMultishotSegments> {
        self.submit_tracked(
            fd,
            NativeAffinityStrength::Hard,
            true,
            |reply| NativeAnyCommand::RecvMultishot {
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
            },
            "native unbound recv multishot response channel closed",
        )
        .await
    }

    pub async fn recv_multishot(
        &self,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
    ) -> std::io::Result<Vec<Vec<u8>>> {
        let out = self
            .recv_multishot_segments(fd, buffer_len, buffer_count, bytes_target)
            .await?;
        let mut chunks = Vec::with_capacity(out.segments.len());
        for seg in out.segments {
            let end = seg.offset.saturating_add(seg.len).min(out.buffer.len());
            if seg.offset >= end {
                chunks.push(Vec::new());
            } else {
                chunks.push(out.buffer[seg.offset..end].to_vec());
            }
        }
        Ok(chunks)
    }

    pub(crate) fn select_stream_session_shard(&self) -> ShardId {
        self.selector.select(self.effective_preferred_shard())
    }

    pub(crate) async fn recv_owned_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
    ) -> std::io::Result<(usize, Vec<u8>)> {
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::RecvOwned { fd, buf, reply },
            "native stream-session recv response channel closed",
        )
        .await
    }

    pub(crate) async fn send_owned_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
    ) -> std::io::Result<(usize, Vec<u8>)> {
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::SendOwned { fd, buf, reply },
            "native stream-session send response channel closed",
        )
        .await
    }

    pub(crate) async fn send_all_batch_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
    ) -> std::io::Result<(usize, Vec<Vec<u8>>)> {
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::SendBatchOwned {
                fd,
                bufs,
                window,
                reply,
            },
            "native stream-session send batch response channel closed",
        )
        .await
    }

    pub(crate) async fn recv_multishot_segments_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
    ) -> std::io::Result<UringRecvMultishotSegments> {
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::RecvMultishot {
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
            },
            "native stream-session recv multishot response channel closed",
        )
        .await
    }

    fn effective_preferred_shard(&self) -> Option<ShardId> {
        self.preferred_shard.or_else(|| {
            ShardCtx::current().and_then(|ctx| {
                (ctx.runtime_id() == self.handle.inner.shared.runtime_id).then_some(ctx.shard_id())
            })
        })
    }

    fn select_shard_for_fd(&self, fd: RawFd, strength: NativeAffinityStrength) -> ShardId {
        let now = Instant::now();
        {
            let mut table = self
                .handle
                .inner
                .shared
                .native_unbound
                .fd_affinity
                .lock()
                .expect("native fd affinity lock poisoned");
            if let Some(lease) = table.get_active(fd, now) {
                table.upsert(fd, lease.shard, strength, now);
                return lease.shard;
            }
        }

        let preferred = self.effective_preferred_shard();
        let selected = self.selector.select(preferred);
        let mut table = self
            .handle
            .inner
            .shared
            .native_unbound
            .fd_affinity
            .lock()
            .expect("native fd affinity lock poisoned");
        if let Some(lease) = table.get_active(fd, now) {
            table.upsert(fd, lease.shard, strength, now);
            return lease.shard;
        }
        table.upsert(fd, selected, strength, now);
        selected
    }

    fn begin_op(&self, fd: RawFd, strength: NativeAffinityStrength) -> (NativeOpId, ShardId) {
        let shard = self.select_shard_for_fd(fd, strength);
        let op_id = self.handle.inner.shared.native_unbound.next_op_id();
        self.handle
            .inner
            .shared
            .native_unbound
            .op_routes
            .lock()
            .expect("native op route lock poisoned")
            .insert(op_id, shard);
        (op_id, shard)
    }

    fn finish_op(&self, op_id: NativeOpId, fd: RawFd, shard: ShardId, release_affinity: bool) {
        self.handle
            .inner
            .shared
            .native_unbound
            .op_routes
            .lock()
            .expect("native op route lock poisoned")
            .remove(&op_id);
        if release_affinity {
            self.handle
                .inner
                .shared
                .native_unbound
                .fd_affinity
                .lock()
                .expect("native fd affinity lock poisoned")
                .release_if(fd, shard);
        }
    }

    fn dispatch_native_any(&self, shard: ShardId, op: NativeAnyCommand) -> std::io::Result<()> {
        if let Some(ctx) = ShardCtx::current()
            .filter(|ctx| ctx.runtime_id() == self.handle.inner.shared.runtime_id)
            .filter(|ctx| ctx.shard_id() == shard)
        {
            self.handle
                .inner
                .shared
                .stats
                .native_any_local_fastpath_submitted
                .fetch_add(1, Ordering::Relaxed);
            ctx.inner
                .local_commands
                .borrow_mut()
                .push_back(op.into_local(shard));
            return Ok(());
        }

        self.handle
            .inner
            .shared
            .stats
            .native_any_envelope_submitted
            .fetch_add(1, Ordering::Relaxed);
        let Some(tx) = self.handle.inner.shared.command_txs.get(usize::from(shard)) else {
            op.fail_closed();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native unbound submit command channel closed",
            ));
        };
        self.handle
            .inner
            .shared
            .stats
            .increment_command_depth(shard);
        match tx.send(Command::SubmitNativeAny { op }) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.handle
                    .inner
                    .shared
                    .stats
                    .decrement_command_depth(shard);
                match err.0 {
                    Command::SubmitNativeAny { op } => op.fail_closed(),
                    _ => unreachable!("native unbound command type mismatch"),
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound submit command channel closed",
                ))
            }
        }
    }

    async fn submit_direct<T, B>(
        &self,
        shard: ShardId,
        build: B,
        closed_msg: &'static str,
    ) -> std::io::Result<T>
    where
        B: FnOnce(oneshot::Sender<std::io::Result<T>>) -> NativeAnyCommand,
    {
        if usize::from(shard) >= self.handle.shard_count() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("invalid shard {shard}"),
            ));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = build(reply_tx);
        self.dispatch_native_any(shard, cmd)?;
        reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                closed_msg,
            ))
        })
    }

    async fn submit_tracked<T, B>(
        &self,
        fd: RawFd,
        strength: NativeAffinityStrength,
        release_affinity: bool,
        build: B,
        closed_msg: &'static str,
    ) -> std::io::Result<T>
    where
        B: FnOnce(oneshot::Sender<std::io::Result<T>>) -> NativeAnyCommand,
    {
        let (op_id, shard) = self.begin_op(fd, strength);
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = build(reply_tx);
        if let Err(err) = self.dispatch_native_any(shard, cmd) {
            self.finish_op(op_id, fd, shard, release_affinity);
            return Err(err);
        }
        let out = reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                closed_msg,
            ))
        });
        self.finish_op(op_id, fd, shard, release_affinity);
        out
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UringRecvSegment {
    pub offset: usize,
    pub len: usize,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Debug, Clone)]
pub struct UringRecvMultishotSegments {
    pub buffer: Vec<u8>,
    pub segments: Vec<UringRecvSegment>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod fs {
    use super::{RuntimeError, RuntimeHandle, UringNativeAny};
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Arc;

    const READ_TO_END_CHUNK: usize = 64 * 1024;

    #[derive(Debug, Clone, Copy, Default)]
    pub struct OpenOptions {
        read: bool,
        write: bool,
        append: bool,
        truncate: bool,
        create: bool,
        create_new: bool,
    }

    impl OpenOptions {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn read(mut self, read: bool) -> Self {
            self.read = read;
            self
        }

        pub fn write(mut self, write: bool) -> Self {
            self.write = write;
            self
        }

        pub fn append(mut self, append: bool) -> Self {
            self.append = append;
            self
        }

        pub fn truncate(mut self, truncate: bool) -> Self {
            self.truncate = truncate;
            self
        }

        pub fn create(mut self, create: bool) -> Self {
            self.create = create;
            self
        }

        pub fn create_new(mut self, create_new: bool) -> Self {
            self.create_new = create_new;
            self
        }

        pub async fn open<P: AsRef<Path>>(
            &self,
            handle: RuntimeHandle,
            path: P,
        ) -> io::Result<File> {
            let native = handle
                .uring_native_unbound()
                .map_err(runtime_error_to_io_for_native)?;
            let (flags, mode) = self.to_open_flags()?;
            let path = path_to_cstring(path.as_ref())?;
            let fd = native.open_at(path, flags, mode).await?;
            Ok(File {
                native,
                fd: Arc::new(fd),
            })
        }

        fn to_open_flags(&self) -> io::Result<(i32, libc::mode_t)> {
            if !self.read && !self.write && !self.append {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "must specify at least one of read, write, or append access",
                ));
            }

            if self.truncate && self.append {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "creating or truncating a file requires write or append access",
                ));
            }

            if (self.truncate || self.create || self.create_new) && !(self.write || self.append) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "creating or truncating a file requires write or append access",
                ));
            }

            let access_flags = match (self.read, self.write, self.append) {
                (true, false, false) => libc::O_RDONLY,
                (false, true, false) => libc::O_WRONLY,
                (true, true, false) => libc::O_RDWR,
                (false, _, true) => libc::O_WRONLY | libc::O_APPEND,
                (true, _, true) => libc::O_RDWR | libc::O_APPEND,
                (false, false, false) => unreachable!("validated above"),
            };

            let mut flags = access_flags | libc::O_CLOEXEC;
            if self.create {
                flags |= libc::O_CREAT;
            }
            if self.create_new {
                flags |= libc::O_CREAT | libc::O_EXCL;
            }
            if self.truncate {
                flags |= libc::O_TRUNC;
            }
            Ok((flags, 0o666))
        }
    }

    #[derive(Clone)]
    pub struct File {
        native: UringNativeAny,
        fd: Arc<OwnedFd>,
    }

    impl File {
        pub async fn open<P: AsRef<Path>>(handle: RuntimeHandle, path: P) -> io::Result<Self> {
            OpenOptions::new().read(true).open(handle, path).await
        }

        pub async fn create<P: AsRef<Path>>(handle: RuntimeHandle, path: P) -> io::Result<Self> {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(handle, path)
                .await
        }

        pub fn from_std(handle: RuntimeHandle, file: std::fs::File) -> io::Result<Self> {
            let native = handle
                .uring_native_unbound()
                .map_err(runtime_error_to_io_for_native)?;
            Ok(Self {
                native,
                fd: Arc::new(file.into()),
            })
        }

        pub fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }

        pub async fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            self.native.read_at(self.as_raw_fd(), offset, len).await
        }

        pub async fn read_at_into(
            &self,
            offset: u64,
            buf: Vec<u8>,
        ) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .read_at_into(self.as_raw_fd(), offset, buf)
                .await
        }

        pub async fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<usize> {
            self.native.write_at(self.as_raw_fd(), offset, buf).await
        }

        pub async fn write_all_at(&self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
            while !buf.is_empty() {
                let wrote = self.write_at(offset, buf).await?;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write_at returned zero",
                    ));
                }
                offset = offset.saturating_add(wrote as u64);
                buf = &buf[wrote.min(buf.len())..];
            }
            Ok(())
        }

        pub async fn read_to_end_at(&self, mut offset: u64) -> io::Result<Vec<u8>> {
            let mut out = Vec::new();
            loop {
                let (got, buf) = self
                    .read_at_into(offset, vec![0; READ_TO_END_CHUNK])
                    .await?;
                if got == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..got.min(buf.len())]);
                offset = offset.saturating_add(got as u64);
                if got < READ_TO_END_CHUNK {
                    break;
                }
            }
            Ok(out)
        }

        pub async fn fsync(&self) -> io::Result<()> {
            self.native.fsync(self.as_raw_fd()).await
        }
    }

    fn path_to_cstring(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            )
        })
    }

    fn runtime_error_to_io_for_native(err: RuntimeError) -> io::Error {
        match err {
            RuntimeError::InvalidConfig(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            RuntimeError::ThreadSpawn(io) => io,
            RuntimeError::InvalidShard(shard) => {
                io::Error::new(io::ErrorKind::NotFound, format!("invalid shard {shard}"))
            }
            RuntimeError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"),
            RuntimeError::Overloaded => {
                io::Error::new(io::ErrorKind::WouldBlock, "runtime overloaded")
            }
            RuntimeError::UnsupportedBackend(msg) => {
                io::Error::new(io::ErrorKind::Unsupported, msg)
            }
            RuntimeError::IoUringInit(io) => io,
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod net {
    use super::{
        JoinHandle, RuntimeError, RuntimeHandle, ShardId, UringNativeAny,
        UringRecvMultishotSegments,
    };
    use std::future::Future;
    use std::io;
    use std::net::{
        SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, ToSocketAddrs,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StreamSessionPolicy {
        ContextPreferred,
        RoundRobin,
        Fixed(ShardId),
    }

    impl Default for StreamSessionPolicy {
        fn default() -> Self {
            Self::ContextPreferred
        }
    }

    #[derive(Clone)]
    pub struct TcpStream {
        native: UringNativeAny,
        fd: Arc<OwnedFd>,
        session_shard: ShardId,
    }

    impl TcpStream {
        pub async fn connect<A>(handle: RuntimeHandle, addr: A) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            Self::connect_with_session_policy(handle, addr, StreamSessionPolicy::ContextPreferred)
                .await
        }

        pub async fn connect_round_robin<A>(handle: RuntimeHandle, addr: A) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            Self::connect_with_session_policy(handle, addr, StreamSessionPolicy::RoundRobin).await
        }

        pub async fn connect_with_session_policy<A>(
            handle: RuntimeHandle,
            addr: A,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = first_socket_addr(addr)?;
            Self::connect_socket_addr_with_session_policy(handle, socket_addr, policy).await
        }

        pub async fn connect_many_round_robin<A>(
            handle: RuntimeHandle,
            addr: A,
            count: usize,
        ) -> io::Result<Vec<Self>>
        where
            A: ToSocketAddrs,
        {
            Self::connect_many_with_session_policy(
                handle,
                addr,
                count,
                StreamSessionPolicy::RoundRobin,
            )
            .await
        }

        pub async fn connect_many_with_session_policy<A>(
            handle: RuntimeHandle,
            addr: A,
            count: usize,
            policy: StreamSessionPolicy,
        ) -> io::Result<Vec<Self>>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = first_socket_addr(addr)?;
            let mut streams = Vec::with_capacity(count);
            for _ in 0..count {
                streams.push(
                    Self::connect_socket_addr_with_session_policy(
                        handle.clone(),
                        socket_addr,
                        policy,
                    )
                    .await?,
                );
            }
            Ok(streams)
        }

        pub fn from_std(handle: RuntimeHandle, stream: StdTcpStream) -> io::Result<Self> {
            Self::from_std_with_session_policy(
                handle,
                stream,
                StreamSessionPolicy::ContextPreferred,
            )
        }

        pub fn from_std_with_session_policy(
            handle: RuntimeHandle,
            stream: StdTcpStream,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self> {
            let (native, session_shard) = select_native_for_policy(handle, policy)?;
            stream.set_nonblocking(true)?;
            let _ = stream.set_nodelay(true);
            Ok(Self {
                native,
                fd: Arc::new(stream.into()),
                session_shard,
            })
        }

        async fn connect_socket_addr_with_session_policy(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self> {
            let (native, session_shard) = select_native_for_policy(handle, policy)?;
            let socket = native.connect_on_shard(session_shard, socket_addr).await?;
            let stream = StdTcpStream::from(socket);
            stream.set_nonblocking(true)?;
            let _ = stream.set_nodelay(true);
            Ok(Self {
                native,
                fd: Arc::new(stream.into()),
                session_shard,
            })
        }

        pub fn as_raw_fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }

        pub fn session_shard(&self) -> ShardId {
            self.session_shard
        }

        pub fn spawn_on_session<F, T>(
            &self,
            handle: &RuntimeHandle,
            fut: F,
        ) -> Result<JoinHandle<T>, RuntimeError>
        where
            F: Future<Output = T> + Send + 'static,
            T: Send + 'static,
        {
            handle.spawn_pinned(self.session_shard, fut)
        }

        pub fn spawn_stealable_on_session<F, T>(
            &self,
            handle: &RuntimeHandle,
            fut: F,
        ) -> Result<JoinHandle<T>, RuntimeError>
        where
            F: Future<Output = T> + Send + 'static,
            T: Send + 'static,
        {
            handle.spawn_stealable_on(self.session_shard, fut)
        }

        pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
            let (sent, _) = self.send_owned(buf.to_vec()).await?;
            Ok(sent)
        }

        pub async fn recv(&self, len: usize) -> io::Result<Vec<u8>> {
            let (got, mut buf) = self.recv_owned(vec![0; len]).await?;
            buf.truncate(got.min(buf.len()));
            Ok(buf)
        }

        pub async fn send_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .send_owned_on_shard(self.session_shard, self.as_raw_fd(), buf)
                .await
        }

        pub async fn recv_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .recv_owned_on_shard(self.session_shard, self.as_raw_fd(), buf)
                .await
        }

        pub async fn send_all_batch(
            &self,
            bufs: Vec<Vec<u8>>,
            window: usize,
        ) -> io::Result<(usize, Vec<Vec<u8>>)> {
            self.native
                .send_all_batch_on_shard(self.session_shard, self.as_raw_fd(), bufs, window)
                .await
        }

        pub async fn recv_multishot_segments(
            &self,
            buffer_len: usize,
            buffer_count: u16,
            bytes_target: usize,
        ) -> io::Result<UringRecvMultishotSegments> {
            self.native
                .recv_multishot_segments_on_shard(
                    self.session_shard,
                    self.as_raw_fd(),
                    buffer_len,
                    buffer_count,
                    bytes_target,
                )
                .await
        }

        pub async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
            while !buf.is_empty() {
                let wrote = self.send(buf).await?;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "send returned zero",
                    ));
                }
                buf = &buf[wrote.min(buf.len())..];
            }
            Ok(())
        }

        pub async fn write_all_owned(&self, mut buf: Vec<u8>) -> io::Result<Vec<u8>> {
            let mut sent = 0usize;
            while sent < buf.len() {
                if sent == 0 {
                    let (wrote, returned) = self.send_owned(buf).await?;
                    if wrote == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "send returned zero",
                        ));
                    }
                    sent = sent.saturating_add(wrote);
                    buf = returned;
                    continue;
                }

                let wrote = self.send(&buf[sent..]).await?;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "send returned zero",
                    ));
                }
                sent = sent.saturating_add(wrote);
            }

            Ok(buf)
        }

        pub async fn read_exact(&self, dst: &mut [u8]) -> io::Result<()> {
            let mut received = 0usize;
            let mut scratch = vec![0; dst.len().max(1)];
            while received < dst.len() {
                let want = dst.len().saturating_sub(received);
                if scratch.len() != want {
                    scratch.resize(want, 0);
                }
                let (got, buf) = self.recv_owned(scratch).await?;
                scratch = buf;
                if got == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream closed",
                    ));
                }
                let copy_len = got
                    .min(scratch.len())
                    .min(dst.len().saturating_sub(received));
                dst[received..received + copy_len].copy_from_slice(&scratch[..copy_len]);
                received += copy_len;
            }
            Ok(())
        }

        pub async fn read_exact_owned(&self, mut dst: Vec<u8>) -> io::Result<Vec<u8>> {
            self.read_exact(dst.as_mut_slice()).await?;
            Ok(dst)
        }
    }

    #[derive(Clone)]
    pub struct TcpListener {
        handle: RuntimeHandle,
        listener: Arc<StdTcpListener>,
    }

    impl TcpListener {
        pub async fn bind<A>(handle: RuntimeHandle, addr: A) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = first_socket_addr(addr)?;
            let listener = bind_std_listener(socket_addr)?;
            Ok(Self {
                handle,
                listener: Arc::new(listener),
            })
        }

        pub fn from_std(handle: RuntimeHandle, listener: StdTcpListener) -> Self {
            let _ = listener.set_nonblocking(true);
            Self {
                handle,
                listener: Arc::new(listener),
            }
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.listener.local_addr()
        }

        pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
            self.accept_with_session_policy(StreamSessionPolicy::ContextPreferred)
                .await
        }

        pub async fn accept_round_robin(&self) -> io::Result<(TcpStream, SocketAddr)> {
            self.accept_with_session_policy(StreamSessionPolicy::RoundRobin)
                .await
        }

        pub async fn accept_with_session_policy(
            &self,
            policy: StreamSessionPolicy,
        ) -> io::Result<(TcpStream, SocketAddr)> {
            let handle = self.handle.clone();
            let native = handle
                .uring_native_unbound()
                .map_err(runtime_error_to_io_for_native)?;
            let accept_shard = native
                .select_shard(None)
                .map_err(runtime_error_to_io_for_native)?;
            let (socket, addr) = native
                .accept_on_shard(accept_shard, self.listener.as_raw_fd())
                .await?;
            let stream = StdTcpStream::from(socket);
            let stream = TcpStream::from_std_with_session_policy(handle, stream, policy)?;
            Ok((stream, addr))
        }
    }

    fn select_native_for_policy(
        handle: RuntimeHandle,
        policy: StreamSessionPolicy,
    ) -> io::Result<(UringNativeAny, ShardId)> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;

        match policy {
            StreamSessionPolicy::ContextPreferred => {
                let shard = native.select_stream_session_shard();
                Ok((native, shard))
            }
            StreamSessionPolicy::RoundRobin => {
                let shard = native
                    .select_shard(None)
                    .map_err(runtime_error_to_io_for_native)?;
                Ok((native, shard))
            }
            StreamSessionPolicy::Fixed(shard) => {
                let native = native
                    .with_preferred_shard(shard)
                    .map_err(runtime_error_to_io_for_native)?;
                Ok((native, shard))
            }
        }
    }

    fn first_socket_addr<A>(addr: A) -> io::Result<SocketAddr>
    where
        A: ToSocketAddrs,
    {
        addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket address resolution produced no results",
            )
        })
    }

    fn bind_std_listener(socket_addr: SocketAddr) -> io::Result<StdTcpListener> {
        let (addr, addr_len, domain) = super::socket_addr_to_storage(socket_addr);
        let raw_fd = unsafe {
            libc::socket(
                domain,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let one: libc::c_int = 1;
        let set_reuse = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set_reuse < 0 {
            return Err(io::Error::last_os_error());
        }

        let bind_result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                addr.as_ref() as *const libc::sockaddr_storage as *const libc::sockaddr,
                addr_len,
            )
        };
        if bind_result < 0 {
            return Err(io::Error::last_os_error());
        }

        let listen_result = unsafe { libc::listen(fd.as_raw_fd(), libc::SOMAXCONN) };
        if listen_result < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(fd.into())
    }

    fn runtime_error_to_io_for_native(err: RuntimeError) -> io::Error {
        match err {
            RuntimeError::InvalidConfig(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            RuntimeError::ThreadSpawn(io) => io,
            RuntimeError::InvalidShard(shard) => {
                io::Error::new(io::ErrorKind::NotFound, format!("invalid shard {shard}"))
            }
            RuntimeError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"),
            RuntimeError::Overloaded => {
                io::Error::new(io::ErrorKind::WouldBlock, "runtime overloaded")
            }
            RuntimeError::UnsupportedBackend(msg) => {
                io::Error::new(io::ErrorKind::Unsupported, msg)
            }
            RuntimeError::IoUringInit(io) => io,
        }
    }
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
    pending_native_ops_by_shard: Vec<AtomicUsize>,
    native_any_envelope_submitted: AtomicU64,
    native_any_local_fastpath_submitted: AtomicU64,
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
        let mut pending_native_ops_by_shard = Vec::with_capacity(shards);
        for _ in 0..shards {
            shard_command_depths.push(AtomicUsize::new(0));
            pending_native_ops_by_shard.push(AtomicUsize::new(0));
        }

        Self {
            shard_command_depths,
            pending_native_ops_by_shard,
            native_any_envelope_submitted: AtomicU64::new(0),
            native_any_local_fastpath_submitted: AtomicU64::new(0),
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
            pending_native_ops_by_shard: self
                .pending_native_ops_by_shard
                .iter()
                .map(|depth| depth.load(Ordering::Relaxed))
                .collect(),
            native_any_envelope_submitted: self
                .native_any_envelope_submitted
                .load(Ordering::Relaxed),
            native_any_local_fastpath_submitted: self
                .native_any_local_fastpath_submitted
                .load(Ordering::Relaxed),
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

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn increment_pending_native_depth(&self, shard: ShardId) {
        if let Some(depth) = self.pending_native_ops_by_shard.get(usize::from(shard)) {
            depth.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn decrement_pending_native_depth(&self, shard: ShardId, by: usize) {
        if by == 0 {
            return;
        }
        if let Some(depth) = self.pending_native_ops_by_shard.get(usize::from(shard)) {
            let _ = depth.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(by))
            });
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn pending_native_depth(&self, shard: ShardId) -> usize {
        self.pending_native_ops_by_shard
            .get(usize::from(shard))
            .map_or(0, |depth| depth.load(Ordering::Relaxed))
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
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    native_unbound: Arc<NativeUnboundState>,
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
    SubmitNativeReadOwned {
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
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
    SubmitNativeSendBatchOwned {
        origin_shard: ShardId,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<Vec<u8>>)>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeRecvMultishot {
        origin_shard: ShardId,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
        reply: oneshot::Sender<std::io::Result<UringRecvMultishotSegments>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeFsync {
        origin_shard: ShardId,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeOpenAt {
        origin_shard: ShardId,
        path: CString,
        flags: i32,
        mode: libc::mode_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeConnect {
        origin_shard: ShardId,
        socket: OwnedFd,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeAccept {
        origin_shard: ShardId,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<(OwnedFd, SocketAddr)>>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum NativeAnyCommand {
    Read {
        fd: RawFd,
        offset: u64,
        len: usize,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    },
    ReadOwned {
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    Write {
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<usize>>,
    },
    RecvOwned {
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    SendOwned {
        fd: RawFd,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    SendBatchOwned {
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<Vec<u8>>)>>,
    },
    RecvMultishot {
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
        reply: oneshot::Sender<std::io::Result<UringRecvMultishotSegments>>,
    },
    Fsync {
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
    },
    OpenAt {
        path: CString,
        flags: i32,
        mode: libc::mode_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    Connect {
        socket: OwnedFd,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    Accept {
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<(OwnedFd, SocketAddr)>>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl NativeAnyCommand {
    fn into_local(self, origin_shard: ShardId) -> LocalCommand {
        match self {
            Self::Read {
                fd,
                offset,
                len,
                reply,
            } => LocalCommand::SubmitNativeRead {
                origin_shard,
                fd,
                offset,
                len,
                reply,
            },
            Self::ReadOwned {
                fd,
                offset,
                buf,
                reply,
            } => LocalCommand::SubmitNativeReadOwned {
                origin_shard,
                fd,
                offset,
                buf,
                reply,
            },
            Self::Write {
                fd,
                offset,
                buf,
                reply,
            } => LocalCommand::SubmitNativeWrite {
                origin_shard,
                fd,
                offset,
                buf,
                reply,
            },
            Self::RecvOwned { fd, buf, reply } => LocalCommand::SubmitNativeRecvOwned {
                origin_shard,
                fd,
                buf,
                reply,
            },
            Self::SendOwned { fd, buf, reply } => LocalCommand::SubmitNativeSendOwned {
                origin_shard,
                fd,
                buf,
                reply,
            },
            Self::SendBatchOwned {
                fd,
                bufs,
                window,
                reply,
            } => LocalCommand::SubmitNativeSendBatchOwned {
                origin_shard,
                fd,
                bufs,
                window,
                reply,
            },
            Self::RecvMultishot {
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
            } => LocalCommand::SubmitNativeRecvMultishot {
                origin_shard,
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
            },
            Self::Fsync { fd, reply } => LocalCommand::SubmitNativeFsync {
                origin_shard,
                fd,
                reply,
            },
            Self::OpenAt {
                path,
                flags,
                mode,
                reply,
            } => LocalCommand::SubmitNativeOpenAt {
                origin_shard,
                path,
                flags,
                mode,
                reply,
            },
            Self::Connect {
                socket,
                addr,
                addr_len,
                reply,
            } => LocalCommand::SubmitNativeConnect {
                origin_shard,
                socket,
                addr,
                addr_len,
                reply,
            },
            Self::Accept { fd, reply } => LocalCommand::SubmitNativeAccept {
                origin_shard,
                fd,
                reply,
            },
        }
    }

    fn fail_closed(self) {
        match self {
            Self::Read { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound read command channel closed",
                )));
            }
            Self::ReadOwned { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound read command channel closed",
                )));
            }
            Self::Write { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound write command channel closed",
                )));
            }
            Self::RecvOwned { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound recv command channel closed",
                )));
            }
            Self::SendOwned { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound send command channel closed",
                )));
            }
            Self::SendBatchOwned { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound send batch command channel closed",
                )));
            }
            Self::RecvMultishot { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound recv multishot command channel closed",
                )));
            }
            Self::Fsync { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound fsync command channel closed",
                )));
            }
            Self::OpenAt { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound open command channel closed",
                )));
            }
            Self::Connect { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound connect command channel closed",
                )));
            }
            Self::Accept { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound accept command channel closed",
                )));
            }
        }
    }
}

enum Command {
    Spawn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    InjectRawMessage {
        from: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeAny {
        op: NativeAnyCommand,
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
    fn submit_native_read_owned(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        offset: u64,
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
                let _ = driver.submit_native_read_owned(fd, offset, buf, reply);
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
    fn submit_native_send_batch(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<Vec<u8>>)>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring send batch violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring send batch requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_send_batch(fd, bufs, window, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_recv_multishot(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
        reply: oneshot::Sender<std::io::Result<UringRecvMultishotSegments>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring recv multishot violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring recv multishot requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_recv_multishot(
                    fd,
                    buffer_len,
                    buffer_count,
                    bytes_target,
                    reply,
                );
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

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_openat(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        path: CString,
        flags: i32,
        mode: libc::mode_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring open violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring open requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_openat(path, flags, mode, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_connect(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        socket: OwnedFd,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring connect violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring connect requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_connect(socket, addr, addr_len, reply);
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_accept(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<(OwnedFd, SocketAddr)>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring accept violated ring affinity",
            )));
            return;
        }

        match self {
            Self::Queue => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native io_uring accept requires io_uring backend",
                )));
            }
            Self::IoUring(driver) => {
                let _ = driver.submit_native_accept(fd, reply);
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
    OpenAt {
        path: CString,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    Connect {
        socket: OwnedFd,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    },
    Accept {
        addr: Box<libc::sockaddr_storage>,
        addr_len: Box<libc::socklen_t>,
        reply: oneshot::Sender<std::io::Result<(OwnedFd, SocketAddr)>>,
    },
    Read {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    },
    ReadOwned {
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
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
    RecvMulti {
        pool_key: NativeRecvPoolKey,
        buffer_len: usize,
        bytes_target: usize,
        bytes_collected: usize,
        cancel_issued: bool,
        consumed_bids: Vec<u16>,
        segments: Vec<UringRecvSegment>,
        reply: oneshot::Sender<std::io::Result<UringRecvMultishotSegments>>,
    },
    Fsync {
        reply: oneshot::Sender<std::io::Result<()>>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NativeRecvPoolKey {
    fd: RawFd,
    buffer_len: usize,
    buffer_count: u16,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeRecvPool {
    buf_group: u16,
    storage: Box<[u8]>,
    registered: bool,
    in_use: bool,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeSendBatch {
    fd: RawFd,
    bufs: Vec<Vec<u8>>,
    positions: Vec<usize>,
    pending: VecDeque<usize>,
    in_flight: usize,
    window: usize,
    total_sent: usize,
    failure: Option<std::io::Error>,
    reply: Option<oneshot::Sender<std::io::Result<(usize, Vec<Vec<u8>>)>>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
struct NativeSendBatchPart {
    batch_index: usize,
    buf_index: usize,
    offset: usize,
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
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    native_send_batches: Slab<NativeSendBatch>,
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    native_send_parts: Slab<NativeSendBatchPart>,
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    native_recv_pools: HashMap<NativeRecvPoolKey, NativeRecvPool>,
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    next_buf_group: u16,
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
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            native_send_batches: Slab::new(),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            native_send_parts: Slab::new(),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            native_recv_pools: HashMap::new(),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            next_buf_group: 1,
            pending_submit: 0,
            submit_batch_limit: IOURING_SUBMIT_BATCH,
            payload_queue_capacity: payload_queue_capacity.max(1),
            stats,
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn on_native_submit(&self) {
        self.stats
            .pending_native_ops
            .fetch_add(1, Ordering::Relaxed);
        self.stats.increment_pending_native_depth(self.shard_id);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn on_native_complete_many(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.stats
            .pending_native_ops
            .fetch_sub(count as u64, Ordering::Relaxed);
        self.stats
            .decrement_pending_native_depth(self.shard_id, count);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn on_native_complete(&self) {
        self.on_native_complete_many(1);
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
    fn submit_native_openat(
        &mut self,
        path: CString,
        flags: i32,
        mode: libc::mode_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::OpenAt { path, reply });
        self.on_native_submit();

        let path_ptr = match self.native_ops.get(native_index) {
            Some(NativeIoOp::OpenAt { path, .. }) => path.as_ptr(),
            _ => unreachable!("native open op kind mismatch"),
        };

        let entry = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path_ptr)
            .flags(flags)
            .mode(mode)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_connect(
        &mut self,
        socket: OwnedFd,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        reply: oneshot::Sender<std::io::Result<OwnedFd>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Connect {
            socket,
            addr,
            addr_len,
            reply,
        });
        self.on_native_submit();

        let (fd, addr_ptr, addr_len) = match self.native_ops.get(native_index) {
            Some(NativeIoOp::Connect {
                socket,
                addr,
                addr_len,
                ..
            }) => (
                socket.as_raw_fd(),
                addr.as_ref() as *const libc::sockaddr_storage,
                *addr_len,
            ),
            _ => unreachable!("native connect op kind mismatch"),
        };

        let entry = opcode::Connect::new(types::Fd(fd), addr_ptr.cast(), addr_len)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_accept(
        &mut self,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<(OwnedFd, SocketAddr)>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Accept {
            addr: Box::new(unsafe { std::mem::zeroed::<libc::sockaddr_storage>() }),
            addr_len: Box::new(std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t),
            reply,
        });
        self.on_native_submit();

        let (addr_ptr, addr_len_ptr) = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Accept { addr, addr_len, .. }) => (
                addr.as_mut() as *mut libc::sockaddr_storage as *mut libc::sockaddr,
                addr_len.as_mut() as *mut libc::socklen_t,
            ),
            _ => unreachable!("native accept op kind mismatch"),
        };

        let entry = opcode::Accept::new(types::Fd(fd), addr_ptr, addr_len_ptr)
            .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
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
        self.on_native_submit();

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
    fn submit_native_read_owned(
        &mut self,
        fd: RawFd,
        offset: u64,
        buf: Vec<u8>,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    ) -> Result<(), SendError> {
        let Ok(len_u32) = u32::try_from(buf.len()) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native read length exceeds u32::MAX",
            )));
            return Ok(());
        };

        let native_index = self.native_ops.insert(NativeIoOp::ReadOwned { buf, reply });
        self.on_native_submit();

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::ReadOwned { buf, .. }) => buf.as_mut_ptr(),
            _ => unreachable!("native read-owned op kind mismatch"),
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
        self.on_native_submit();

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
        self.on_native_submit();

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
        self.on_native_submit();

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
    fn submit_native_send_batch(
        &mut self,
        fd: RawFd,
        bufs: Vec<Vec<u8>>,
        window: usize,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<Vec<u8>>)>>,
    ) -> Result<(), SendError> {
        if bufs.is_empty() {
            let _ = reply.send(Ok((0, bufs)));
            return Ok(());
        }

        let mut pending = VecDeque::with_capacity(bufs.len());
        pending.extend(0..bufs.len());
        let positions = vec![0usize; bufs.len()];
        let batch_index = self.native_send_batches.insert(NativeSendBatch {
            fd,
            bufs,
            positions,
            pending,
            in_flight: 0,
            window: window.max(1),
            total_sent: 0,
            failure: None,
            reply: Some(reply),
        });
        self.on_native_submit();

        if self.submit_more_send_batch_parts(batch_index).is_err() {
            self.mark_send_batch_failed(
                batch_index,
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native io_uring send batch submit failed",
                ),
            );
            self.maybe_finish_send_batch(batch_index);
            return Err(SendError::Closed);
        }
        Ok(())
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_recv_multishot(
        &mut self,
        fd: RawFd,
        buffer_len: usize,
        buffer_count: u16,
        bytes_target: usize,
        reply: oneshot::Sender<std::io::Result<UringRecvMultishotSegments>>,
    ) -> Result<(), SendError> {
        if buffer_len == 0 || buffer_count == 0 || bytes_target == 0 {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "multishot recv requires non-zero buffer_len, buffer_count, and bytes_target",
            )));
            return Ok(());
        }
        let Ok(buffer_len_i32) = i32::try_from(buffer_len) else {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "multishot recv buffer_len exceeds i32::MAX",
            )));
            return Ok(());
        };
        let total_len = buffer_len.saturating_mul(usize::from(buffer_count));
        if total_len == 0 {
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "multishot recv total buffer size overflowed",
            )));
            return Ok(());
        }

        let pool_key = NativeRecvPoolKey {
            fd,
            buffer_len,
            buffer_count,
        };
        if !self.native_recv_pools.contains_key(&pool_key) {
            let bgid = self.next_buffer_group();
            self.native_recv_pools.insert(
                pool_key,
                NativeRecvPool {
                    buf_group: bgid,
                    storage: vec![0; total_len].into_boxed_slice(),
                    registered: false,
                    in_use: false,
                },
            );
        }

        let (buf_group, storage_ptr, needs_register) = {
            let pool = self
                .native_recv_pools
                .get_mut(&pool_key)
                .expect("native recv pool must exist");
            if pool.in_use {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "multishot recv pool already in use for fd",
                )));
                return Ok(());
            }
            pool.in_use = true;
            (pool.buf_group, pool.storage.as_mut_ptr(), !pool.registered)
        };

        let native_index = self.native_ops.insert(NativeIoOp::RecvMulti {
            pool_key,
            buffer_len,
            bytes_target,
            bytes_collected: 0,
            cancel_issued: false,
            consumed_bids: Vec::new(),
            segments: Vec::new(),
            reply,
        });
        self.on_native_submit();

        if needs_register {
            let provide_entry = opcode::ProvideBuffers::new(
                storage_ptr,
                buffer_len_i32,
                buffer_count,
                buf_group,
                0,
            )
            .build()
            .user_data(native_housekeeping_to_userdata(native_index));
            if self.push_entry(provide_entry).is_err() {
                self.mark_recv_pool_free(pool_key);
                self.fail_native_op(native_index);
                return Err(SendError::Closed);
            }
            if self.mark_submission_pending().is_err() {
                self.mark_recv_pool_free(pool_key);
                self.fail_native_op(native_index);
                return Err(SendError::Closed);
            }
            if let Some(pool) = self.native_recv_pools.get_mut(&pool_key) {
                pool.registered = true;
            }
        }

        let recv_entry = opcode::RecvMulti::new(types::Fd(fd), buf_group)
            .build()
            .user_data(native_to_userdata(native_index));
        if self.push_entry(recv_entry).is_err() {
            self.mark_recv_pool_free(pool_key);
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn next_buffer_group(&mut self) -> u16 {
        let mut bgid = self.next_buf_group;
        if bgid == 0 {
            bgid = 1;
        }
        self.next_buf_group = bgid.wrapping_add(1);
        if self.next_buf_group == 0 {
            self.next_buf_group = 1;
        }
        bgid
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_remove_buffers(&mut self, nbufs: u16, bgid: u16) -> Result<(), SendError> {
        if nbufs == 0 {
            return Ok(());
        }
        let entry = opcode::RemoveBuffers::new(nbufs, bgid)
            .build()
            .user_data(native_housekeeping_to_userdata(0));
        self.push_entry(entry)?;
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn reprovide_multishot_buffers(&mut self, key: NativeRecvPoolKey, bids: &[u16]) {
        if bids.is_empty() {
            return;
        }
        let (buffer_len_i32, storage_ptr, buf_group) = {
            let Some(pool) = self.native_recv_pools.get(&key) else {
                return;
            };
            let Ok(buffer_len_i32) = i32::try_from(key.buffer_len) else {
                if let Some(pool) = self.native_recv_pools.get_mut(&key) {
                    pool.registered = false;
                }
                return;
            };
            (
                buffer_len_i32,
                pool.storage.as_ptr() as *mut u8,
                pool.buf_group,
            )
        };
        let mut valid_bids = bids
            .iter()
            .copied()
            .filter(|bid| usize::from(*bid) < usize::from(key.buffer_count))
            .collect::<Vec<_>>();
        if valid_bids.is_empty() {
            return;
        }
        valid_bids.sort_unstable();
        valid_bids.dedup();

        let mut runs = Vec::new();
        let mut run_start = valid_bids[0];
        let mut run_len: u16 = 1;
        for &bid in valid_bids.iter().skip(1) {
            let expected = run_start.saturating_add(run_len);
            if bid == expected && run_len < u16::MAX {
                run_len = run_len.saturating_add(1);
            } else {
                runs.push((run_start, run_len));
                run_start = bid;
                run_len = 1;
            }
        }
        runs.push((run_start, run_len));

        let mut had_error = false;
        for (start_bid, nbufs) in runs {
            let offset = usize::from(start_bid).saturating_mul(key.buffer_len);
            let ptr = storage_ptr.wrapping_add(offset);
            let entry =
                opcode::ProvideBuffers::new(ptr, buffer_len_i32, nbufs, buf_group, start_bid)
                    .build()
                    .user_data(native_housekeeping_to_userdata(0));
            if self.push_entry(entry).is_err() || self.mark_submission_pending().is_err() {
                had_error = true;
                break;
            }
        }
        if had_error {
            if let Some(pool) = self.native_recv_pools.get_mut(&key) {
                pool.registered = false;
            }
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn mark_recv_pool_free(&mut self, key: NativeRecvPoolKey) {
        if let Some(pool) = self.native_recv_pools.get_mut(&key) {
            pool.in_use = false;
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn take_recv_pool_storage_compact(
        &self,
        key: NativeRecvPoolKey,
        segments: &mut Vec<UringRecvSegment>,
    ) -> std::io::Result<Vec<u8>> {
        let Some(pool) = self.native_recv_pools.get(&key) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "multishot recv buffer pool missing",
            ));
        };
        if segments.is_empty() {
            return Ok(Vec::new());
        }

        let total_touched = segments.iter().map(|seg| seg.len).sum();
        let mut compacted = Vec::with_capacity(total_touched);
        let mut rewritten = Vec::with_capacity(segments.len());
        let mut next_offset = 0usize;

        for seg in segments.iter().copied() {
            let end = seg.offset.saturating_add(seg.len).min(pool.storage.len());
            if seg.offset >= end {
                continue;
            }
            compacted.extend_from_slice(&pool.storage[seg.offset..end]);
            let copied = end - seg.offset;
            rewritten.push(UringRecvSegment {
                offset: next_offset,
                len: copied,
            });
            next_offset = next_offset.saturating_add(copied);
        }

        *segments = rewritten;
        Ok(compacted)
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_async_cancel(&mut self, user_data: u64, index: usize) -> Result<(), SendError> {
        let entry = opcode::AsyncCancel::new(user_data)
            .build()
            .user_data(native_housekeeping_to_userdata(index));
        self.push_entry(entry)?;
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_fsync(
        &mut self,
        fd: RawFd,
        reply: oneshot::Sender<std::io::Result<()>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Fsync { reply });
        self.on_native_submit();

        let entry = opcode::Fsync::new(types::Fd(fd))
            .build()
            .user_data(native_to_userdata(native_index));

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
            return Err(SendError::Closed);
        }
        self.mark_submission_pending()
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_more_send_batch_parts(&mut self, batch_index: usize) -> Result<(), SendError> {
        loop {
            let submit = {
                let Some(batch) = self.native_send_batches.get_mut(batch_index) else {
                    return Ok(());
                };
                if batch.failure.is_some() || batch.in_flight >= batch.window {
                    return Ok(());
                }
                let Some(buf_index) = batch.pending.pop_front() else {
                    return Ok(());
                };
                let offset = batch.positions[buf_index];
                let len = batch.bufs[buf_index].len().saturating_sub(offset);
                if len == 0 {
                    continue;
                }
                (batch.fd, buf_index, offset, len)
            };

            let (fd, buf_index, offset, len) = submit;
            let len_u32 = match u32::try_from(len) {
                Ok(v) => v,
                Err(_) => {
                    self.mark_send_batch_failed(
                        batch_index,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "native send batch chunk exceeds u32::MAX",
                        ),
                    );
                    return Ok(());
                }
            };
            let part_index = self.native_send_parts.insert(NativeSendBatchPart {
                batch_index,
                buf_index,
                offset,
            });
            let buf_ptr = match self.native_send_batches.get(batch_index) {
                Some(batch) => batch.bufs[buf_index].as_ptr().wrapping_add(offset),
                None => {
                    self.native_send_parts.remove(part_index);
                    return Ok(());
                }
            };
            let entry = opcode::Send::new(types::Fd(fd), buf_ptr, len_u32)
                .build()
                .user_data(native_batch_part_to_userdata(part_index));
            if self.push_entry(entry).is_err() {
                self.native_send_parts.remove(part_index);
                return Err(SendError::Closed);
            }
            if let Some(batch) = self.native_send_batches.get_mut(batch_index) {
                batch.in_flight = batch.in_flight.saturating_add(1);
            }
            self.mark_submission_pending()?;
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn mark_send_batch_failed(&mut self, batch_index: usize, err: std::io::Error) {
        if let Some(batch) = self.native_send_batches.get_mut(batch_index) {
            if batch.failure.is_none() {
                batch.failure = Some(err);
            }
            batch.pending.clear();
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn maybe_finish_send_batch(&mut self, batch_index: usize) {
        let should_finish = match self.native_send_batches.get(batch_index) {
            Some(batch) => {
                if batch.failure.is_some() {
                    batch.in_flight == 0
                } else {
                    batch.in_flight == 0 && batch.pending.is_empty()
                }
            }
            None => false,
        };
        if !should_finish {
            return;
        }

        self.on_native_complete();
        let batch = self.native_send_batches.remove(batch_index);
        if let Some(reply) = batch.reply {
            let outcome = if let Some(err) = batch.failure {
                Err(err)
            } else {
                Ok((batch.total_sent, batch.bufs))
            };
            let _ = reply.send(outcome);
        }
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn complete_native_send_batch_part(&mut self, part_index: usize, result: i32) {
        let Some(part) = self.native_send_parts.try_remove(part_index) else {
            return;
        };
        let Some(batch) = self.native_send_batches.get_mut(part.batch_index) else {
            return;
        };
        if batch.in_flight > 0 {
            batch.in_flight -= 1;
        }
        if batch.failure.is_some() {
            self.maybe_finish_send_batch(part.batch_index);
            return;
        }

        let total_len = batch.bufs[part.buf_index].len();
        let remaining = total_len.saturating_sub(part.offset);
        if result < 0 {
            batch.failure = Some(std::io::Error::from_raw_os_error(-result));
            batch.pending.clear();
            self.maybe_finish_send_batch(part.batch_index);
            return;
        }

        let wrote = (result as usize).min(remaining);
        if wrote == 0 && remaining > 0 {
            batch.failure = Some(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "native io_uring send batch wrote zero bytes",
            ));
            batch.pending.clear();
            self.maybe_finish_send_batch(part.batch_index);
            return;
        }

        batch.positions[part.buf_index] = part.offset.saturating_add(wrote);
        batch.total_sent = batch.total_sent.saturating_add(wrote);
        if batch.positions[part.buf_index] < total_len {
            // Keep making forward progress on a partially sent frame.
            batch.pending.push_front(part.buf_index);
        }
        if self.submit_more_send_batch_parts(part.batch_index).is_err() {
            self.mark_send_batch_failed(
                part.batch_index,
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native io_uring send batch submit failed",
                ),
            );
        }
        self.maybe_finish_send_batch(part.batch_index);
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
            self.on_native_complete();
            let op = self.native_ops.remove(index);
            match op {
                NativeIoOp::OpenAt { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring open failed",
                    )));
                }
                NativeIoOp::Connect { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring connect failed",
                    )));
                }
                NativeIoOp::Accept { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring accept failed",
                    )));
                }
                NativeIoOp::Read { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring read failed",
                    )));
                }
                NativeIoOp::ReadOwned { reply, .. } => {
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
                NativeIoOp::RecvMulti {
                    reply,
                    pool_key,
                    consumed_bids,
                    ..
                } => {
                    self.reprovide_multishot_buffers(pool_key, &consumed_bids);
                    self.mark_recv_pool_free(pool_key);
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv multishot failed",
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
        self.on_native_complete_many(ops.len());
        for (_, op) in ops {
            match op {
                NativeIoOp::OpenAt { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring open canceled",
                    )));
                }
                NativeIoOp::Connect { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring connect canceled",
                    )));
                }
                NativeIoOp::Accept { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring accept canceled",
                    )));
                }
                NativeIoOp::Read { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring read canceled",
                    )));
                }
                NativeIoOp::ReadOwned { reply, .. } => {
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
                NativeIoOp::RecvMulti {
                    reply,
                    pool_key,
                    consumed_bids,
                    ..
                } => {
                    self.reprovide_multishot_buffers(pool_key, &consumed_bids);
                    self.mark_recv_pool_free(pool_key);
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv multishot canceled",
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
        let batches = std::mem::take(&mut self.native_send_batches);
        self.on_native_complete_many(batches.len());
        for (_, mut batch) in batches {
            if let Some(reply) = batch.reply.take() {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native io_uring send batch canceled",
                )));
            }
        }
        self.native_send_parts.clear();
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

        let pending = self.pending_submit;
        let mut submitted = false;
        let mut saw_transient = false;
        for _ in 0..4 {
            match self.ring.submit() {
                Ok(_) => {
                    submitted = true;
                    break;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::Interrupted
                        || matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EBUSY)) =>
                {
                    saw_transient = true;
                    thread::yield_now();
                }
                Err(err) => {
                    saw_transient = false;
                    let _ = err;
                    break;
                }
            }
        }

        if submitted {
            self.pending_submit = 0;
            return Ok(());
        }

        if saw_transient {
            // Keep pending submissions queued for a later retry.
            self.pending_submit = pending;
            return Ok(());
        }

        self.pending_submit = 0;
        if !submitted {
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

                waiter_completions.push((cqe.user_data(), cqe.result(), cqe.flags()));
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
        for (user_data, result, _flags) in waiter_completions {
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            if native_housekeeping_from_userdata(user_data).is_some() {
                continue;
            }

            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            if let Some(part_index) = native_batch_part_from_userdata(user_data) {
                self.complete_native_send_batch_part(part_index, result);
                continue;
            }

            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            if let Some(native_index) = native_from_userdata(user_data) {
                self.complete_native_op(native_index, result, _flags);
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
    fn complete_native_op(&mut self, index: usize, result: i32, flags: u32) {
        if !self.native_ops.contains(index) {
            return;
        }

        enum MultiOutcome {
            Continue,
            IssueCancel,
            Finish(
                Result<Vec<UringRecvSegment>, std::io::Error>,
                NativeRecvPoolKey,
                Vec<u16>,
            ),
        }

        let mut multi_outcome = None::<MultiOutcome>;
        if let Some(NativeIoOp::RecvMulti {
            pool_key,
            buffer_len,
            bytes_target,
            bytes_collected,
            cancel_issued,
            consumed_bids,
            segments,
            ..
        }) = self.native_ops.get_mut(index)
        {
            if result < 0 {
                let err = std::io::Error::from_raw_os_error(-result);
                if *cancel_issued && err.raw_os_error() == Some(libc::ECANCELED) {
                    let bids = std::mem::take(consumed_bids);
                    multi_outcome = Some(MultiOutcome::Finish(
                        Ok(std::mem::take(segments)),
                        *pool_key,
                        bids,
                    ));
                } else {
                    let bids = std::mem::take(consumed_bids);
                    multi_outcome = Some(MultiOutcome::Finish(Err(err), *pool_key, bids));
                }
            } else {
                let read_len = result as usize;
                if read_len > 0 {
                    match io_uring::cqueue::buffer_select(flags) {
                        Some(bid) if usize::from(bid) < usize::from(pool_key.buffer_count) => {
                            let start = usize::from(bid) * *buffer_len;
                            let capped = read_len.min(*buffer_len);
                            if self.native_recv_pools.contains_key(pool_key) {
                                segments.push(UringRecvSegment {
                                    offset: start,
                                    len: capped,
                                });
                                consumed_bids.push(bid);
                                *bytes_collected = bytes_collected.saturating_add(capped);
                            } else {
                                let bids = std::mem::take(consumed_bids);
                                multi_outcome = Some(MultiOutcome::Finish(
                                    Err(std::io::Error::new(
                                        std::io::ErrorKind::NotFound,
                                        "multishot recv buffer pool missing",
                                    )),
                                    *pool_key,
                                    bids,
                                ));
                            }
                        }
                        Some(_) => {
                            let bids = std::mem::take(consumed_bids);
                            multi_outcome = Some(MultiOutcome::Finish(
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "multishot recv completion buffer id out of range",
                                )),
                                *pool_key,
                                bids,
                            ));
                        }
                        None => {
                            let bids = std::mem::take(consumed_bids);
                            multi_outcome = Some(MultiOutcome::Finish(
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "multishot recv completion missing buffer id",
                                )),
                                *pool_key,
                                bids,
                            ));
                        }
                    }
                }

                if multi_outcome.is_none() {
                    let has_more = io_uring::cqueue::more(flags);
                    if *bytes_collected >= *bytes_target && has_more && !*cancel_issued {
                        *cancel_issued = true;
                        multi_outcome = Some(MultiOutcome::IssueCancel);
                    } else if result == 0 || !has_more {
                        let bids = std::mem::take(consumed_bids);
                        multi_outcome = Some(MultiOutcome::Finish(
                            Ok(std::mem::take(segments)),
                            *pool_key,
                            bids,
                        ));
                    } else {
                        multi_outcome = Some(MultiOutcome::Continue);
                    }
                }
            }
        }

        if let Some(outcome) = multi_outcome {
            match outcome {
                MultiOutcome::Continue => return,
                MultiOutcome::IssueCancel => {
                    if self
                        .submit_async_cancel(native_to_userdata(index), index)
                        .is_ok()
                    {
                        return;
                    }
                    self.fail_native_op(index);
                    return;
                }
                MultiOutcome::Finish(outcome, pool_key, consumed_bids) => {
                    self.on_native_complete();
                    let op = self.native_ops.remove(index);
                    let outcome = match outcome {
                        Ok(mut segments) => {
                            match self.take_recv_pool_storage_compact(pool_key, &mut segments) {
                                Ok(buffer) => Ok(UringRecvMultishotSegments { buffer, segments }),
                                Err(err) => Err(err),
                            }
                        }
                        Err(err) => Err(err),
                    };
                    if let NativeIoOp::RecvMulti { reply, .. } = op {
                        let _ = reply.send(outcome);
                    }
                    self.reprovide_multishot_buffers(pool_key, &consumed_bids);
                    self.mark_recv_pool_free(pool_key);
                    return;
                }
            }
        }

        self.on_native_complete();
        let op = self.native_ops.remove(index);
        match op {
            NativeIoOp::OpenAt { reply, .. } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let file = unsafe { OwnedFd::from_raw_fd(result) };
                let _ = reply.send(Ok(file));
            }
            NativeIoOp::Connect { socket, reply, .. } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok(socket));
            }
            NativeIoOp::Accept {
                addr,
                addr_len,
                reply,
            } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let socket = unsafe { OwnedFd::from_raw_fd(result) };
                let peer = socket_addr_from_storage(addr.as_ref(), *addr_len);
                let _ = reply.send(peer.map(|peer| (socket, peer)));
            }
            NativeIoOp::Read { mut buf, reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let read_len = result as usize;
                buf.truncate(read_len);
                let _ = reply.send(Ok(buf));
            }
            NativeIoOp::ReadOwned { buf, reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok((result as usize, buf)));
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
            NativeIoOp::RecvMulti { .. } => {}
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
        #[cfg(all(feature = "uring-native", target_os = "linux"))]
        {
            let remove = self
                .native_recv_pools
                .iter()
                .filter_map(|(key, pool)| {
                    pool.registered
                        .then_some((key.buffer_count, pool.buf_group))
                })
                .collect::<Vec<_>>();
            for (count, group) in remove {
                let _ = self.submit_remove_buffers(count, group);
            }
            self.native_recv_pools.clear();
            let _ = self.flush_submissions();
        }
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
    if (user_data & NATIVE_OP_USER_BIT) == 0
        || (user_data & NATIVE_HOUSEKEEPING_USER_BIT) != 0
        || (user_data & NATIVE_BATCH_PART_USER_BIT) != 0
    {
        return None;
    }
    usize::try_from((user_data & !NATIVE_OP_USER_BIT).checked_sub(1)?).ok()
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_housekeeping_to_userdata(index: usize) -> u64 {
    NATIVE_OP_USER_BIT | NATIVE_HOUSEKEEPING_USER_BIT | (index as u64 + 1)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_housekeeping_from_userdata(user_data: u64) -> Option<usize> {
    if (user_data & NATIVE_OP_USER_BIT) == 0 || (user_data & NATIVE_HOUSEKEEPING_USER_BIT) == 0 {
        return None;
    }
    usize::try_from(
        (user_data & !(NATIVE_OP_USER_BIT | NATIVE_HOUSEKEEPING_USER_BIT)).checked_sub(1)?,
    )
    .ok()
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_batch_part_to_userdata(index: usize) -> u64 {
    NATIVE_OP_USER_BIT | NATIVE_BATCH_PART_USER_BIT | (index as u64 + 1)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn native_batch_part_from_userdata(user_data: u64) -> Option<usize> {
    if (user_data & NATIVE_OP_USER_BIT) == 0
        || (user_data & NATIVE_BATCH_PART_USER_BIT) == 0
        || (user_data & NATIVE_HOUSEKEEPING_USER_BIT) != 0
    {
        return None;
    }
    usize::try_from(
        (user_data & !(NATIVE_OP_USER_BIT | NATIVE_BATCH_PART_USER_BIT)).checked_sub(1)?,
    )
    .ok()
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
                    stop = handle_command(
                        cmd,
                        shard_id,
                        &spawner,
                        &event_state,
                        &stats,
                        &local_commands,
                    );
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
                        stop = handle_command(
                            cmd,
                            shard_id,
                            &spawner,
                            &event_state,
                            &stats,
                            &local_commands,
                        );
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
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            Command::SubmitNativeAny { op } => op.fail_closed(),
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
    _shard_id: ShardId,
    spawner: &LocalSpawner,
    event_state: &EventState,
    stats: &RuntimeStatsInner,
    _local_commands: &RefCell<VecDeque<LocalCommand>>,
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
        #[cfg(all(feature = "uring-native", target_os = "linux"))]
        Command::SubmitNativeAny { op } => {
            _local_commands
                .borrow_mut()
                .push_back(op.into_local(_shard_id));
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
            LocalCommand::SubmitNativeReadOwned {
                origin_shard,
                fd,
                offset,
                buf,
                reply,
            } => backend.submit_native_read_owned(
                shard_id,
                origin_shard,
                fd,
                offset,
                buf,
                reply,
                stats,
            ),
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
            LocalCommand::SubmitNativeSendBatchOwned {
                origin_shard,
                fd,
                bufs,
                window,
                reply,
            } => backend.submit_native_send_batch(
                shard_id,
                origin_shard,
                fd,
                bufs,
                window,
                reply,
                stats,
            ),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeRecvMultishot {
                origin_shard,
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
            } => backend.submit_native_recv_multishot(
                shard_id,
                origin_shard,
                fd,
                buffer_len,
                buffer_count,
                bytes_target,
                reply,
                stats,
            ),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeFsync {
                origin_shard,
                fd,
                reply,
            } => backend.submit_native_fsync(shard_id, origin_shard, fd, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeOpenAt {
                origin_shard,
                path,
                flags,
                mode,
                reply,
            } => backend.submit_native_openat(
                shard_id,
                origin_shard,
                path,
                flags,
                mode,
                reply,
                stats,
            ),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeConnect {
                origin_shard,
                socket,
                addr,
                addr_len,
                reply,
            } => backend.submit_native_connect(
                shard_id,
                origin_shard,
                socket,
                addr,
                addr_len,
                reply,
                stats,
            ),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeAccept {
                origin_shard,
                fd,
                reply,
            } => backend.submit_native_accept(shard_id, origin_shard, fd, reply, stats),
        }
    }
}
