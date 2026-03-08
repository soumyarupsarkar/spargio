//! Sharded async runtime API centered around `msg_ring`-style signaling.
#![cfg_attr(not(feature = "uring-native"), deny(missing_docs))]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use crossbeam_queue::SegQueue;
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::future::join_all;
use futures::future::{Either, select};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::task::AtomicWaker;
use futures::task::LocalSpawnExt;
use std::cell::RefCell;
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
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};
#[cfg(target_os = "linux")]
use slab::Slab;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::os::fd::{FromRawFd, OwnedFd};

/// Runtime shard identifier.
///
/// A shard is one runtime worker thread and its local queues/ring state.
pub type ShardId = u16;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub type NativeOpId = u64;
const EXTERNAL_SENDER: ShardId = ShardId::MAX;
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "macros")]
pub use spargio_macros::main;

#[doc(hidden)]
pub mod __private {
    use core::future::Future;

    pub fn block_on<F>(fut: F) -> F::Output
    where
        F: Future,
    {
        futures::executor::block_on(fut)
    }
}

#[cfg(target_os = "linux")]
const MSG_RING_CQE_FLAG: u32 = 1 << 8;
#[cfg(target_os = "linux")]
const IOURING_SUBMIT_BATCH: usize = 64;
#[cfg(target_os = "linux")]
const DOORBELL_TAG: u16 = u16::MAX;
const HOT_MSG_TAG_COUNT: usize = 65_536;
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

#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeLocalBufReplySlot {
    result: Mutex<Option<std::io::Result<(usize, Vec<u8>)>>>,
    waker: AtomicWaker,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl NativeLocalBufReplySlot {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            waker: AtomicWaker::new(),
        }
    }

    fn complete(&self, out: std::io::Result<(usize, Vec<u8>)>) {
        *self
            .result
            .lock()
            .expect("native local buf reply lock poisoned") = Some(out);
        self.waker.wake();
    }

    fn take(&self) -> Option<std::io::Result<(usize, Vec<u8>)>> {
        self.result
            .lock()
            .expect("native local buf reply lock poisoned")
            .take()
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeLocalBufReplyFuture {
    slot: Arc<NativeLocalBufReplySlot>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Future for NativeLocalBufReplyFuture {
    type Output = std::io::Result<(usize, Vec<u8>)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(out) = self.slot.take() {
            return Poll::Ready(out);
        }
        self.slot.waker.register(cx.waker());
        if let Some(out) = self.slot.take() {
            Poll::Ready(out)
        } else {
            Poll::Pending
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum NativeBufReply {
    Oneshot(oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>),
    Local(Arc<NativeLocalBufReplySlot>),
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl NativeBufReply {
    fn oneshot(tx: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>) -> Self {
        Self::Oneshot(tx)
    }

    fn local_pair() -> (Self, NativeLocalBufReplyFuture) {
        let slot = Arc::new(NativeLocalBufReplySlot::new());
        let fut = NativeLocalBufReplyFuture { slot: slot.clone() };
        (Self::Local(slot), fut)
    }

    fn complete(self, out: std::io::Result<(usize, Vec<u8>)>) {
        match self {
            Self::Oneshot(reply) => {
                let _ = reply.send(out);
            }
            Self::Local(slot) => slot.complete(out),
        }
    }
}

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

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn path_to_cstring_for_native_ops(path: &std::path::Path) -> std::io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains interior NUL byte",
        )
    })
}

/// Async boundary channel for passing request/response work across runtime boundaries.
///
/// Use this when you want explicit backpressure, timeout-aware calls, and
/// cancellation-safe response delivery between producers and consumers.
pub mod boundary {
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
    use futures::channel::oneshot;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    const POLL_INTERVAL: Duration = Duration::from_millis(1);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    /// Error type returned by boundary request/response operations.
    pub enum BoundaryError {
        /// The boundary endpoint is closed.
        Closed,
        /// The boundary queue is full.
        Overloaded,
        /// The operation timed out.
        Timeout,
        /// The caller or callee side canceled before completion.
        Canceled,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    /// Snapshot of aggregate boundary error counters.
    pub struct BoundaryStats {
        /// Number of overload rejections.
        pub overloaded: u64,
        /// Number of timeout outcomes.
        pub timed_out: u64,
        /// Number of cancellations.
        pub canceled: u64,
        /// Number of closed-channel outcomes.
        pub closed: u64,
    }

    struct BoundaryStatsInner {
        overloaded: AtomicU64,
        timed_out: AtomicU64,
        canceled: AtomicU64,
        closed: AtomicU64,
    }

    static BOUNDARY_STATS: BoundaryStatsInner = BoundaryStatsInner {
        overloaded: AtomicU64::new(0),
        timed_out: AtomicU64::new(0),
        canceled: AtomicU64::new(0),
        closed: AtomicU64::new(0),
    };

    impl BoundaryStatsInner {
        fn snapshot(&self) -> BoundaryStats {
            BoundaryStats {
                overloaded: self.overloaded.load(Ordering::Relaxed),
                timed_out: self.timed_out.load(Ordering::Relaxed),
                canceled: self.canceled.load(Ordering::Relaxed),
                closed: self.closed.load(Ordering::Relaxed),
            }
        }

        fn clear(&self) {
            self.overloaded.store(0, Ordering::Relaxed);
            self.timed_out.store(0, Ordering::Relaxed);
            self.canceled.store(0, Ordering::Relaxed);
            self.closed.store(0, Ordering::Relaxed);
        }
    }

    /// Returns the current process-wide boundary stats snapshot.
    pub fn stats_snapshot() -> BoundaryStats {
        BOUNDARY_STATS.snapshot()
    }

    /// Resets boundary stats counters.
    ///
    /// Intended for tests/benchmarks.
    pub fn reset_stats_for_tests() {
        BOUNDARY_STATS.clear();
    }

    struct BoundaryEnvelope<Request, Response> {
        request: Request,
        deadline: Option<Instant>,
        reply: oneshot::Sender<Result<Response, BoundaryError>>,
    }

    /// Server-side request envelope that carries the request payload and reply path.
    pub struct BoundaryRequest<Request, Response> {
        request: Request,
        deadline: Option<Instant>,
        reply: Option<oneshot::Sender<Result<Response, BoundaryError>>>,
    }

    impl<Request, Response> BoundaryRequest<Request, Response> {
        /// Returns a shared reference to the request payload.
        pub fn request(&self) -> &Request {
            &self.request
        }

        /// Returns the call deadline, if one was set by the client.
        pub fn deadline(&self) -> Option<Instant> {
            self.deadline
        }

        /// Consumes this envelope and returns the request payload.
        pub fn into_request(self) -> Request {
            self.request
        }

        /// Sends the response back to the caller.
        ///
        /// Returns an error when the request already timed out or was canceled.
        pub fn respond(mut self, response: Response) -> Result<(), BoundaryError> {
            if let Some(deadline) = self.deadline {
                if Instant::now() > deadline {
                    if let Some(reply) = self.reply.take() {
                        let _ = reply.send(Err(BoundaryError::Timeout));
                    }
                    BOUNDARY_STATS.timed_out.fetch_add(1, Ordering::Relaxed);
                    return Err(BoundaryError::Timeout);
                }
            }

            let Some(reply) = self.reply.take() else {
                BOUNDARY_STATS.canceled.fetch_add(1, Ordering::Relaxed);
                return Err(BoundaryError::Canceled);
            };

            reply.send(Ok(response)).map_err(|_| {
                BOUNDARY_STATS.canceled.fetch_add(1, Ordering::Relaxed);
                BoundaryError::Canceled
            })
        }
    }

    #[derive(Clone)]
    /// Client side of a boundary channel.
    pub struct BoundaryClient<Request, Response> {
        tx: Sender<BoundaryEnvelope<Request, Response>>,
    }

    /// Server side of a boundary channel.
    pub struct BoundaryServer<Request, Response> {
        rx: Receiver<BoundaryEnvelope<Request, Response>>,
    }

    /// Ticket/future returned by `BoundaryClient` calls.
    pub struct BoundaryTicket<Response> {
        rx: Option<oneshot::Receiver<Result<Response, BoundaryError>>>,
    }

    impl<Response> BoundaryTicket<Response> {
        /// Waits for the response with an additional timeout.
        pub async fn wait_timeout(self, timeout: Duration) -> Result<Response, BoundaryError> {
            match super::timeout(timeout, self).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    BOUNDARY_STATS.timed_out.fetch_add(1, Ordering::Relaxed);
                    Err(BoundaryError::Timeout)
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
                    BOUNDARY_STATS.canceled.fetch_add(1, Ordering::Relaxed);
                    Poll::Ready(Err(BoundaryError::Canceled))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl<Request, Response> BoundaryClient<Request, Response> {
        /// Enqueues a request and returns a response ticket.
        pub async fn call(
            &self,
            request: Request,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            self.enqueue_async(request, None).await
        }

        /// Enqueues a request with a per-call timeout deadline.
        pub async fn call_with_timeout(
            &self,
            request: Request,
            timeout: Duration,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            self.enqueue_async(request, Some(Instant::now() + timeout))
                .await
        }

        /// Attempts to enqueue immediately without waiting for queue space.
        pub fn try_call(
            &self,
            request: Request,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            let (reply_tx, reply_rx) = oneshot::channel();
            let msg = BoundaryEnvelope {
                request,
                deadline: None,
                reply: reply_tx,
            };

            match self.tx.try_send(msg) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    BOUNDARY_STATS.overloaded.fetch_add(1, Ordering::Relaxed);
                    return Err(BoundaryError::Overloaded);
                }
                Err(TrySendError::Disconnected(_)) => {
                    BOUNDARY_STATS.closed.fetch_add(1, Ordering::Relaxed);
                    return Err(BoundaryError::Closed);
                }
            }
            Ok(BoundaryTicket { rx: Some(reply_rx) })
        }

        async fn enqueue_async(
            &self,
            request: Request,
            deadline: Option<Instant>,
        ) -> Result<BoundaryTicket<Response>, BoundaryError> {
            let (reply_tx, reply_rx) = oneshot::channel();
            let mut msg = Some(BoundaryEnvelope {
                request,
                deadline,
                reply: reply_tx,
            });

            loop {
                if let Some(deadline) = deadline {
                    if Instant::now() >= deadline {
                        BOUNDARY_STATS.timed_out.fetch_add(1, Ordering::Relaxed);
                        return Err(BoundaryError::Timeout);
                    }
                }

                let next = msg.take().expect("boundary request envelope missing");
                match self.tx.try_send(next) {
                    Ok(()) => return Ok(BoundaryTicket { rx: Some(reply_rx) }),
                    Err(TrySendError::Full(returned)) => {
                        BOUNDARY_STATS.overloaded.fetch_add(1, Ordering::Relaxed);
                        msg = Some(returned);
                        super::sleep(POLL_INTERVAL).await;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        BOUNDARY_STATS.closed.fetch_add(1, Ordering::Relaxed);
                        return Err(BoundaryError::Closed);
                    }
                }
            }
        }
    }

    impl<Request, Response> BoundaryServer<Request, Response> {
        /// Receives the next request, waiting until one is available.
        pub async fn recv(&self) -> Result<BoundaryRequest<Request, Response>, BoundaryError> {
            loop {
                match self.rx.try_recv() {
                    Ok(msg) => return Ok(boundary_request(msg)),
                    Err(TryRecvError::Empty) => super::sleep(POLL_INTERVAL).await,
                    Err(TryRecvError::Disconnected) => {
                        BOUNDARY_STATS.closed.fetch_add(1, Ordering::Relaxed);
                        return Err(BoundaryError::Closed);
                    }
                }
            }
        }

        /// Receives the next request, returning timeout if none arrives in time.
        pub async fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Result<BoundaryRequest<Request, Response>, BoundaryError> {
            let deadline = Instant::now() + timeout;
            loop {
                match self.rx.try_recv() {
                    Ok(msg) => return Ok(boundary_request(msg)),
                    Err(TryRecvError::Empty) => {
                        let now = Instant::now();
                        if now >= deadline {
                            BOUNDARY_STATS.timed_out.fetch_add(1, Ordering::Relaxed);
                            return Err(BoundaryError::Timeout);
                        }
                        let sleep_for = deadline.saturating_duration_since(now).min(POLL_INTERVAL);
                        super::sleep(sleep_for).await;
                    }
                    Err(TryRecvError::Disconnected) => {
                        BOUNDARY_STATS.closed.fetch_add(1, Ordering::Relaxed);
                        return Err(BoundaryError::Closed);
                    }
                }
            }
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

    /// Creates a bounded boundary request/response channel pair.
    ///
    /// `capacity` is clamped to at least `1`.
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
/// Error returned by timeout helpers when the deadline expires.
pub struct TimeoutError;

/// Asynchronously sleeps for at least `duration`.
pub async fn sleep(duration: Duration) {
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(reply_rx) = ShardCtx::current().map(|ctx| ctx.enqueue_native_sleep(duration)) {
        if matches!(reply_rx.await, Ok(Ok(()))) {
            return;
        }
    }

    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

/// Asynchronously sleeps until `deadline`.
pub async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    sleep(deadline.saturating_duration_since(now)).await;
}

/// A resettable sleep future.
pub struct Sleep {
    deadline: Instant,
    fut: Pin<Box<dyn Future<Output = ()> + 'static>>,
    elapsed: bool,
}

impl Sleep {
    /// Creates a sleep that fires after `duration`.
    pub fn new(duration: Duration) -> Self {
        Self::until(Instant::now() + duration)
    }

    /// Creates a sleep that fires at an absolute `deadline`.
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline,
            fut: Box::pin(sleep_until(deadline)),
            elapsed: false,
        }
    }

    /// Returns the current deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Returns `true` once the timer has elapsed.
    pub fn is_elapsed(&self) -> bool {
        self.elapsed || Instant::now() >= self.deadline
    }

    /// Resets this timer to a new absolute `deadline`.
    pub fn reset(&mut self, deadline: Instant) {
        self.deadline = deadline;
        self.elapsed = false;
        self.fut = Box::pin(sleep_until(deadline));
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.elapsed {
            return Poll::Ready(());
        }
        if Instant::now() >= self.deadline {
            self.elapsed = true;
            return Poll::Ready(());
        }
        match self.fut.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.elapsed = true;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Runs `fut` and returns an error if it does not complete within `duration`.
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

/// Runs `fut` and returns an error if it does not complete by `deadline`.
pub async fn timeout_at<F>(deadline: Instant, fut: F) -> Result<F::Output, TimeoutError>
where
    F: Future,
{
    timeout(deadline.saturating_duration_since(Instant::now()), fut).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Policy used by [`Interval`] when ticks are missed.
pub enum MissedTickBehavior {
    /// Deliver missed ticks as quickly as possible to catch up.
    Burst,
    /// Shift schedule to `now + period` after each observed tick.
    Delay,
    /// Skip directly to the next aligned future tick.
    Skip,
}

impl Default for MissedTickBehavior {
    fn default() -> Self {
        Self::Burst
    }
}

#[derive(Debug, Clone)]
/// Repeating timer that yields scheduled tick instants.
pub struct Interval {
    period: Duration,
    next: Instant,
    missed_tick_behavior: MissedTickBehavior,
}

impl Interval {
    /// Returns the interval period.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Returns the missed tick behavior policy.
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.missed_tick_behavior
    }

    /// Sets the missed tick behavior policy.
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed_tick_behavior = behavior;
    }

    /// Waits for the next tick and returns that tick's scheduled instant.
    pub async fn tick(&mut self) -> Instant {
        let scheduled = self.next;
        sleep_until(scheduled).await;
        let now = Instant::now();
        self.next = compute_next_tick(
            self.next,
            self.period,
            self.missed_tick_behavior,
            now.max(scheduled),
        );
        scheduled
    }
}

/// Creates an interval that starts immediately.
pub fn interval(period: Duration) -> Interval {
    interval_at(Instant::now(), period)
}

/// Creates an interval starting at `start`.
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero");
    Interval {
        period,
        next: start,
        missed_tick_behavior: MissedTickBehavior::Burst,
    }
}

fn compute_next_tick(
    scheduled: Instant,
    period: Duration,
    behavior: MissedTickBehavior,
    now: Instant,
) -> Instant {
    match behavior {
        MissedTickBehavior::Burst => scheduled + period,
        MissedTickBehavior::Delay => now + period,
        MissedTickBehavior::Skip => {
            if now <= scheduled {
                return scheduled + period;
            }
            let elapsed = now.saturating_duration_since(scheduled);
            let step = period.as_nanos();
            if step == 0 {
                return now;
            }
            let ticks_missed = elapsed.as_nanos() / step + 1;
            let jump = period
                .checked_mul(ticks_missed.min(u128::from(u32::MAX)) as u32)
                .unwrap_or(Duration::MAX);
            scheduled.checked_add(jump).unwrap_or(now + period)
        }
    }
}

/// Runs a top-level async job on a freshly constructed runtime using default builder settings.
///
/// The provided closure receives a [`RuntimeHandle`] so the job can spawn additional work.
pub async fn run<F, Fut, T>(entry: F) -> Result<T, RuntimeError>
where
    F: FnOnce(RuntimeHandle) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    run_with(Runtime::builder(), entry).await
}

/// Runs a top-level async job on a freshly constructed runtime built from `builder`.
///
/// This is an ergonomic entry helper equivalent to:
/// 1) `builder.build()`
/// 2) `handle.spawn_stealable(...)`
/// 3) waiting for the returned join handle.
pub async fn run_with<F, Fut, T>(builder: RuntimeBuilder, entry: F) -> Result<T, RuntimeError>
where
    F: FnOnce(RuntimeHandle) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut runtime = builder.build()?;
    let handle = runtime.handle();
    let job = entry(handle.clone());
    let join = handle.spawn_stealable(job)?;
    let outcome = join.await.map_err(|_| RuntimeError::Closed);
    runtime.shutdown().await;
    outcome
}

/// Runs a top-level local (`!Send`) async job on a specific shard.
///
/// The `entry` closure is sent to `shard` and invoked there with that shard's
/// [`ShardCtx`], allowing the returned future to be `!Send`.
pub async fn run_local_on<F, Fut, T>(
    builder: RuntimeBuilder,
    shard: ShardId,
    entry: F,
) -> Result<T, RuntimeError>
where
    F: FnOnce(ShardCtx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let mut runtime = builder.build()?;
    let handle = runtime.handle();
    let join = handle.spawn_local_on(shard, entry)?;
    let outcome = join.await.map_err(|_| RuntimeError::Closed);
    runtime.shutdown().await;
    outcome
}

#[derive(Clone, Default)]
/// Cooperative cancellation token shared between tasks.
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    canceled: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}

impl CancellationToken {
    /// Creates a new uncanceled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels this token and wakes all registered waiters.
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

    /// Returns `true` if this token has been canceled.
    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.load(Ordering::SeqCst)
    }

    /// Returns a future that resolves when this token is canceled.
    pub fn cancelled(&self) -> CancellationFuture {
        CancellationFuture {
            token: self.clone(),
        }
    }
}

/// Future returned by [`CancellationToken::cancelled`].
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

/// Group helper that couples task spawning with a shared cancellation token.
pub struct TaskGroup {
    handle: RuntimeHandle,
    token: CancellationToken,
}

impl TaskGroup {
    /// Creates a new task group bound to `handle`.
    pub fn new(handle: RuntimeHandle) -> Self {
        Self {
            handle,
            token: CancellationToken::new(),
        }
    }

    /// Cancels the group token.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Returns a clone of the group cancellation token.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Spawns a task under this group's cancellation token and placement policy.
    ///
    /// The returned join handle yields `Ok(Some(output))` on completion and
    /// `Ok(None)` when canceled by the group token.
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

/// Join handle returned by [`TaskGroup::spawn_with_placement`].
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
/// Runtime event stream item.
pub enum Event {
    /// A raw ring message event.
    RingMsg {
        /// Shard id that originated the message.
        from: ShardId,
        /// Message tag.
        tag: u16,
        /// Message value payload.
        val: u32,
    },
}

/// Trait for strongly-typed cross-shard message encoding/decoding.
pub trait RingMsg: Copy + Send + 'static {
    /// Encodes this message into `(tag, value)` wire form.
    fn encode(self) -> (u16, u32);
    /// Decodes a `(tag, value)` pair into a typed message.
    fn decode(tag: u16, val: u32) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Runtime backend selection.
pub enum BackendKind {
    /// Linux `io_uring` backend.
    IoUring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// Worker idle strategy used when a shard has no immediately runnable work.
pub enum IdleStrategy {
    /// Use backend default behavior.
    ///
    /// On Linux `io_uring` backend this currently means `thread::yield_now()`
    /// in idle scheduler passes.
    #[default]
    BackendDefault,
    /// Yield the current worker timeslice on idle scheduler passes.
    Yield,
    /// Sleep for the provided duration on idle scheduler passes.
    ///
    /// If `duration` is zero, Spargio falls back to `thread::yield_now()`.
    Sleep(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Task placement policy used when spawning.
pub enum TaskPlacement {
    /// Pin execution to a specific shard.
    Pinned(ShardId),
    /// Place on the next shard in round-robin order.
    RoundRobin,
    /// Place by hashing a sticky key to a shard.
    Sticky(u64),
    /// Enqueue as stealable work with round-robin preferred shard.
    Stealable,
    /// Enqueue as stealable work with explicit preferred shard.
    StealablePreferred(ShardId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Internal queue backend used for stealable task inboxes.
pub enum StealableQueueBackend {
    /// Mutex + `VecDeque` backend (default).
    Mutex,
    /// Experimental lock-free queue backend.
    SegQueueExperimental,
}

#[derive(Debug, Clone)]
/// Runtime statistics snapshot.
pub struct RuntimeStats {
    /// Per-shard command queue depth samples.
    pub shard_command_depths: Vec<usize>,
    /// Per-shard in-flight native-op counts.
    pub pending_native_ops_by_shard: Vec<usize>,
    /// Count of native unbound envelope submissions.
    pub native_any_envelope_submitted: u64,
    /// Count of native local fast-path submissions.
    pub native_any_local_fastpath_submitted: u64,
    /// Count of native local direct submissions.
    pub native_any_local_direct_submitted: u64,
    /// Count of pinned spawns requested.
    pub spawn_pinned_submitted: u64,
    /// Count of stealable spawns requested.
    pub spawn_stealable_submitted: u64,
    /// Count of executed stealable tasks.
    pub stealable_executed: u64,
    /// Count of stealable tasks executed on a non-preferred shard.
    pub stealable_stolen: u64,
    /// Count of stealable enqueue backpressure hits.
    pub stealable_backpressure: u64,
    /// Count of stealable tasks executed on preferred/local shard.
    pub stealable_local_hits: u64,
    /// Count of steal attempts.
    pub steal_attempts: u64,
    /// Count of victim queue scans performed while stealing.
    pub steal_scans: u64,
    /// Count of successful steal operations.
    pub steal_success: u64,
    /// Count of steals skipped due to backoff.
    pub steal_skipped_backoff: u64,
    /// Count of steals skipped due to locality gate.
    pub steal_skipped_locality: u64,
    /// Maximum observed consecutive failed steals.
    pub steal_failed_streak_max: u64,
    /// Count of stealable wake notifications sent.
    pub stealable_wake_sent: u64,
    /// Count of coalesced stealable wake notifications.
    pub stealable_wake_coalesced: u64,
    /// Configured victim stride for stealing.
    pub steal_victim_stride: usize,
    /// Configured victim probes per steal cycle.
    pub steal_victim_probe_count: usize,
    /// Configured steal batch size.
    pub steal_batch_size: usize,
    /// Configured locality margin for stealing decisions.
    pub steal_locality_margin: usize,
    /// Configured fail-cost heuristic for stealing decisions.
    pub steal_fail_cost: usize,
    /// Configured minimum backoff ticks after failed steals.
    pub steal_backoff_min: usize,
    /// Configured maximum backoff ticks after failed steals.
    pub steal_backoff_max: usize,
    /// Count of ring messages submitted.
    pub ring_msgs_submitted: u64,
    /// Count of ring messages completed.
    pub ring_msgs_completed: u64,
    /// Count of failed ring message submissions/completions.
    pub ring_msgs_failed: u64,
    /// Count of ring message backpressure rejections.
    pub ring_msgs_backpressure: u64,
    /// Count of native-op affinity routing violations observed.
    pub native_affinity_violations: u64,
    /// Total in-flight native operations.
    pub pending_native_ops: u64,
}

impl RuntimeStats {
    /// Returns the sum of `shard_command_depths`.
    pub fn total_command_depth(&self) -> usize {
        self.shard_command_depths.iter().sum()
    }

    /// Returns the maximum observed command depth among shards.
    pub fn max_command_depth(&self) -> usize {
        self.shard_command_depths.iter().copied().max().unwrap_or(0)
    }

    /// Returns the maximum in-flight native-op count across shards.
    pub fn max_pending_native_ops_by_shard(&self) -> usize {
        self.pending_native_ops_by_shard
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    }

    /// Returns `steal_success / steal_attempts`, or `0.0` if no attempts.
    pub fn steal_success_rate(&self) -> f64 {
        if self.steal_attempts == 0 {
            return 0.0;
        }
        self.steal_success as f64 / self.steal_attempts as f64
    }

    /// Returns `stealable_local_hits / stealable_executed`, or `0.0`.
    pub fn local_hit_ratio(&self) -> f64 {
        if self.stealable_executed == 0 {
            return 0.0;
        }
        self.stealable_local_hits as f64 / self.stealable_executed as f64
    }

    /// Returns `steal_success / steal_scans`, or `0.0`.
    pub fn stolen_per_scan(&self) -> f64 {
        if self.steal_scans == 0 {
            return 0.0;
        }
        self.steal_success as f64 / self.steal_scans as f64
    }
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
/// Builder for constructing a [`Runtime`].
///
/// Defaults are tuned for general-purpose usage. Use the steal-policy and
/// queue settings only when you have benchmark evidence for your workload.
pub struct RuntimeBuilder {
    shards: usize,
    thread_prefix: String,
    thread_affinity: Vec<usize>,
    backend: BackendKind,
    idle_strategy: IdleStrategy,
    ring_entries: u32,
    msg_ring_queue_capacity: usize,
    hot_msg_tags: Vec<u16>,
    coalesced_hot_msg_tags: Vec<u16>,
    hot_counter_wake_threshold: u64,
    stealable_queue_capacity: usize,
    stealable_queue_backend: StealableQueueBackend,
    steal_budget: usize,
    steal_victim_stride: usize,
    steal_victim_probe_count: usize,
    steal_batch_size: usize,
    steal_locality_margin: usize,
    steal_fail_cost: usize,
    steal_backoff_min: usize,
    steal_backoff_max: usize,
    #[cfg(target_os = "linux")]
    io_uring: IoUringBuildConfig,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            shards: std::thread::available_parallelism().map_or(1, usize::from),
            thread_prefix: "spargio-shard".to_owned(),
            thread_affinity: Vec::new(),
            backend: BackendKind::IoUring,
            idle_strategy: IdleStrategy::BackendDefault,
            ring_entries: 256,
            msg_ring_queue_capacity: 4096,
            hot_msg_tags: Vec::new(),
            coalesced_hot_msg_tags: Vec::new(),
            hot_counter_wake_threshold: 1,
            stealable_queue_capacity: 4096,
            stealable_queue_backend: StealableQueueBackend::Mutex,
            steal_budget: 64,
            steal_victim_stride: 1,
            steal_victim_probe_count: 2,
            steal_batch_size: 4,
            steal_locality_margin: 1,
            steal_fail_cost: 1,
            steal_backoff_min: 1,
            steal_backoff_max: 32,
            #[cfg(target_os = "linux")]
            io_uring: IoUringBuildConfig::default(),
        }
    }
}

impl RuntimeBuilder {
    /// Creates a new builder with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the number of runtime shards (worker threads).
    pub fn shards(mut self, count: usize) -> Self {
        self.shards = count;
        self
    }

    /// Sets the prefix used for worker thread names.
    pub fn thread_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.thread_prefix = prefix.into();
        self
    }

    /// Sets an optional CPU affinity list used for worker threads.
    ///
    /// Duplicates are removed and values are sorted.
    pub fn thread_affinity<I>(mut self, cpus: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        let mut cpus = cpus.into_iter().collect::<Vec<_>>();
        cpus.sort_unstable();
        cpus.dedup();
        self.thread_affinity = cpus;
        self
    }

    /// Selects the runtime backend.
    pub fn backend(mut self, backend: BackendKind) -> Self {
        self.backend = backend;
        self
    }

    /// Selects how worker shards idle when no immediate work is available.
    pub fn idle_strategy(mut self, strategy: IdleStrategy) -> Self {
        self.idle_strategy = strategy;
        self
    }

    /// Sets ring SQ/CQ entry count for each shard (minimum `1`).
    pub fn ring_entries(mut self, entries: u32) -> Self {
        self.ring_entries = entries.max(1);
        self
    }

    /// Sets per-shard command queue capacity (minimum `1`).
    pub fn msg_ring_queue_capacity(mut self, capacity: usize) -> Self {
        self.msg_ring_queue_capacity = capacity.max(1);
        self
    }

    /// Marks a message tag as "hot" for fast hot-lane polling.
    pub fn hot_msg_tag(mut self, tag: u16) -> Self {
        self.hot_msg_tags.push(tag);
        self
    }

    /// Marks multiple message tags as "hot".
    pub fn hot_msg_tags<I>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = u16>,
    {
        self.hot_msg_tags.extend(tags);
        self
    }

    /// Marks a tag as coalesced-hot (counts are coalesced before wake).
    pub fn coalesced_hot_msg_tag(mut self, tag: u16) -> Self {
        self.coalesced_hot_msg_tags.push(tag);
        self
    }

    /// Marks multiple tags as coalesced-hot.
    pub fn coalesced_hot_msg_tags<I>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = u16>,
    {
        self.coalesced_hot_msg_tags.extend(tags);
        self
    }

    /// Sets wake threshold for coalesced hot-counter updates (minimum `1`).
    pub fn hot_counter_wake_threshold(mut self, threshold: u64) -> Self {
        self.hot_counter_wake_threshold = threshold.max(1);
        self
    }

    /// Sets stealable task queue capacity per shard (minimum `1`).
    pub fn stealable_queue_capacity(mut self, capacity: usize) -> Self {
        self.stealable_queue_capacity = capacity.max(1);
        self
    }

    /// Selects backend implementation for stealable task queues.
    pub fn stealable_queue_backend(mut self, backend: StealableQueueBackend) -> Self {
        self.stealable_queue_backend = backend;
        self
    }

    /// Sets maximum local steal work budget per scheduler loop (minimum `1`).
    pub fn steal_budget(mut self, budget: usize) -> Self {
        self.steal_budget = budget.max(1);
        self
    }

    /// Sets victim-shard stepping stride for steal probing (minimum `1`).
    pub fn steal_victim_stride(mut self, stride: usize) -> Self {
        self.steal_victim_stride = stride.max(1);
        self
    }

    /// Sets number of victim queues to probe per steal cycle (minimum `1`).
    pub fn steal_victim_probe_count(mut self, probes: usize) -> Self {
        self.steal_victim_probe_count = probes.max(1);
        self
    }

    /// Sets maximum batch size for a successful steal (minimum `1`).
    pub fn steal_batch_size(mut self, batch_size: usize) -> Self {
        self.steal_batch_size = batch_size.max(1);
        self
    }

    /// Sets locality margin used by steal gating heuristics.
    pub fn steal_locality_margin(mut self, margin: usize) -> Self {
        self.steal_locality_margin = margin;
        self
    }

    /// Sets fail-cost used by steal gating heuristics (minimum `1`).
    pub fn steal_fail_cost(mut self, cost: usize) -> Self {
        self.steal_fail_cost = cost.max(1);
        self
    }

    /// Sets minimum backoff ticks after steal failures (minimum `1`).
    pub fn steal_backoff_min(mut self, min_ticks: usize) -> Self {
        self.steal_backoff_min = min_ticks.max(1);
        if self.steal_backoff_max < self.steal_backoff_min {
            self.steal_backoff_max = self.steal_backoff_min;
        }
        self
    }

    /// Sets maximum backoff ticks after steal failures (minimum `1`).
    pub fn steal_backoff_max(mut self, max_ticks: usize) -> Self {
        self.steal_backoff_max = max_ticks.max(1);
        if self.steal_backoff_max < self.steal_backoff_min {
            self.steal_backoff_min = self.steal_backoff_max;
        }
        self
    }

    #[cfg(target_os = "linux")]
    /// Enables/disables SQPOLL and optionally sets idle timeout in ms.
    pub fn io_uring_sqpoll(mut self, idle_ms: Option<u32>) -> Self {
        self.io_uring.sqpoll_idle_ms = idle_ms;
        if idle_ms.is_none() {
            self.io_uring.sqpoll_cpu = None;
        }
        self
    }

    #[cfg(target_os = "linux")]
    /// Pins SQPOLL thread to a specific CPU (implies SQPOLL enabled).
    pub fn io_uring_sqpoll_cpu(mut self, cpu: Option<u32>) -> Self {
        self.io_uring.sqpoll_cpu = cpu;
        if cpu.is_some() && self.io_uring.sqpoll_idle_ms.is_none() {
            self.io_uring.sqpoll_idle_ms = Some(2_000);
        }
        self
    }

    #[cfg(target_os = "linux")]
    /// Enables/disables `IORING_SETUP_SINGLE_ISSUER`.
    pub fn io_uring_single_issuer(mut self, enable: bool) -> Self {
        self.io_uring.single_issuer = enable;
        self
    }

    #[cfg(target_os = "linux")]
    /// Enables/disables `IORING_SETUP_COOP_TASKRUN`.
    pub fn io_uring_coop_taskrun(mut self, enable: bool) -> Self {
        self.io_uring.coop_taskrun = enable;
        self
    }

    #[cfg(target_os = "linux")]
    /// Convenience profile for throughput-oriented io_uring settings.
    pub fn io_uring_throughput_mode(mut self, sqpoll_idle_ms: Option<u32>) -> Self {
        self.io_uring.coop_taskrun = true;
        self.io_uring.sqpoll_idle_ms = sqpoll_idle_ms;
        if sqpoll_idle_ms.is_none() {
            self.io_uring.sqpoll_cpu = None;
        }
        self
    }

    /// Builds and starts a runtime from this configuration.
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
        let stealable_inboxes = build_stealable_inboxes(self.shards, self.stealable_queue_backend);
        let stealable_wake_flags = build_stealable_wake_flags(self.shards);
        let steal_policy = StealPolicyConfig {
            victim_stride: self.steal_victim_stride.max(1),
            victim_probe_count: self.steal_victim_probe_count.max(1),
            batch_size: self.steal_batch_size.max(1),
            locality_margin: self.steal_locality_margin,
            fail_cost: self.steal_fail_cost.max(1),
            backoff_min: self.steal_backoff_min.max(1),
            backoff_max: self.steal_backoff_max.max(self.steal_backoff_min).max(1),
        };
        let mut hot_msg_tag_bits = build_hot_msg_tag_lookup(&self.hot_msg_tags);
        for &tag in &self.coalesced_hot_msg_tags {
            hot_msg_tag_bits[usize::from(tag)] = true;
        }
        let hot_msg_tags = Arc::new(hot_msg_tag_bits);
        let coalesced_hot_msg_tags =
            Arc::new(build_hot_msg_tag_lookup(&self.coalesced_hot_msg_tags));
        let stats = Arc::new(RuntimeStatsInner::new(self.shards, steal_policy));

        let shared = Arc::new(RuntimeShared {
            runtime_id,
            backend: self.backend,
            command_txs: senders.clone(),
            stealable_inboxes: stealable_inboxes.clone(),
            stealable_wake_flags: stealable_wake_flags.clone(),
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
        let thread_affinity = Arc::new(self.thread_affinity.clone());

        let mut joins = Vec::with_capacity(self.shards);
        match self.backend {
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
                        let stealable_wake_flags = stealable_wake_flags.clone();
                        let hot_msg_tags = hot_msg_tags.clone();
                        let coalesced_hot_msg_tags = coalesced_hot_msg_tags.clone();
                        let thread_affinity = thread_affinity.clone();
                        let thread_name = format!("{}-{}", self.thread_prefix, idx);
                        let runtime_id = shared.runtime_id;
                        let backend = ShardBackend::IoUring(IoUringDriver::new(
                            idx as ShardId,
                            ring,
                            ring_fds.clone(),
                            payload_queues.clone(),
                            coalesced_hot_msg_tags.clone(),
                            stats.clone(),
                            self.msg_ring_queue_capacity,
                        ));
                        let stats = stats.clone();

                        let join = match thread::Builder::new().name(thread_name).spawn(move || {
                            if let Some(cpu) =
                                thread_affinity_cpu_for_shard(thread_affinity.as_ref(), idx)
                            {
                                let _ = set_current_thread_affinity(cpu);
                            }
                            run_shard(
                                runtime_id,
                                idx as ShardId,
                                rx,
                                remotes_for_shard,
                                stealable_deques,
                                stealable_wake_flags,
                                hot_msg_tags,
                                coalesced_hot_msg_tags,
                                self.hot_counter_wake_threshold,
                                self.steal_budget,
                                steal_policy,
                                self.idle_strategy,
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

fn thread_affinity_cpu_for_shard(cpus: &[usize], shard_idx: usize) -> Option<usize> {
    if cpus.is_empty() {
        return None;
    }
    cpus.get(shard_idx % cpus.len()).copied()
}

#[cfg(target_os = "linux")]
fn set_current_thread_affinity(cpu: usize) -> std::io::Result<()> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
    }
    let rc = unsafe {
        libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &set as *const libc::cpu_set_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn set_current_thread_affinity(_cpu: usize) -> std::io::Result<()> {
    Ok(())
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

/// Owned runtime instance.
///
/// Dropping this value shuts workers down (best effort). Prefer calling
/// [`Runtime::shutdown`] to await a clean async shutdown.
pub struct Runtime {
    shared: Arc<RuntimeShared>,
    remotes: Vec<RemoteShard>,
    joins: Vec<thread::JoinHandle<()>>,
    is_shutdown: bool,
}

impl Runtime {
    /// Returns a new [`RuntimeBuilder`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Returns the runtime backend kind.
    pub fn backend(&self) -> BackendKind {
        self.shared.backend
    }

    /// Returns configured shard count.
    pub fn shard_count(&self) -> usize {
        self.remotes.len()
    }

    /// Returns a handle for a specific shard.
    pub fn remote(&self, shard: ShardId) -> Option<RemoteShard> {
        self.remotes.get(usize::from(shard)).cloned()
    }

    /// Returns a clonable runtime handle for spawning and control operations.
    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            inner: Arc::new(RuntimeHandleInner {
                shared: self.shared.clone(),
                remotes: self.remotes.clone(),
                next_shard: AtomicUsize::new(0),
            }),
        }
    }

    /// Spawns a `Send` task pinned to `shard`.
    pub fn spawn_on<F, T>(&self, shard: ShardId, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        spawn_on_shared(&self.shared, shard, fut)
    }

    /// Initiates and awaits full runtime shutdown.
    pub async fn shutdown(&mut self) {
        if self.is_shutdown {
            return;
        }
        self.is_shutdown = true;

        for (idx, tx) in self.shared.command_txs.iter().enumerate() {
            self.shared.stats.increment_command_depth(idx as ShardId);
            let _ = tx.send(Command::Shutdown);
        }

        while !self.joins.is_empty() {
            let mut idx = 0usize;
            while idx < self.joins.len() {
                if self.joins[idx].is_finished() {
                    let join = self.joins.swap_remove(idx);
                    let _ = join.join();
                } else {
                    idx += 1;
                }
            }

            if !self.joins.is_empty() {
                sleep(Duration::from_millis(1)).await;
            }
        }
    }

    fn shutdown_blocking(&mut self) {
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
        self.shutdown_blocking();
    }
}

#[derive(Clone)]
/// Cheap, clonable runtime control handle.
pub struct RuntimeHandle {
    inner: Arc<RuntimeHandleInner>,
}

struct RuntimeHandleInner {
    shared: Arc<RuntimeShared>,
    remotes: Vec<RemoteShard>,
    next_shard: AtomicUsize,
}

impl RuntimeHandle {
    /// Returns the runtime backend kind.
    pub fn backend(&self) -> BackendKind {
        self.inner.shared.backend
    }

    /// Returns configured shard count.
    pub fn shard_count(&self) -> usize {
        self.inner.remotes.len()
    }

    /// Returns a handle for a specific shard.
    pub fn remote(&self, shard: ShardId) -> Option<RemoteShard> {
        self.inner.remotes.get(usize::from(shard)).cloned()
    }

    /// Spawns a local (`!Send`) task initializer on `shard`.
    ///
    /// The `init` closure runs on the target shard and may return a `!Send`
    /// future tied to that shard-local context.
    pub fn spawn_local_on<F, Fut, T>(
        &self,
        shard: ShardId,
        init: F,
    ) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: FnOnce(ShardCtx) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        self.inner
            .shared
            .stats
            .spawn_pinned_submitted
            .fetch_add(1, Ordering::Relaxed);
        spawn_local_on_shared(&self.inner.shared, shard, init)
    }

    /// Spawns a `Send` task pinned to `shard`.
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

    /// Spawns a stealable task using round-robin preferred shard selection.
    pub fn spawn_stealable<F, T>(&self, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_with_placement(TaskPlacement::Stealable, fut)
    }

    /// Spawns a stealable task with explicit preferred shard.
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

    /// Spawns a `Send` task with explicit placement policy.
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

    /// Spawns blocking CPU/OS work on a dedicated helper thread.
    pub fn spawn_blocking<F, T>(&self, f: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        thread::Builder::new()
            .name("spargio-blocking".to_string())
            .spawn(move || {
                let out = f();
                let _ = tx.send(out);
            })
            .map_err(RuntimeError::ThreadSpawn)?;
        Ok(JoinHandle { rx: Some(rx) })
    }

    /// Returns a point-in-time runtime statistics snapshot.
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

    /// Submits a low-level custom `io_uring` operation on an explicit shard.
    ///
    /// `state` is retained until completion so extension writers can keep any
    /// buffers/ownership needed by the SQE alive.
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    /// - any pointers/FD references encoded in the built SQE remain valid until
    ///   completion is delivered for this operation.
    /// - SQE semantics (opcode, flags, argument layout) are valid for the
    ///   running kernel and target descriptor.
    /// - completion decoding in `complete` is correct for the submitted opcode.
    pub async unsafe fn submit_unsafe_on_shard<S, T, B, C>(
        &self,
        shard: ShardId,
        state: S,
        build: B,
        complete: C,
    ) -> std::io::Result<T>
    where
        S: Send + 'static,
        T: Send + 'static,
        B: FnOnce(&mut S) -> std::io::Result<io_uring::squeue::Entry> + Send + 'static,
        C: FnOnce(S, UringCqe) -> std::io::Result<T> + Send + 'static,
    {
        if usize::from(shard) >= self.handle.shard_count() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("invalid shard {shard}"),
            ));
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let op = NativeUnsafeOpEnvelope::new(state, build, complete, reply_tx);
        self.dispatch_native_any(shard, NativeAnyCommand::Unsafe { op: Box::new(op) })?;
        reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native unsafe op response channel closed",
            ))
        })
    }

    /// Submits a low-level custom `io_uring` operation using current selector policy.
    ///
    /// This is equivalent to selecting a shard with `select_shard(...)` and then
    /// calling [`UringNativeAny::submit_unsafe_on_shard`].
    ///
    /// # Safety
    ///
    /// Same safety requirements as [`UringNativeAny::submit_unsafe_on_shard`].
    pub async unsafe fn submit_unsafe<S, T, B, C>(
        &self,
        state: S,
        build: B,
        complete: C,
    ) -> std::io::Result<T>
    where
        S: Send + 'static,
        T: Send + 'static,
        B: FnOnce(&mut S) -> std::io::Result<io_uring::squeue::Entry> + Send + 'static,
        C: FnOnce(S, UringCqe) -> std::io::Result<T> + Send + 'static,
    {
        let shard = self.selector.select(self.effective_preferred_shard());
        unsafe {
            self.submit_unsafe_on_shard(shard, state, build, complete)
                .await
        }
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

    pub async fn sleep(&self, duration: Duration) -> std::io::Result<()> {
        let shard = self.selector.select(self.effective_preferred_shard());
        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::Timeout { duration, reply },
            "native unbound timeout response channel closed",
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

    pub async fn mkdir_at<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        mode: libc::mode_t,
    ) -> std::io::Result<()> {
        let path = path_to_cstring_for_native_ops(path.as_ref())?;
        unsafe {
            self.submit_unsafe(
                (path, mode),
                |state| {
                    let (path, mode) = state;
                    Ok(
                        opcode::MkDirAt::new(types::Fd(libc::AT_FDCWD), path.as_ptr())
                            .mode(*mode)
                            .build(),
                    )
                },
                |_, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    Ok(())
                },
            )
            .await
        }
    }

    pub async fn unlink_at<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        is_dir: bool,
    ) -> std::io::Result<()> {
        let path = path_to_cstring_for_native_ops(path.as_ref())?;
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        unsafe {
            self.submit_unsafe(
                (path, flags),
                |state| {
                    let (path, flags) = state;
                    Ok(
                        opcode::UnlinkAt::new(types::Fd(libc::AT_FDCWD), path.as_ptr())
                            .flags(*flags)
                            .build(),
                    )
                },
                |_, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    Ok(())
                },
            )
            .await
        }
    }

    pub async fn rename_at<P: AsRef<std::path::Path>, Q: AsRef<std::path::Path>>(
        &self,
        from: P,
        to: Q,
    ) -> std::io::Result<()> {
        let from = path_to_cstring_for_native_ops(from.as_ref())?;
        let to = path_to_cstring_for_native_ops(to.as_ref())?;
        unsafe {
            self.submit_unsafe(
                (from, to),
                |state| {
                    let (from, to) = state;
                    Ok(opcode::RenameAt::new(
                        types::Fd(libc::AT_FDCWD),
                        from.as_ptr(),
                        types::Fd(libc::AT_FDCWD),
                        to.as_ptr(),
                    )
                    .build())
                },
                |_, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    Ok(())
                },
            )
            .await
        }
    }

    pub async fn link_at<P: AsRef<std::path::Path>, Q: AsRef<std::path::Path>>(
        &self,
        original: P,
        link: Q,
    ) -> std::io::Result<()> {
        let original = path_to_cstring_for_native_ops(original.as_ref())?;
        let link = path_to_cstring_for_native_ops(link.as_ref())?;
        unsafe {
            self.submit_unsafe(
                (original, link),
                |state| {
                    let (original, link) = state;
                    Ok(opcode::LinkAt::new(
                        types::Fd(libc::AT_FDCWD),
                        original.as_ptr(),
                        types::Fd(libc::AT_FDCWD),
                        link.as_ptr(),
                    )
                    .build())
                },
                |_, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    Ok(())
                },
            )
            .await
        }
    }

    pub async fn symlink_at<P: AsRef<std::path::Path>, Q: AsRef<std::path::Path>>(
        &self,
        target: P,
        linkpath: Q,
    ) -> std::io::Result<()> {
        let target = path_to_cstring_for_native_ops(target.as_ref())?;
        let linkpath = path_to_cstring_for_native_ops(linkpath.as_ref())?;
        unsafe {
            self.submit_unsafe(
                (target, linkpath),
                |state| {
                    let (target, linkpath) = state;
                    Ok(opcode::SymlinkAt::new(
                        types::Fd(libc::AT_FDCWD),
                        target.as_ptr(),
                        linkpath.as_ptr(),
                    )
                    .build())
                },
                |_, cqe| {
                    if cqe.result < 0 {
                        return Err(std::io::Error::from_raw_os_error(-cqe.result));
                    }
                    Ok(())
                },
            )
            .await
        }
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
            |reply| NativeAnyCommand::RecvOwned {
                fd,
                buf,
                offset: 0,
                reply,
            },
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
            |reply| NativeAnyCommand::SendOwned {
                fd,
                buf,
                offset: 0,
                reply,
            },
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

    pub(crate) async fn recv_owned_at_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
    ) -> std::io::Result<(usize, Vec<u8>)> {
        let mut maybe_buf = Some(buf);
        if let Some(reply_rx) = ShardCtx::current()
            .filter(|ctx| ctx.runtime_id() == self.handle.inner.shared.runtime_id)
            .filter(|ctx| ctx.shard_id() == shard)
            .map(|ctx| {
                let buf = maybe_buf
                    .take()
                    .expect("recv_owned_at_on_shard local branch already consumed buffer");
                self.handle
                    .inner
                    .shared
                    .stats
                    .native_any_local_fastpath_submitted
                    .fetch_add(1, Ordering::Relaxed);
                self.handle
                    .inner
                    .shared
                    .stats
                    .native_any_local_direct_submitted
                    .fetch_add(1, Ordering::Relaxed);
                let (reply, reply_rx) = NativeBufReply::local_pair();
                ctx.inner.local_commands.borrow_mut().push_back(
                    LocalCommand::SubmitNativeRecvOwned {
                        origin_shard: shard,
                        fd,
                        buf,
                        offset,
                        reply,
                    },
                );
                reply_rx
            })
        {
            return reply_rx.await;
        }

        let buf = maybe_buf
            .take()
            .expect("recv_owned_at_on_shard remote branch missing buffer");

        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::RecvOwned {
                fd,
                buf,
                offset,
                reply,
            },
            "native stream-session recv response channel closed",
        )
        .await
    }

    pub(crate) async fn send_owned_at_on_shard(
        &self,
        shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
    ) -> std::io::Result<(usize, Vec<u8>)> {
        let mut maybe_buf = Some(buf);
        if let Some(reply_rx) = ShardCtx::current()
            .filter(|ctx| ctx.runtime_id() == self.handle.inner.shared.runtime_id)
            .filter(|ctx| ctx.shard_id() == shard)
            .map(|ctx| {
                let buf = maybe_buf
                    .take()
                    .expect("send_owned_at_on_shard local branch already consumed buffer");
                self.handle
                    .inner
                    .shared
                    .stats
                    .native_any_local_fastpath_submitted
                    .fetch_add(1, Ordering::Relaxed);
                self.handle
                    .inner
                    .shared
                    .stats
                    .native_any_local_direct_submitted
                    .fetch_add(1, Ordering::Relaxed);
                let (reply, reply_rx) = NativeBufReply::local_pair();
                ctx.inner.local_commands.borrow_mut().push_back(
                    LocalCommand::SubmitNativeSendOwned {
                        origin_shard: shard,
                        fd,
                        buf,
                        offset,
                        reply,
                    },
                );
                reply_rx
            })
        {
            return reply_rx.await;
        }

        let buf = maybe_buf
            .take()
            .expect("send_owned_at_on_shard remote branch missing buffer");

        self.submit_direct(
            shard,
            |reply| NativeAnyCommand::SendOwned {
                fd,
                buf,
                offset,
                reply,
            },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UringCqe {
    pub result: i32,
    pub flags: u32,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod fs {
    use super::{RuntimeError, RuntimeHandle, UringNativeAny};
    use std::collections::HashSet;
    use std::ffi::CString;
    use std::fs::{Metadata, Permissions};
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const READ_TO_END_CHUNK: usize = 64 * 1024;
    static CREATE_DIR_ALL_BLOCKING_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

    #[doc(hidden)]
    pub fn create_dir_all_blocking_fallback_count_for_test() -> u64 {
        CREATE_DIR_ALL_BLOCKING_FALLBACK_COUNT.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn reset_create_dir_all_blocking_fallback_count_for_test() {
        CREATE_DIR_ALL_BLOCKING_FALLBACK_COUNT.store(0, Ordering::Relaxed);
    }

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

    pub async fn create_dir<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let path_ref = path.as_ref();
        match native.mkdir_at(path_ref, 0o777).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let path = path_ref.to_path_buf();
                run_blocking(handle, move || std::fs::create_dir(path)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn create_dir_all<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<()> {
        let path_ref = path.as_ref();
        if path_ref.as_os_str().is_empty() {
            return Ok(());
        }

        // Preserve std behavior for complex relative forms (`.`/`..`) by
        // using the compatibility blocking path.
        if path_ref.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        }) {
            CREATE_DIR_ALL_BLOCKING_FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
            let path = path_ref.to_path_buf();
            return run_blocking(handle, move || std::fs::create_dir_all(path)).await;
        }

        let mut current = PathBuf::new();
        for component in path_ref.components() {
            match component {
                Component::RootDir => {
                    current.push(component.as_os_str());
                    continue;
                }
                Component::Normal(part) => current.push(part),
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => continue,
            }

            match create_dir(handle, &current).await {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }

    pub async fn remove_file<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let path_ref = path.as_ref();
        match native.unlink_at(path_ref, false).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let path = path_ref.to_path_buf();
                run_blocking(handle, move || std::fs::remove_file(path)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn remove_dir<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let path_ref = path.as_ref();
        match native.unlink_at(path_ref, true).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let path = path_ref.to_path_buf();
                run_blocking(handle, move || std::fs::remove_dir(path)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(
        handle: &RuntimeHandle,
        from: P,
        to: Q,
    ) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let from_ref = from.as_ref();
        let to_ref = to.as_ref();
        match native.rename_at(from_ref, to_ref).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let from = from_ref.to_path_buf();
                let to = to_ref.to_path_buf();
                run_blocking(handle, move || std::fs::rename(from, to)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(
        handle: &RuntimeHandle,
        original: P,
        link: Q,
    ) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let original_ref = original.as_ref();
        let link_ref = link.as_ref();
        match native.link_at(original_ref, link_ref).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let original = original_ref.to_path_buf();
                let link = link_ref.to_path_buf();
                run_blocking(handle, move || std::fs::hard_link(original, link)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(
        handle: &RuntimeHandle,
        original: P,
        link: Q,
    ) -> io::Result<()> {
        let native = handle
            .uring_native_unbound()
            .map_err(runtime_error_to_io_for_native)?;
        let original_ref = original.as_ref();
        let link_ref = link.as_ref();
        match native.symlink_at(original_ref, link_ref).await {
            Ok(()) => Ok(()),
            Err(err) if should_fallback_to_blocking_for_path_op(&err) => {
                let original = original_ref.to_path_buf();
                let link = link_ref.to_path_buf();
                run_blocking(handle, move || std::os::unix::fs::symlink(original, link)).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn metadata<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<Metadata> {
        let path = path.as_ref().to_path_buf();
        run_blocking(handle, move || std::fs::metadata(path)).await
    }

    pub async fn metadata_lite<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
    ) -> io::Result<super::extension::fs::StatxMetadata> {
        let path = path.as_ref().to_path_buf();
        super::extension::fs::statx_or_metadata(handle.clone(), path).await
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DirEntryType {
        File,
        Directory,
        Symlink,
        BlockDevice,
        CharDevice,
        Fifo,
        Socket,
        Unknown,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DirEntry {
        pub file_name: String,
        pub path: PathBuf,
        pub inode: u64,
        pub entry_type: DirEntryType,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum DuSizeMode {
        #[default]
        Allocated,
        Apparent,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum DuSymlinkMode {
        #[default]
        NoFollow,
        Follow,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum DuErrorMode {
        #[default]
        FailFast,
        Skip,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DuOptions {
        pub size_mode: DuSizeMode,
        pub symlink_mode: DuSymlinkMode,
        pub hardlink_dedupe: bool,
        pub one_file_system: bool,
        pub error_mode: DuErrorMode,
    }

    impl Default for DuOptions {
        fn default() -> Self {
            Self {
                size_mode: DuSizeMode::Allocated,
                symlink_mode: DuSymlinkMode::NoFollow,
                hardlink_dedupe: true,
                one_file_system: false,
                error_mode: DuErrorMode::FailFast,
            }
        }
    }

    impl DuOptions {
        pub fn size_mode(mut self, size_mode: DuSizeMode) -> Self {
            self.size_mode = size_mode;
            self
        }

        pub fn symlink_mode(mut self, symlink_mode: DuSymlinkMode) -> Self {
            self.symlink_mode = symlink_mode;
            self
        }

        pub fn hardlink_dedupe(mut self, hardlink_dedupe: bool) -> Self {
            self.hardlink_dedupe = hardlink_dedupe;
            self
        }

        pub fn one_file_system(mut self, one_file_system: bool) -> Self {
            self.one_file_system = one_file_system;
            self
        }

        pub fn error_mode(mut self, error_mode: DuErrorMode) -> Self {
            self.error_mode = error_mode;
            self
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct DuSummary {
        pub total_bytes: u64,
        pub total_entries: u64,
        pub files: u64,
        pub directories: u64,
        pub symlinks: u64,
        pub skipped_errors: u64,
        pub skipped_cross_device: u64,
    }

    pub async fn read_dir<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
    ) -> io::Result<Vec<DirEntry>> {
        let path = path.as_ref().to_path_buf();
        let mut entries = super::extension::fs::read_dir_entries(handle.clone(), &path).await?;
        entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                file_name: entry.file_name.clone(),
                path: path.join(entry.file_name),
                inode: entry.inode,
                entry_type: dir_entry_type_from_extension(entry.entry_type),
            })
            .collect())
    }

    pub async fn du<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        root: P,
        options: DuOptions,
    ) -> io::Result<DuSummary> {
        let root = root.as_ref().to_path_buf();
        run_blocking(handle, move || du_blocking(&root, options)).await
    }

    pub async fn symlink_metadata<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
    ) -> io::Result<Metadata> {
        let path = path.as_ref().to_path_buf();
        run_blocking(handle, move || std::fs::symlink_metadata(path)).await
    }

    pub async fn set_permissions<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
        perm: Permissions,
    ) -> io::Result<()> {
        let path = path.as_ref().to_path_buf();
        run_blocking(handle, move || std::fs::set_permissions(path, perm)).await
    }

    pub async fn canonicalize<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
    ) -> io::Result<PathBuf> {
        let path = path.as_ref().to_path_buf();
        run_blocking(handle, move || std::fs::canonicalize(path)).await
    }

    pub async fn read<P: AsRef<Path>>(handle: &RuntimeHandle, path: P) -> io::Result<Vec<u8>> {
        let file = File::open(handle.clone(), path).await?;
        file.read_to_end_at(0).await
    }

    pub async fn read_to_string<P: AsRef<Path>>(
        handle: &RuntimeHandle,
        path: P,
    ) -> io::Result<String> {
        let bytes = read(handle, path).await?;
        String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub async fn write<P: AsRef<Path>, B: AsRef<[u8]>>(
        handle: &RuntimeHandle,
        path: P,
        contents: B,
    ) -> io::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(handle.clone(), path)
            .await?;
        file.write_all_at(0, contents.as_ref()).await?;
        file.fsync().await
    }

    fn path_to_cstring(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            )
        })
    }

    fn should_fallback_to_blocking_for_path_op(err: &io::Error) -> bool {
        matches!(
            err.raw_os_error(),
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
        )
    }

    fn dir_entry_type_from_extension(
        entry_type: super::extension::fs::DirEntryType,
    ) -> DirEntryType {
        match entry_type {
            super::extension::fs::DirEntryType::File => DirEntryType::File,
            super::extension::fs::DirEntryType::Directory => DirEntryType::Directory,
            super::extension::fs::DirEntryType::Symlink => DirEntryType::Symlink,
            super::extension::fs::DirEntryType::BlockDevice => DirEntryType::BlockDevice,
            super::extension::fs::DirEntryType::CharDevice => DirEntryType::CharDevice,
            super::extension::fs::DirEntryType::Fifo => DirEntryType::Fifo,
            super::extension::fs::DirEntryType::Socket => DirEntryType::Socket,
            super::extension::fs::DirEntryType::Unknown => DirEntryType::Unknown,
        }
    }

    fn du_blocking(root: &Path, options: DuOptions) -> io::Result<DuSummary> {
        let mut summary = DuSummary::default();
        let mut stack = vec![root.to_path_buf()];
        let mut seen_dirs: HashSet<(u64, u64)> = HashSet::new();
        let mut seen_hardlinks: HashSet<(u64, u64)> = HashSet::new();

        let root_dev = if options.one_file_system {
            Some(std::fs::symlink_metadata(root)?.dev())
        } else {
            None
        };

        while let Some(path) = stack.pop() {
            let symlink_meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(err) => {
                    if should_skip_du_error(options.error_mode, &mut summary, err)? {
                        continue;
                    }
                    unreachable!();
                }
            };

            let is_symlink = symlink_meta.file_type().is_symlink();
            if is_symlink {
                summary.symlinks = summary.symlinks.saturating_add(1);
            }

            if is_symlink && matches!(options.symlink_mode, DuSymlinkMode::NoFollow) {
                summary.total_entries = summary.total_entries.saturating_add(1);
                summary.total_bytes = summary
                    .total_bytes
                    .saturating_add(du_bytes_for(&symlink_meta, options.size_mode));
                continue;
            }

            let meta = if is_symlink {
                match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(err) => {
                        if should_skip_du_error(options.error_mode, &mut summary, err)? {
                            continue;
                        }
                        unreachable!();
                    }
                }
            } else {
                symlink_meta
            };

            let dev = meta.dev();
            if let Some(root_dev) = root_dev {
                if path != root && dev != root_dev {
                    summary.skipped_cross_device = summary.skipped_cross_device.saturating_add(1);
                    continue;
                }
            }

            let ino = meta.ino();
            let is_dir = meta.file_type().is_dir();
            let is_file = meta.file_type().is_file();

            if is_dir {
                if !seen_dirs.insert((dev, ino)) {
                    continue;
                }
                summary.directories = summary.directories.saturating_add(1);
            } else if is_file {
                summary.files = summary.files.saturating_add(1);
            }

            summary.total_entries = summary.total_entries.saturating_add(1);

            if options.hardlink_dedupe && meta.nlink() > 1 {
                if !seen_hardlinks.insert((dev, ino)) {
                    continue;
                }
            }

            summary.total_bytes = summary
                .total_bytes
                .saturating_add(du_bytes_for(&meta, options.size_mode));

            if is_dir {
                let iter = match std::fs::read_dir(&path) {
                    Ok(iter) => iter,
                    Err(err) => {
                        if should_skip_du_error(options.error_mode, &mut summary, err)? {
                            continue;
                        }
                        unreachable!();
                    }
                };

                for entry in iter {
                    match entry {
                        Ok(entry) => stack.push(entry.path()),
                        Err(err) => {
                            if should_skip_du_error(options.error_mode, &mut summary, err)? {
                                continue;
                            }
                            unreachable!();
                        }
                    }
                }
            }
        }

        Ok(summary)
    }

    fn should_skip_du_error(
        mode: DuErrorMode,
        summary: &mut DuSummary,
        err: io::Error,
    ) -> io::Result<bool> {
        if matches!(mode, DuErrorMode::Skip) {
            summary.skipped_errors = summary.skipped_errors.saturating_add(1);
            return Ok(true);
        }
        Err(err)
    }

    fn du_bytes_for(meta: &Metadata, mode: DuSizeMode) -> u64 {
        match mode {
            DuSizeMode::Allocated => meta.blocks().saturating_mul(512),
            DuSizeMode::Apparent => meta.len(),
        }
    }

    async fn run_blocking<T, F>(handle: &RuntimeHandle, f: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        let join = handle
            .spawn_blocking(f)
            .map_err(runtime_error_to_io_for_native)?;
        join.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "blocking helper worker exited before sending result",
            )
        })?
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
        UdpSocket as StdUdpSocket,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::{
        SocketAddr as UnixSocketAddr, UnixDatagram as StdUnixDatagram,
        UnixListener as StdUnixListener, UnixStream as StdUnixStream,
    };
    use std::path::Path;
    use std::sync::Arc;

    const IO_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);

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

        pub async fn connect_socket_addr(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
        ) -> io::Result<Self> {
            Self::connect_socket_addr_with_session_policy(
                handle,
                socket_addr,
                StreamSessionPolicy::ContextPreferred,
            )
            .await
        }

        pub async fn connect_socket_addr_round_robin(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
        ) -> io::Result<Self> {
            Self::connect_socket_addr_with_session_policy(
                handle,
                socket_addr,
                StreamSessionPolicy::RoundRobin,
            )
            .await
        }

        pub async fn connect_with_session_policy<A>(
            handle: RuntimeHandle,
            addr: A,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = resolve_first_socket_addr_blocking(addr)?;
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

        pub async fn connect_many_socket_addr_round_robin(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
            count: usize,
        ) -> io::Result<Vec<Self>> {
            Self::connect_many_socket_addr_with_session_policy(
                handle,
                socket_addr,
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
            let socket_addr = resolve_first_socket_addr_blocking(addr)?;
            Self::connect_many_socket_addr_with_session_policy(handle, socket_addr, count, policy)
                .await
        }

        pub async fn connect_many_socket_addr_with_session_policy(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
            count: usize,
            policy: StreamSessionPolicy,
        ) -> io::Result<Vec<Self>> {
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

        pub async fn connect_socket_addr_with_session_policy(
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
            self.send_owned_from(buf, 0).await
        }

        async fn send_owned_from(
            &self,
            buf: Vec<u8>,
            offset: usize,
        ) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .send_owned_at_on_shard(self.session_shard, self.as_raw_fd(), buf, offset)
                .await
        }

        pub async fn recv_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.recv_owned_from(buf, 0).await
        }

        async fn recv_owned_from(
            &self,
            buf: Vec<u8>,
            offset: usize,
        ) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .recv_owned_at_on_shard(self.session_shard, self.as_raw_fd(), buf, offset)
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
                let remaining = buf.len().saturating_sub(sent);
                let (wrote, returned) = self.send_owned_from(buf, sent).await?;
                buf = returned;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "send returned zero",
                    ));
                }
                sent = sent.saturating_add(wrote.min(remaining));
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
            let mut received = 0usize;
            while received < dst.len() {
                let remaining = dst.len().saturating_sub(received);
                let (got, returned) = self.recv_owned_from(dst, received).await?;
                dst = returned;
                if got == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream closed",
                    ));
                }
                received = received.saturating_add(got.min(remaining));
            }
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
            let socket_addr = resolve_first_socket_addr_blocking(addr)?;
            Self::bind_socket_addr(handle, socket_addr).await
        }

        pub async fn bind_socket_addr(
            handle: RuntimeHandle,
            socket_addr: SocketAddr,
        ) -> io::Result<Self> {
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

    #[derive(Clone)]
    pub struct UdpSocket {
        handle: RuntimeHandle,
        socket: Arc<StdUdpSocket>,
    }

    impl UdpSocket {
        pub async fn bind<A>(handle: RuntimeHandle, addr: A) -> io::Result<Self>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = resolve_first_socket_addr_blocking(addr)?;
            let socket = StdUdpSocket::bind(socket_addr)?;
            socket.set_nonblocking(true)?;
            Ok(Self {
                handle,
                socket: Arc::new(socket),
            })
        }

        pub fn from_std(handle: RuntimeHandle, socket: StdUdpSocket) -> io::Result<Self> {
            socket.set_nonblocking(true)?;
            Ok(Self {
                handle,
                socket: Arc::new(socket),
            })
        }

        pub fn as_raw_fd(&self) -> RawFd {
            self.socket.as_raw_fd()
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            self.socket.local_addr()
        }

        pub async fn connect<A>(&self, addr: A) -> io::Result<()>
        where
            A: ToSocketAddrs,
        {
            let socket_addr = resolve_first_socket_addr_blocking(addr)?;
            self.socket.connect(socket_addr)
        }

        pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
            loop {
                match self.socket.send(buf) {
                    Ok(sent) => return Ok(sent),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn recv(&self, len: usize) -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; len.max(1)];
            loop {
                match self.socket.recv(&mut buf) {
                    Ok(got) => {
                        buf.truncate(got.min(buf.len()));
                        return Ok(buf);
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
            loop {
                match self.socket.send_to(buf, target) {
                    Ok(sent) => return Ok(sent),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn recv_from(&self, len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
            let mut buf = vec![0u8; len.max(1)];
            loop {
                match self.socket.recv_from(&mut buf) {
                    Ok((got, from)) => {
                        buf.truncate(got.min(buf.len()));
                        return Ok((buf, from));
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub fn handle(&self) -> RuntimeHandle {
            self.handle.clone()
        }
    }

    #[derive(Clone)]
    pub struct UnixStream {
        native: UringNativeAny,
        fd: Arc<OwnedFd>,
        session_shard: ShardId,
    }

    impl UnixStream {
        pub async fn connect<P: AsRef<Path>>(handle: RuntimeHandle, path: P) -> io::Result<Self> {
            Self::connect_with_session_policy(handle, path, StreamSessionPolicy::ContextPreferred)
                .await
        }

        pub async fn connect_with_session_policy<P: AsRef<Path>>(
            handle: RuntimeHandle,
            path: P,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self> {
            let stream = StdUnixStream::connect(path)?;
            Self::from_std_with_session_policy(handle, stream, policy)
        }

        pub fn from_std(handle: RuntimeHandle, stream: StdUnixStream) -> io::Result<Self> {
            Self::from_std_with_session_policy(
                handle,
                stream,
                StreamSessionPolicy::ContextPreferred,
            )
        }

        pub fn from_std_with_session_policy(
            handle: RuntimeHandle,
            stream: StdUnixStream,
            policy: StreamSessionPolicy,
        ) -> io::Result<Self> {
            let (native, session_shard) = select_native_for_policy(handle, policy)?;
            stream.set_nonblocking(true)?;
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

        pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
            let (sent, _) = self.send_owned(buf.to_vec()).await?;
            Ok(sent)
        }

        pub async fn recv(&self, len: usize) -> io::Result<Vec<u8>> {
            let (got, mut buf) = self.recv_owned(vec![0; len.max(1)]).await?;
            buf.truncate(got.min(buf.len()));
            Ok(buf)
        }

        pub async fn send_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.send_owned_from(buf, 0).await
        }

        async fn send_owned_from(
            &self,
            buf: Vec<u8>,
            offset: usize,
        ) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .send_owned_at_on_shard(self.session_shard, self.as_raw_fd(), buf, offset)
                .await
        }

        pub async fn recv_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.recv_owned_from(buf, 0).await
        }

        async fn recv_owned_from(
            &self,
            buf: Vec<u8>,
            offset: usize,
        ) -> io::Result<(usize, Vec<u8>)> {
            self.native
                .recv_owned_at_on_shard(self.session_shard, self.as_raw_fd(), buf, offset)
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
                let remaining = buf.len().saturating_sub(sent);
                let (wrote, returned) = self.send_owned_from(buf, sent).await?;
                buf = returned;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "send returned zero",
                    ));
                }
                sent = sent.saturating_add(wrote.min(remaining));
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
            let mut received = 0usize;
            while received < dst.len() {
                let remaining = dst.len().saturating_sub(received);
                let (got, returned) = self.recv_owned_from(dst, received).await?;
                dst = returned;
                if got == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream closed",
                    ));
                }
                received = received.saturating_add(got.min(remaining));
            }
            Ok(dst)
        }
    }

    #[derive(Clone)]
    pub struct UnixListener {
        handle: RuntimeHandle,
        listener: Arc<StdUnixListener>,
    }

    impl UnixListener {
        pub async fn bind<P: AsRef<Path>>(handle: RuntimeHandle, path: P) -> io::Result<Self> {
            let path = path.as_ref();
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            let listener = StdUnixListener::bind(path)?;
            listener.set_nonblocking(true)?;
            Ok(Self {
                handle,
                listener: Arc::new(listener),
            })
        }

        pub fn from_std(handle: RuntimeHandle, listener: StdUnixListener) -> io::Result<Self> {
            listener.set_nonblocking(true)?;
            Ok(Self {
                handle,
                listener: Arc::new(listener),
            })
        }

        pub fn local_addr(&self) -> io::Result<UnixSocketAddr> {
            self.listener.local_addr()
        }

        pub async fn accept(&self) -> io::Result<(UnixStream, UnixSocketAddr)> {
            loop {
                match self.listener.accept() {
                    Ok((stream, addr)) => {
                        let stream = UnixStream::from_std(self.handle.clone(), stream)?;
                        return Ok((stream, addr));
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }

    #[derive(Clone)]
    pub struct UnixDatagram {
        handle: RuntimeHandle,
        socket: Arc<StdUnixDatagram>,
    }

    impl UnixDatagram {
        pub async fn bind<P: AsRef<Path>>(handle: RuntimeHandle, path: P) -> io::Result<Self> {
            let path = path.as_ref();
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            let socket = StdUnixDatagram::bind(path)?;
            socket.set_nonblocking(true)?;
            Ok(Self {
                handle,
                socket: Arc::new(socket),
            })
        }

        pub fn from_std(handle: RuntimeHandle, socket: StdUnixDatagram) -> io::Result<Self> {
            socket.set_nonblocking(true)?;
            Ok(Self {
                handle,
                socket: Arc::new(socket),
            })
        }

        pub fn local_addr(&self) -> io::Result<UnixSocketAddr> {
            self.socket.local_addr()
        }

        pub async fn connect<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
            self.socket.connect(path)
        }

        pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
            loop {
                match self.socket.send(buf) {
                    Ok(sent) => return Ok(sent),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn recv(&self, len: usize) -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; len.max(1)];
            loop {
                match self.socket.recv(&mut buf) {
                    Ok(got) => {
                        buf.truncate(got.min(buf.len()));
                        return Ok(buf);
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
            loop {
                match self.socket.send_to(buf, path.as_ref()) {
                    Ok(sent) => return Ok(sent),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn recv_from(&self, len: usize) -> io::Result<(Vec<u8>, UnixSocketAddr)> {
            let mut buf = vec![0u8; len.max(1)];
            loop {
                match self.socket.recv_from(&mut buf) {
                    Ok((got, from)) => {
                        buf.truncate(got.min(buf.len()));
                        return Ok((buf, from));
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        super::sleep(IO_RETRY_SLEEP).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub fn handle(&self) -> RuntimeHandle {
            self.handle.clone()
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

    fn resolve_first_socket_addr_blocking<A>(addr: A) -> io::Result<SocketAddr>
    where
        A: ToSocketAddrs,
    {
        // NOTE: hostname resolution via `ToSocketAddrs` may block for DNS.
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

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod io {
    #![allow(async_fn_in_trait)]

    use super::net;
    use std::io;
    use std::sync::Mutex;

    pub trait AsyncRead {
        async fn read_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)>;
    }

    pub trait AsyncWrite {
        async fn write_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)>;
    }

    impl AsyncRead for net::TcpStream {
        async fn read_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.recv_owned(buf).await
        }
    }

    impl AsyncWrite for net::TcpStream {
        async fn write_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.send_owned(buf).await
        }
    }

    impl AsyncRead for net::UnixStream {
        async fn read_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.recv_owned(buf).await
        }
    }

    impl AsyncWrite for net::UnixStream {
        async fn write_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.send_owned(buf).await
        }
    }

    pub trait AsyncReadExt: AsyncRead {
        async fn read_exact_owned(&self, mut dst: Vec<u8>) -> io::Result<Vec<u8>> {
            let mut received = 0usize;
            while received < dst.len() {
                let remaining = dst.len().saturating_sub(received);
                let (got, out) = self.read_owned(dst).await?;
                dst = out;
                if got == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream closed",
                    ));
                }
                received = received.saturating_add(got.min(remaining));
            }
            Ok(dst)
        }
    }

    impl<T: AsyncRead + ?Sized> AsyncReadExt for T {}

    pub trait AsyncWriteExt: AsyncWrite {
        async fn write_all_owned(&self, mut src: Vec<u8>) -> io::Result<Vec<u8>> {
            let mut sent = 0usize;
            while sent < src.len() {
                let remaining = src.len().saturating_sub(sent);
                let (wrote, out) = self.write_owned(src).await?;
                src = out;
                if wrote == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned zero",
                    ));
                }
                sent = sent.saturating_add(wrote.min(remaining));
            }
            Ok(src)
        }
    }

    impl<T: AsyncWrite + ?Sized> AsyncWriteExt for T {}

    #[derive(Clone)]
    pub struct ReadHalf<T: Clone> {
        inner: T,
    }

    #[derive(Clone)]
    pub struct WriteHalf<T: Clone> {
        inner: T,
    }

    pub fn split<T: Clone>(io: T) -> (ReadHalf<T>, WriteHalf<T>) {
        (ReadHalf { inner: io.clone() }, WriteHalf { inner: io })
    }

    impl<T> ReadHalf<T>
    where
        T: Clone,
    {
        pub fn into_inner(self) -> T {
            self.inner
        }
    }

    impl<T> WriteHalf<T>
    where
        T: Clone,
    {
        pub fn into_inner(self) -> T {
            self.inner
        }
    }

    impl<T> AsyncRead for ReadHalf<T>
    where
        T: AsyncRead + Clone,
    {
        async fn read_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.inner.read_owned(buf).await
        }
    }

    impl<T> AsyncWrite for WriteHalf<T>
    where
        T: AsyncWrite + Clone,
    {
        async fn write_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            self.inner.write_owned(buf).await
        }
    }

    pub async fn copy_to_vec<R>(
        reader: &R,
        dst: &mut Vec<u8>,
        chunk_size: usize,
    ) -> io::Result<usize>
    where
        R: AsyncRead + ?Sized,
    {
        let mut total = 0usize;
        let chunk_size = chunk_size.max(1);
        loop {
            let (got, buf) = reader.read_owned(vec![0u8; chunk_size]).await?;
            if got == 0 {
                return Ok(total);
            }
            let got = got.min(buf.len());
            dst.extend_from_slice(&buf[..got]);
            total = total.saturating_add(got);
        }
    }

    pub struct BufReader<R> {
        inner: R,
        capacity: usize,
        stash: Mutex<Vec<u8>>,
    }

    impl<R> BufReader<R> {
        pub fn new(inner: R) -> Self {
            Self {
                inner,
                capacity: 8 * 1024,
                stash: Mutex::new(Vec::new()),
            }
        }

        pub fn with_capacity(inner: R, capacity: usize) -> Self {
            Self {
                inner,
                capacity: capacity.max(1),
                stash: Mutex::new(Vec::new()),
            }
        }
    }

    impl<R> AsyncRead for BufReader<R>
    where
        R: AsyncRead,
    {
        async fn read_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            {
                let mut stash = self.stash.lock().expect("buf reader stash lock poisoned");
                if !stash.is_empty() {
                    let mut out = buf;
                    let take = stash.len().min(out.len());
                    out[..take].copy_from_slice(&stash[..take]);
                    stash.drain(..take);
                    return Ok((take, out));
                }
            }

            let want = buf.len().max(self.capacity);
            let (got, tmp) = self.inner.read_owned(vec![0u8; want]).await?;
            let got = got.min(tmp.len());
            let mut stash = self.stash.lock().expect("buf reader stash lock poisoned");
            let mut out = buf;
            let take = got.min(out.len());
            out[..take].copy_from_slice(&tmp[..take]);
            if got > take {
                stash.extend_from_slice(&tmp[take..got]);
            }
            Ok((take, out))
        }
    }

    pub struct BufWriter<W> {
        inner: W,
        capacity: usize,
        pending: Mutex<Vec<u8>>,
    }

    impl<W> BufWriter<W> {
        pub fn new(inner: W) -> Self {
            Self {
                inner,
                capacity: 8 * 1024,
                pending: Mutex::new(Vec::new()),
            }
        }

        pub fn with_capacity(inner: W, capacity: usize) -> Self {
            Self {
                inner,
                capacity: capacity.max(1),
                pending: Mutex::new(Vec::new()),
            }
        }

        pub async fn flush(&self) -> io::Result<()>
        where
            W: AsyncWrite,
        {
            let payload = {
                let mut pending = self.pending.lock().expect("buf writer lock poisoned");
                if pending.is_empty() {
                    return Ok(());
                }
                std::mem::take(&mut *pending)
            };
            let _ = self.inner.write_all_owned(payload).await?;
            Ok(())
        }
    }

    impl<W> AsyncWrite for BufWriter<W>
    where
        W: AsyncWrite,
    {
        async fn write_owned(&self, buf: Vec<u8>) -> io::Result<(usize, Vec<u8>)> {
            let input_len = buf.len();
            let flush_payload = {
                let mut pending = self.pending.lock().expect("buf writer lock poisoned");
                pending.extend_from_slice(&buf);
                if pending.len() >= self.capacity {
                    Some(std::mem::take(&mut *pending))
                } else {
                    None
                }
            };
            if let Some(payload) = flush_payload {
                let _ = self.inner.write_all_owned(payload).await?;
            }
            Ok((input_len, buf))
        }
    }

    pub mod framed {
        use super::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
        use std::io;

        pub struct LengthDelimited<R, W> {
            reader: R,
            writer: W,
        }

        impl<R, W> LengthDelimited<R, W>
        where
            R: AsyncRead,
            W: AsyncWrite,
        {
            pub fn new(reader: R, writer: W) -> Self {
                Self { reader, writer }
            }

            pub async fn write_frame(&mut self, payload: Vec<u8>) -> io::Result<()> {
                let mut framed = Vec::with_capacity(4 + payload.len());
                let len = payload.len() as u32;
                framed.extend_from_slice(&len.to_be_bytes());
                framed.extend_from_slice(&payload);
                let _ = self.writer.write_all_owned(framed).await?;
                Ok(())
            }

            pub async fn read_frame(&mut self) -> io::Result<Vec<u8>> {
                let len_buf = self.reader.read_exact_owned(vec![0u8; 4]).await?;
                let frame_len =
                    u32::from_be_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;
                self.reader.read_exact_owned(vec![0u8; frame_len]).await
            }
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
pub mod extension {
    pub mod fs {
        use super::super::{RuntimeError, RuntimeHandle, ShardId, UringCqe, UringNativeAny};
        use io_uring::{opcode, types};
        use std::ffi::CString;
        use std::io;
        use std::mem::MaybeUninit;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::{DirEntryExt, FileTypeExt};
        use std::path::Path;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct StatxMetadata {
            pub mask: u32,
            pub mode: u16,
            pub ino: u64,
            pub nlink: u32,
            pub uid: u32,
            pub gid: u32,
            pub size: u64,
            pub blocks: u64,
            pub blksize: u32,
            pub dev: u64,
            pub rdev: u64,
            pub attributes: u64,
            pub attributes_mask: u64,
            pub atime_sec: i64,
            pub mtime_sec: i64,
            pub ctime_sec: i64,
            pub btime_sec: i64,
        }

        impl StatxMetadata {
            fn from_raw(raw: libc::statx) -> Self {
                Self {
                    mask: raw.stx_mask,
                    mode: raw.stx_mode,
                    ino: raw.stx_ino,
                    nlink: raw.stx_nlink,
                    uid: raw.stx_uid,
                    gid: raw.stx_gid,
                    size: raw.stx_size,
                    blocks: raw.stx_blocks,
                    blksize: raw.stx_blksize,
                    dev: pack_statx_dev(raw.stx_dev_major, raw.stx_dev_minor),
                    rdev: pack_statx_dev(raw.stx_rdev_major, raw.stx_rdev_minor),
                    attributes: raw.stx_attributes,
                    attributes_mask: raw.stx_attributes_mask,
                    atime_sec: raw.stx_atime.tv_sec,
                    mtime_sec: raw.stx_mtime.tv_sec,
                    ctime_sec: raw.stx_ctime.tv_sec,
                    btime_sec: raw.stx_btime.tv_sec,
                }
            }

            fn from_metadata(meta: std::fs::Metadata) -> Self {
                Self {
                    mask: 0,
                    mode: (meta.mode() & 0o7777) as u16,
                    ino: meta.ino(),
                    nlink: u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
                    uid: meta.uid(),
                    gid: meta.gid(),
                    size: meta.len(),
                    blocks: meta.blocks(),
                    blksize: u32::try_from(meta.blksize()).unwrap_or(u32::MAX),
                    dev: meta.dev(),
                    rdev: meta.rdev(),
                    attributes: 0,
                    attributes_mask: 0,
                    atime_sec: meta.atime(),
                    mtime_sec: meta.mtime(),
                    ctime_sec: meta.ctime(),
                    btime_sec: 0,
                }
            }

            pub fn is_dir(&self) -> bool {
                (self.mode & libc::S_IFMT as u16) == libc::S_IFDIR as u16
            }

            pub fn is_file(&self) -> bool {
                (self.mode & libc::S_IFMT as u16) == libc::S_IFREG as u16
            }

            pub fn is_symlink(&self) -> bool {
                (self.mode & libc::S_IFMT as u16) == libc::S_IFLNK as u16
            }
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DirEntryType {
            File,
            Directory,
            Symlink,
            BlockDevice,
            CharDevice,
            Fifo,
            Socket,
            Unknown,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct DirEntry {
            pub file_name: String,
            pub inode: u64,
            pub offset: i64,
            pub entry_type: DirEntryType,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct StatxOptions {
            pub flags: i32,
            pub mask: u32,
        }

        impl Default for StatxOptions {
            fn default() -> Self {
                Self {
                    flags: libc::AT_STATX_SYNC_AS_STAT,
                    mask: libc::STATX_BASIC_STATS,
                }
            }
        }

        impl StatxOptions {
            pub fn flags(mut self, flags: i32) -> Self {
                self.flags = flags;
                self
            }

            pub fn mask(mut self, mask: u32) -> Self {
                self.mask = mask;
                self
            }
        }

        pub async fn statx(
            native: &UringNativeAny,
            path: impl AsRef<Path>,
        ) -> io::Result<StatxMetadata> {
            statx_on_shard(
                native,
                native.select_shard(None).map_err(runtime_error_to_io)?,
                path,
                StatxOptions::default(),
            )
            .await
        }

        pub async fn statx_on_shard(
            native: &UringNativeAny,
            shard: ShardId,
            path: impl AsRef<Path>,
            options: StatxOptions,
        ) -> io::Result<StatxMetadata> {
            let c_path = super::super::path_to_cstring_for_native_ops(path.as_ref())?;
            let state = StatxState {
                path: c_path,
                statx: MaybeUninit::zeroed(),
                options,
            };

            let result = unsafe {
                native
                    .submit_unsafe_on_shard(
                        shard,
                        state,
                        |state| {
                            Ok(opcode::Statx::new(
                                types::Fd(libc::AT_FDCWD),
                                state.path.as_ptr(),
                                state.statx.as_mut_ptr().cast::<types::statx>(),
                            )
                            .flags(state.options.flags)
                            .mask(state.options.mask)
                            .build())
                        },
                        |state, cqe| {
                            cqe_to_io_result(cqe)?;
                            // Kernel writes the output struct before CQE completion.
                            let raw = state.statx.assume_init();
                            Ok(StatxMetadata::from_raw(raw))
                        },
                    )
                    .await
            };
            result
        }

        pub async fn statx_or_metadata(
            handle: RuntimeHandle,
            path: impl AsRef<Path>,
        ) -> io::Result<StatxMetadata> {
            let path = path.as_ref().to_path_buf();
            let native = handle.uring_native_unbound().map_err(runtime_error_to_io)?;
            match statx(&native, &path).await {
                Ok(meta) => Ok(meta),
                Err(err) if is_unsupported_native_statx(&err) => {
                    let path_for_blocking = path.clone();
                    let join = handle
                        .spawn_blocking(move || std::fs::metadata(path_for_blocking))
                        .map_err(runtime_error_to_io)?;
                    let metadata = join.await.map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "blocking metadata task canceled")
                    })??;
                    Ok(StatxMetadata::from_metadata(metadata))
                }
                Err(err) => Err(err),
            }
        }

        pub async fn read_dir_entries(
            handle: RuntimeHandle,
            path: impl AsRef<Path>,
        ) -> io::Result<Vec<DirEntry>> {
            let path = path.as_ref().to_path_buf();
            let join = handle
                .spawn_blocking(move || read_dir_entries_blocking(&path))
                .map_err(runtime_error_to_io)?;
            join.await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "blocking directory enumeration task canceled",
                )
            })?
        }

        #[derive(Debug)]
        struct StatxState {
            path: CString,
            statx: MaybeUninit<libc::statx>,
            options: StatxOptions,
        }

        fn read_dir_entries_blocking(path: &Path) -> io::Result<Vec<DirEntry>> {
            match getdents64(path) {
                Ok(mut entries) => {
                    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
                    Ok(entries)
                }
                Err(err) if is_unsupported_native_getdents(&err) => std_read_dir(path),
                Err(err) => Err(err),
            }
        }

        fn getdents64(path: &Path) -> io::Result<Vec<DirEntry>> {
            const DIRENT64_HEADER_LEN: usize = 19;
            let c_path = super::super::path_to_cstring_for_native_ops(path)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
            let raw_fd = unsafe { libc::open(c_path.as_ptr(), flags) };
            if raw_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let mut entries = Vec::new();
            let mut buffer = vec![0u8; 32 * 1024];

            loop {
                let read = unsafe {
                    libc::syscall(
                        libc::SYS_getdents64 as libc::c_long,
                        fd.as_raw_fd(),
                        buffer.as_mut_ptr().cast::<libc::c_void>(),
                        buffer.len(),
                    )
                };

                if read < 0 {
                    return Err(io::Error::last_os_error());
                }

                let read = usize::try_from(read).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "negative getdents result")
                })?;
                if read == 0 {
                    break;
                }

                let mut pos = 0usize;
                while pos < read {
                    if read - pos < DIRENT64_HEADER_LEN {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "truncated linux_dirent64 header",
                        ));
                    }

                    let record = &buffer[pos..read];
                    let reclen = u16::from_ne_bytes([record[16], record[17]]) as usize;
                    if reclen < DIRENT64_HEADER_LEN || pos + reclen > read {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid linux_dirent64 record length",
                        ));
                    }

                    let inode = u64::from_ne_bytes([
                        record[0], record[1], record[2], record[3], record[4], record[5],
                        record[6], record[7],
                    ]);
                    let offset = i64::from_ne_bytes([
                        record[8], record[9], record[10], record[11], record[12], record[13],
                        record[14], record[15],
                    ]);
                    let entry_type = dirent_type_from_raw(record[18]);

                    let name_slice = &record[DIRENT64_HEADER_LEN..reclen];
                    let nul_pos = name_slice
                        .iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(name_slice.len());
                    let name_slice = &name_slice[..nul_pos];
                    if !name_slice.is_empty() && name_slice != b"." && name_slice != b".." {
                        let file_name = std::ffi::OsStr::from_bytes(name_slice)
                            .to_string_lossy()
                            .into_owned();
                        entries.push(DirEntry {
                            file_name,
                            inode,
                            offset,
                            entry_type,
                        });
                    }

                    pos += reclen;
                }
            }

            Ok(entries)
        }

        fn std_read_dir(path: &Path) -> io::Result<Vec<DirEntry>> {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.as_os_str().to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                let file_type = entry.file_type()?;
                entries.push(DirEntry {
                    file_name: name,
                    inode: entry.ino(),
                    offset: 0,
                    entry_type: dirent_type_from_file_type(file_type),
                });
            }
            entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
            Ok(entries)
        }

        fn dirent_type_from_raw(raw_type: u8) -> DirEntryType {
            match raw_type {
                libc::DT_REG => DirEntryType::File,
                libc::DT_DIR => DirEntryType::Directory,
                libc::DT_LNK => DirEntryType::Symlink,
                libc::DT_BLK => DirEntryType::BlockDevice,
                libc::DT_CHR => DirEntryType::CharDevice,
                libc::DT_FIFO => DirEntryType::Fifo,
                libc::DT_SOCK => DirEntryType::Socket,
                _ => DirEntryType::Unknown,
            }
        }

        fn dirent_type_from_file_type(file_type: std::fs::FileType) -> DirEntryType {
            if file_type.is_file() {
                return DirEntryType::File;
            }
            if file_type.is_dir() {
                return DirEntryType::Directory;
            }
            if file_type.is_symlink() {
                return DirEntryType::Symlink;
            }
            if file_type.is_block_device() {
                return DirEntryType::BlockDevice;
            }
            if file_type.is_char_device() {
                return DirEntryType::CharDevice;
            }
            if file_type.is_fifo() {
                return DirEntryType::Fifo;
            }
            if file_type.is_socket() {
                return DirEntryType::Socket;
            }
            DirEntryType::Unknown
        }

        fn is_unsupported_native_getdents(err: &io::Error) -> bool {
            matches!(
                err.raw_os_error(),
                Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
            )
        }

        fn pack_statx_dev(major: u32, minor: u32) -> u64 {
            (u64::from(major) << 32) | u64::from(minor)
        }

        fn cqe_to_io_result(cqe: UringCqe) -> io::Result<()> {
            if cqe.result < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.result));
            }
            Ok(())
        }

        fn is_unsupported_native_statx(err: &io::Error) -> bool {
            matches!(
                err.raw_os_error(),
                Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
            )
        }

        fn runtime_error_to_io(err: RuntimeError) -> io::Error {
            match err {
                RuntimeError::InvalidConfig(msg) => {
                    io::Error::new(io::ErrorKind::InvalidInput, msg)
                }
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

fn spawn_local_on_shared<F, Fut, T>(
    shared: &Arc<RuntimeShared>,
    shard: ShardId,
    init: F,
) -> Result<JoinHandle<T>, RuntimeError>
where
    F: FnOnce(ShardCtx) -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let runtime_id = shared.runtime_id;
    shared
        .send_to(
            shard,
            Command::Spawn(Box::pin(async move {
                let local_join = {
                    let Some(ctx) =
                        ShardCtx::current().filter(|ctx| ctx.runtime_id() == runtime_id)
                    else {
                        return;
                    };
                    let fut = init(ctx.clone());
                    ctx.spawn_local(fut)
                };
                let Ok(out) = local_join.await else {
                    return;
                };
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
    if inbox
        .try_push(
            StealableTask {
                task: Box::pin(async move {
                    let out = fut.await;
                    let _ = tx.send(out);
                }),
            },
            shared.stealable_queue_capacity,
        )
        .is_err()
    {
        shared
            .stats
            .stealable_backpressure
            .fetch_add(1, Ordering::Relaxed);
        return Err(RuntimeError::Overloaded);
    }
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
    task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
}

enum StealableInbox {
    Mutex {
        queue: CachePadded<Mutex<VecDeque<StealableTask>>>,
    },
    SegQueue {
        queue: SegQueue<StealableTask>,
        len: CachePadded<AtomicUsize>,
    },
}

impl StealableInbox {
    fn try_push(&self, task: StealableTask, capacity: usize) -> Result<(), StealableTask> {
        match self {
            StealableInbox::Mutex { queue } => {
                let mut guard = queue.lock().expect("stealable queue lock poisoned");
                if guard.len() >= capacity {
                    return Err(task);
                }
                guard.push_back(task);
                Ok(())
            }
            StealableInbox::SegQueue { queue, len } => loop {
                let current = len.load(Ordering::Relaxed);
                if current >= capacity {
                    return Err(task);
                }
                if len
                    .compare_exchange_weak(
                        current,
                        current + 1,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    queue.push(task);
                    return Ok(());
                }
            },
        }
    }

    fn pop_local(&self) -> Option<StealableTask> {
        match self {
            StealableInbox::Mutex { queue } => queue
                .lock()
                .expect("stealable queue lock poisoned")
                .pop_front(),
            StealableInbox::SegQueue { queue, len } => {
                let task = queue.pop();
                if task.is_some() {
                    len.fetch_sub(1, Ordering::AcqRel);
                }
                task
            }
        }
    }

    fn pop_stolen(&self) -> Option<StealableTask> {
        match self {
            StealableInbox::Mutex { queue } => queue
                .lock()
                .expect("stealable queue lock poisoned")
                .pop_back(),
            StealableInbox::SegQueue { queue, len } => {
                let task = queue.pop();
                if task.is_some() {
                    len.fetch_sub(1, Ordering::AcqRel);
                }
                task
            }
        }
    }

    fn len_estimate(&self) -> usize {
        match self {
            StealableInbox::Mutex { queue } => {
                queue.lock().expect("stealable queue lock poisoned").len()
            }
            StealableInbox::SegQueue { len, .. } => len.load(Ordering::Relaxed),
        }
    }
}

type StealableInboxes = Arc<Vec<Arc<StealableInbox>>>;
type StealableWakeFlags = Arc<Vec<CachePadded<AtomicBool>>>;

#[derive(Debug, Clone, Copy)]
struct StealPolicyConfig {
    victim_stride: usize,
    victim_probe_count: usize,
    batch_size: usize,
    locality_margin: usize,
    fail_cost: usize,
    backoff_min: usize,
    backoff_max: usize,
}

#[derive(Default)]
struct StealLoopState {
    failed_streak: usize,
    cooldown_remaining: usize,
}

struct RuntimeStatsInner {
    shard_command_depths: Vec<CachePadded<AtomicUsize>>,
    pending_native_ops_by_shard: Vec<CachePadded<AtomicUsize>>,
    native_any_envelope_submitted: AtomicU64,
    native_any_local_fastpath_submitted: AtomicU64,
    native_any_local_direct_submitted: AtomicU64,
    spawn_pinned_submitted: AtomicU64,
    spawn_stealable_submitted: AtomicU64,
    stealable_executed: AtomicU64,
    stealable_stolen: AtomicU64,
    stealable_backpressure: AtomicU64,
    stealable_local_hits: AtomicU64,
    steal_attempts: AtomicU64,
    steal_scans: AtomicU64,
    steal_success: AtomicU64,
    steal_skipped_backoff: AtomicU64,
    steal_skipped_locality: AtomicU64,
    steal_failed_streak_max: AtomicU64,
    stealable_wake_sent: AtomicU64,
    stealable_wake_coalesced: AtomicU64,
    steal_policy: StealPolicyConfig,
    ring_msgs_submitted: AtomicU64,
    ring_msgs_completed: AtomicU64,
    ring_msgs_failed: AtomicU64,
    ring_msgs_backpressure: AtomicU64,
    native_affinity_violations: AtomicU64,
    pending_native_ops: AtomicU64,
}

impl RuntimeStatsInner {
    fn new(shards: usize, steal_policy: StealPolicyConfig) -> Self {
        let mut shard_command_depths = Vec::with_capacity(shards);
        let mut pending_native_ops_by_shard = Vec::with_capacity(shards);
        for _ in 0..shards {
            shard_command_depths.push(CachePadded::new(AtomicUsize::new(0)));
            pending_native_ops_by_shard.push(CachePadded::new(AtomicUsize::new(0)));
        }

        Self {
            shard_command_depths,
            pending_native_ops_by_shard,
            native_any_envelope_submitted: AtomicU64::new(0),
            native_any_local_fastpath_submitted: AtomicU64::new(0),
            native_any_local_direct_submitted: AtomicU64::new(0),
            spawn_pinned_submitted: AtomicU64::new(0),
            spawn_stealable_submitted: AtomicU64::new(0),
            stealable_executed: AtomicU64::new(0),
            stealable_stolen: AtomicU64::new(0),
            stealable_backpressure: AtomicU64::new(0),
            stealable_local_hits: AtomicU64::new(0),
            steal_attempts: AtomicU64::new(0),
            steal_scans: AtomicU64::new(0),
            steal_success: AtomicU64::new(0),
            steal_skipped_backoff: AtomicU64::new(0),
            steal_skipped_locality: AtomicU64::new(0),
            steal_failed_streak_max: AtomicU64::new(0),
            stealable_wake_sent: AtomicU64::new(0),
            stealable_wake_coalesced: AtomicU64::new(0),
            steal_policy,
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
            native_any_local_direct_submitted: self
                .native_any_local_direct_submitted
                .load(Ordering::Relaxed),
            spawn_pinned_submitted: self.spawn_pinned_submitted.load(Ordering::Relaxed),
            spawn_stealable_submitted: self.spawn_stealable_submitted.load(Ordering::Relaxed),
            stealable_executed: self.stealable_executed.load(Ordering::Relaxed),
            stealable_stolen: self.stealable_stolen.load(Ordering::Relaxed),
            stealable_backpressure: self.stealable_backpressure.load(Ordering::Relaxed),
            stealable_local_hits: self.stealable_local_hits.load(Ordering::Relaxed),
            steal_attempts: self.steal_attempts.load(Ordering::Relaxed),
            steal_scans: self.steal_scans.load(Ordering::Relaxed),
            steal_success: self.steal_success.load(Ordering::Relaxed),
            steal_skipped_backoff: self.steal_skipped_backoff.load(Ordering::Relaxed),
            steal_skipped_locality: self.steal_skipped_locality.load(Ordering::Relaxed),
            steal_failed_streak_max: self.steal_failed_streak_max.load(Ordering::Relaxed),
            stealable_wake_sent: self.stealable_wake_sent.load(Ordering::Relaxed),
            stealable_wake_coalesced: self.stealable_wake_coalesced.load(Ordering::Relaxed),
            steal_victim_stride: self.steal_policy.victim_stride,
            steal_victim_probe_count: self.steal_policy.victim_probe_count,
            steal_batch_size: self.steal_policy.batch_size,
            steal_locality_margin: self.steal_policy.locality_margin,
            steal_fail_cost: self.steal_policy.fail_cost,
            steal_backoff_min: self.steal_policy.backoff_min,
            steal_backoff_max: self.steal_policy.backoff_max,
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

    fn observe_failed_streak(&self, failed_streak: usize) {
        let failed_streak = failed_streak as u64;
        let _ = self.steal_failed_streak_max.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |cur| Some(cur.max(failed_streak)),
        );
    }
}

#[derive(Clone)]
struct RuntimeShared {
    runtime_id: u64,
    backend: BackendKind,
    command_txs: Vec<Sender<Command>>,
    stealable_inboxes: StealableInboxes,
    stealable_wake_flags: StealableWakeFlags,
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
        let Some(flag) = self.stealable_wake_flags.get(usize::from(target)) else {
            return;
        };
        if flag.swap(true, Ordering::AcqRel) {
            self.stats
                .stealable_wake_coalesced
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        if let Some(ctx) = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.runtime_id) {
            if ctx.shard_id() == target {
                flag.store(false, Ordering::Release);
                return;
            }
            if ctx.enqueue_local_stealable_wake(target).is_ok() {
                self.stats
                    .stealable_wake_sent
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                flag.store(false, Ordering::Release);
            }
            return;
        }

        if self.send_to(target, Command::StealableWake).is_err() {
            flag.store(false, Ordering::Release);
        } else {
            self.stats
                .stealable_wake_sent
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone)]
/// Handle to a specific target shard for message sending.
pub struct RemoteShard {
    id: ShardId,
    shared: Arc<RuntimeShared>,
}

impl RemoteShard {
    /// Returns the destination shard id.
    pub fn id(&self) -> ShardId {
        self.id
    }

    /// Sends one raw `(tag, val)` message and returns an acknowledgement ticket.
    pub fn send_raw(&self, tag: u16, val: u32) -> Result<SendTicket, SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.send_raw_inner(tag, val, Some(ack_tx))?;
        Ok(SendTicket { rx: Some(ack_rx) })
    }

    /// Sends one raw `(tag, val)` message without waiting for acknowledgement.
    pub fn send_raw_nowait(&self, tag: u16, val: u32) -> Result<(), SendError> {
        self.send_raw_inner(tag, val, None)
    }

    /// Sends a batch of raw messages without acknowledgement tickets.
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

    /// Attempts direct in-ring submission for one message when called on-shard.
    ///
    /// Falls back to queued path when called outside runtime worker context.
    pub fn send_raw_direct_nowait(&self, tag: u16, val: u32) -> Result<(), SendError> {
        self.send_many_raw_direct_nowait(std::iter::once((tag, val)))
    }

    /// Attempts direct in-ring submission for a batch when called on-shard.
    ///
    /// Falls back to queued path when called outside runtime worker context.
    pub fn send_many_raw_direct_nowait<I>(&self, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        let current = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.shared.runtime_id);
        if let Some(ctx) = current {
            return ctx.enqueue_local_send_many_direct(self.id, msgs);
        }

        // Outside runtime threads we cannot submit to io_uring directly.
        self.send_many_raw_nowait(msgs)
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

    /// Sends a typed message and returns an acknowledgement ticket.
    pub fn send<M: RingMsg>(&self, msg: M) -> Result<SendTicket, SendError> {
        let (tag, val) = msg.encode();
        self.send_raw(tag, val)
    }

    /// Sends a typed message without waiting for acknowledgement.
    pub fn send_nowait<M: RingMsg>(&self, msg: M) -> Result<(), SendError> {
        let (tag, val) = msg.encode();
        self.send_raw_nowait(tag, val)
    }

    /// Sends a batch of typed messages without acknowledgement.
    pub fn send_many_nowait<M, I>(&self, msgs: I) -> Result<(), SendError>
    where
        M: RingMsg,
        I: IntoIterator<Item = M>,
    {
        self.send_many_raw_nowait(msgs.into_iter().map(|msg| msg.encode()))
    }

    /// Flushes pending local sends and returns a completion ticket.
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
/// Shard-local execution context available on worker threads.
pub struct ShardCtx {
    inner: Rc<ShardCtxInner>,
}

struct ShardCtxInner {
    runtime_id: u64,
    shard_id: ShardId,
    event_state: Arc<EventState>,
    hot_event_state: Arc<EventState>,
    hot_counter_state: Arc<HotCounterState>,
    spawner: LocalSpawner,
    remotes: Vec<RemoteShard>,
    local_commands: Rc<RefCell<VecDeque<LocalCommand>>>,
}

thread_local! {
    static CURRENT_SHARD: RefCell<Option<ShardCtx>> = const { RefCell::new(None) };
}

impl ShardCtx {
    /// Returns the current shard context when called from a runtime worker.
    pub fn current() -> Option<Self> {
        CURRENT_SHARD.with(|ctx| ctx.borrow().clone())
    }

    /// Returns the current shard id.
    pub fn shard_id(&self) -> ShardId {
        self.inner.shard_id
    }

    fn runtime_id(&self) -> u64 {
        self.inner.runtime_id
    }

    /// Returns a remote handle for `target`.
    pub fn remote(&self, target: ShardId) -> Option<RemoteShard> {
        self.inner.remotes.get(usize::from(target)).cloned()
    }

    /// Enqueues one raw message to `target` (no acknowledgement ticket).
    pub fn send_raw_nowait(&self, target: ShardId, tag: u16, val: u32) -> Result<(), SendError> {
        self.enqueue_local_send(target, tag, val, None)
    }

    /// Enqueues a batch of raw messages to `target`.
    pub fn send_many_raw_nowait<I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        self.enqueue_local_send_many(target, msgs)
    }

    /// Enqueues one raw message via direct in-ring path.
    pub fn send_raw_direct_nowait(
        &self,
        target: ShardId,
        tag: u16,
        val: u32,
    ) -> Result<(), SendError> {
        self.enqueue_local_send_many_direct(target, std::iter::once((tag, val)))
    }

    /// Enqueues a batch of raw messages via direct in-ring path.
    pub fn send_many_raw_direct_nowait<I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        I: IntoIterator<Item = (u16, u32)>,
    {
        self.enqueue_local_send_many_direct(target, msgs)
    }

    /// Enqueues a batch of typed messages to `target`.
    pub fn send_many_nowait<M, I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
    where
        M: RingMsg,
        I: IntoIterator<Item = M>,
    {
        self.enqueue_local_send_many(target, msgs.into_iter().map(|msg| msg.encode()))
    }

    /// Sends one raw message to `target` and returns an acknowledgement ticket.
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

    fn enqueue_local_send_many_direct<I>(&self, target: ShardId, msgs: I) -> Result<(), SendError>
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
            .push_back(LocalCommand::SubmitRingMsgDirectBatch { target, messages });
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

    /// Flushes queued local commands and returns a completion ticket.
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

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    pub async fn native_sleep(&self, duration: Duration) -> std::io::Result<()> {
        let reply_rx = self.enqueue_native_sleep(duration);
        reply_rx.await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native timeout response channel closed",
            ))
        })
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn enqueue_native_sleep(&self, duration: Duration) -> oneshot::Receiver<std::io::Result<()>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::SubmitNativeTimeout {
                origin_shard: self.inner.shard_id,
                duration,
                reply: reply_tx,
            });
        reply_rx
    }

    /// Spawns a shard-local (`!Send`) task on this shard.
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

    /// Returns a future for the next regular event.
    pub fn next_event(&self) -> NextEvent {
        NextEvent {
            state: self.inner.event_state.clone(),
        }
    }

    /// Returns a future for the next hot-lane event.
    pub fn next_hot_event(&self) -> NextEvent {
        NextEvent {
            state: self.inner.hot_event_state.clone(),
        }
    }

    /// Returns a future that resolves with accumulated hot count for `tag`.
    pub fn next_hot_count(&self, tag: u16) -> NextHotCount {
        NextHotCount {
            state: self.inner.hot_counter_state.clone(),
            tag,
        }
    }

    /// Tries to take the current hot counter value for `tag` without waiting.
    pub fn try_take_hot_count(&self, tag: u16) -> Option<u64> {
        self.inner.hot_counter_state.try_take(tag)
    }
}

#[derive(Debug)]
/// Runtime construction/spawn/control error.
pub enum RuntimeError {
    /// Invalid builder configuration.
    InvalidConfig(&'static str),
    /// Failed to spawn worker/helper thread.
    ThreadSpawn(std::io::Error),
    /// Requested shard id does not exist.
    InvalidShard(ShardId),
    /// Runtime/channel is closed.
    Closed,
    /// Operation was rejected due to backpressure.
    Overloaded,
    /// Requested operation is not supported by active backend.
    UnsupportedBackend(&'static str),
    #[cfg(target_os = "linux")]
    /// Failed to initialize io_uring resources.
    IoUringInit(std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Message send failure.
pub enum SendError {
    /// Runtime/channel is closed.
    Closed,
    /// Queue backpressure prevented enqueue.
    Backpressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Join handle completion failure.
pub enum JoinError {
    /// Task was canceled or its completion channel was dropped.
    Canceled,
}

/// Future returned by runtime spawn APIs.
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

/// Future returned by shard-local spawn APIs.
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

/// Completion ticket for send/flush operations.
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

/// Future that yields the next event from a shard event queue.
pub struct NextEvent {
    state: Arc<EventState>,
}

impl Future for NextEvent {
    type Output = Event;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.poll_next(cx)
    }
}

/// Future that yields the next coalesced hot-counter value for a tag.
pub struct NextHotCount {
    state: Arc<HotCounterState>,
    tag: u16,
}

impl Future for NextHotCount {
    type Output = u64;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.state.poll_take(this.tag, cx)
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
    SubmitRingMsgDirectBatch {
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
        offset: usize,
        reply: NativeBufReply,
    },
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeSendOwned {
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
        reply: NativeBufReply,
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
    SubmitNativeTimeout {
        origin_shard: ShardId,
        duration: Duration,
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
    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    SubmitNativeUnsafe {
        origin_shard: ShardId,
        op: Box<dyn NativeUnsafeOpDriver>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
trait NativeUnsafeOpDriver: Send {
    fn build_entry(&mut self) -> std::io::Result<io_uring::squeue::Entry>;
    fn complete(self: Box<Self>, cqe: UringCqe);
    fn fail(self: Box<Self>, err: std::io::Error);
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct NativeUnsafeOpEnvelope<S, T, B, C>
where
    S: Send + 'static,
    T: Send + 'static,
    B: FnOnce(&mut S) -> std::io::Result<io_uring::squeue::Entry> + Send + 'static,
    C: FnOnce(S, UringCqe) -> std::io::Result<T> + Send + 'static,
{
    state: Option<S>,
    build: Option<B>,
    complete: Option<C>,
    reply: Option<oneshot::Sender<std::io::Result<T>>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl<S, T, B, C> NativeUnsafeOpEnvelope<S, T, B, C>
where
    S: Send + 'static,
    T: Send + 'static,
    B: FnOnce(&mut S) -> std::io::Result<io_uring::squeue::Entry> + Send + 'static,
    C: FnOnce(S, UringCqe) -> std::io::Result<T> + Send + 'static,
{
    fn new(state: S, build: B, complete: C, reply: oneshot::Sender<std::io::Result<T>>) -> Self {
        Self {
            state: Some(state),
            build: Some(build),
            complete: Some(complete),
            reply: Some(reply),
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl<S, T, B, C> NativeUnsafeOpDriver for NativeUnsafeOpEnvelope<S, T, B, C>
where
    S: Send + 'static,
    T: Send + 'static,
    B: FnOnce(&mut S) -> std::io::Result<io_uring::squeue::Entry> + Send + 'static,
    C: FnOnce(S, UringCqe) -> std::io::Result<T> + Send + 'static,
{
    fn build_entry(&mut self) -> std::io::Result<io_uring::squeue::Entry> {
        let build = self
            .build
            .take()
            .expect("native unsafe op build closure missing");
        let state = self
            .state
            .as_mut()
            .expect("native unsafe op state missing before build");
        build(state)
    }

    fn complete(mut self: Box<Self>, cqe: UringCqe) {
        let complete = self
            .complete
            .take()
            .expect("native unsafe op completion closure missing");
        let state = self
            .state
            .take()
            .expect("native unsafe op state missing on completion");
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(complete(state, cqe));
        }
    }

    fn fail(mut self: Box<Self>, err: std::io::Error) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(err));
        }
    }
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
        offset: usize,
        reply: oneshot::Sender<std::io::Result<(usize, Vec<u8>)>>,
    },
    SendOwned {
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
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
    Timeout {
        duration: Duration,
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
    Unsafe {
        op: Box<dyn NativeUnsafeOpDriver>,
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
            Self::RecvOwned {
                fd,
                buf,
                offset,
                reply,
            } => LocalCommand::SubmitNativeRecvOwned {
                origin_shard,
                fd,
                buf,
                offset,
                reply: NativeBufReply::oneshot(reply),
            },
            Self::SendOwned {
                fd,
                buf,
                offset,
                reply,
            } => LocalCommand::SubmitNativeSendOwned {
                origin_shard,
                fd,
                buf,
                offset,
                reply: NativeBufReply::oneshot(reply),
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
            Self::Timeout { duration, reply } => LocalCommand::SubmitNativeTimeout {
                origin_shard,
                duration,
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
            Self::Unsafe { op } => LocalCommand::SubmitNativeUnsafe { origin_shard, op },
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
            Self::Timeout { reply, .. } => {
                let _ = reply.send(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unbound timeout command channel closed",
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
            Self::Unsafe { op } => {
                op.fail(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "native unsafe op command channel closed",
                ));
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
    queue: SegQueue<Event>,
    waiters: Mutex<Vec<Waker>>,
}

impl EventState {
    fn push(&self, event: Event) {
        self.queue.push(event);
        self.wake_waiters();
    }

    fn push_many<I>(&self, events: I)
    where
        I: IntoIterator<Item = Event>,
    {
        let mut count = 0usize;
        for event in events {
            self.queue.push(event);
            count += 1;
        }
        if count == 0 {
            return;
        }
        self.wake_waiters();
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Event> {
        if let Some(event) = self.queue.pop() {
            return Poll::Ready(event);
        }

        {
            let mut waiters = self.waiters.lock().expect("event lock poisoned");
            if let Some(event) = self.queue.pop() {
                return Poll::Ready(event);
            }
            if !waiters.iter().any(|w| w.will_wake(cx.waker())) {
                waiters.push(cx.waker().clone());
            }
        }

        if let Some(event) = self.queue.pop() {
            return Poll::Ready(event);
        }

        Poll::Pending
    }

    fn wake_waiters(&self) {
        let waiters = {
            let mut waiters = self.waiters.lock().expect("event lock poisoned");
            std::mem::take(&mut *waiters)
        };

        for w in waiters {
            w.wake();
        }
    }
}

struct HotCounterState {
    wake_threshold: u64,
    inner: Mutex<HotCounterInner>,
}

#[derive(Default)]
struct HotCounterInner {
    counts: HashMap<u16, u64>,
    waiters: HashMap<u16, Vec<Waker>>,
}

impl HotCounterState {
    fn new(wake_threshold: u64) -> Self {
        Self {
            wake_threshold: wake_threshold.max(1),
            inner: Mutex::new(HotCounterInner::default()),
        }
    }

    fn add_many<I>(&self, updates: I)
    where
        I: IntoIterator<Item = (u16, u64)>,
    {
        let mut wake = Vec::new();
        {
            let mut inner = self.inner.lock().expect("hot counter lock poisoned");
            for (tag, delta) in updates {
                if delta == 0 {
                    continue;
                }
                let before = inner.counts.get(&tag).copied().unwrap_or(0);
                let after = before.saturating_add(delta);
                inner.counts.insert(tag, after);

                let should_wake =
                    before == 0 || (before < self.wake_threshold && after >= self.wake_threshold);
                if should_wake {
                    if let Some(waiters) = inner.waiters.remove(&tag) {
                        wake.extend(waiters);
                    }
                }
            }
        }
        for w in wake {
            w.wake();
        }
    }

    fn poll_take(&self, tag: u16, cx: &mut Context<'_>) -> Poll<u64> {
        let mut inner = self.inner.lock().expect("hot counter lock poisoned");
        if let Some(value) = inner.counts.remove(&tag) {
            if value > 0 {
                return Poll::Ready(value);
            }
        }

        let waiters = inner.waiters.entry(tag).or_default();
        if !waiters.iter().any(|w| w.will_wake(cx.waker())) {
            waiters.push(cx.waker().clone());
        }
        Poll::Pending
    }

    fn try_take(&self, tag: u16) -> Option<u64> {
        let mut inner = self.inner.lock().expect("hot counter lock poisoned");
        inner.counts.remove(&tag).filter(|value| *value > 0)
    }
}

enum ShardBackend {
    #[cfg(target_os = "linux")]
    IoUring(IoUringDriver),
    #[cfg(not(target_os = "linux"))]
    Unsupported,
}

impl ShardBackend {
    #[cfg(target_os = "linux")]
    fn driver_mut(&mut self) -> &mut IoUringDriver {
        match self {
            Self::IoUring(driver) => driver,
        }
    }

    fn prefers_busy_poll(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            return true;
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn poll(
        &mut self,
        event_state: &EventState,
        hot_event_state: &EventState,
        hot_counter_state: &HotCounterState,
        hot_msg_tags: &[bool],
        coalesced_hot_msg_tags: &[bool],
    ) {
        #[cfg(target_os = "linux")]
        {
            self.driver_mut().reap(
                event_state,
                hot_event_state,
                hot_counter_state,
                hot_msg_tags,
                coalesced_hot_msg_tags,
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = event_state;
            let _ = hot_event_state;
            let _ = hot_counter_state;
            let _ = hot_msg_tags;
            let _ = coalesced_hot_msg_tags;
        }
    }

    fn submit_ring_msg(
        &mut self,
        _from: ShardId,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: Option<oneshot::Sender<Result<(), SendError>>>,
        _command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        stats.ring_msgs_submitted.fetch_add(1, Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        {
            if self
                .driver_mut()
                .submit_ring_msg(target, tag, val, ack)
                .is_err()
            {
                stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
                // submit_ring_msg already completed ack with an error
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
            if let Some(ack) = ack {
                let _ = ack.send(Err(SendError::Closed));
            }
        }
    }

    fn submit_ring_msg_batch(
        &mut self,
        _from: ShardId,
        target: ShardId,
        messages: Vec<(u16, u32)>,
        _command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        if messages.is_empty() {
            return;
        }

        stats
            .ring_msgs_submitted
            .fetch_add(messages.len() as u64, Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        {
            let (accepted, err) = self.driver_mut().submit_ring_msg_batch(target, &messages);
            if err.is_some() {
                let failed = messages.len().saturating_sub(accepted);
                if failed > 0 {
                    stats
                        .ring_msgs_failed
                        .fetch_add(failed as u64, Ordering::Relaxed);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target;
            stats
                .ring_msgs_failed
                .fetch_add(messages.len() as u64, Ordering::Relaxed);
        }
    }

    fn submit_ring_msg_direct_batch(
        &mut self,
        _from: ShardId,
        target: ShardId,
        messages: Vec<(u16, u32)>,
        _command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        if messages.is_empty() {
            return;
        }

        stats
            .ring_msgs_submitted
            .fetch_add(messages.len() as u64, Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        {
            let (accepted, err) = self
                .driver_mut()
                .submit_ring_msg_direct_batch(target, &messages);
            if err.is_some() {
                let failed = messages.len().saturating_sub(accepted);
                if failed > 0 {
                    stats
                        .ring_msgs_failed
                        .fetch_add(failed as u64, Ordering::Relaxed);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target;
            stats
                .ring_msgs_failed
                .fetch_add(messages.len() as u64, Ordering::Relaxed);
        }
    }

    fn submit_stealable_wake(
        &mut self,
        target: ShardId,
        _command_txs: &[Sender<Command>],
        stats: &RuntimeStatsInner,
    ) {
        #[cfg(target_os = "linux")]
        {
            stats.ring_msgs_submitted.fetch_add(1, Ordering::Relaxed);
            if self.driver_mut().submit_stealable_wake(target).is_err() {
                stats.ring_msgs_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target;
            let _ = stats;
        }
    }

    fn flush(&mut self, ack: oneshot::Sender<Result<(), SendError>>) {
        #[cfg(target_os = "linux")]
        {
            if self.driver_mut().submit_flush(ack).is_err() {
                // submit_flush already completed ack with an error
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = ack.send(Err(SendError::Closed));
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

        let _ = self.driver_mut().submit_native_read(fd, offset, len, reply);
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

        let _ = self
            .driver_mut()
            .submit_native_read_owned(fd, offset, buf, reply);
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

        let _ = self
            .driver_mut()
            .submit_native_write(fd, offset, buf, reply);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_recv(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
        reply: NativeBufReply,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring recv violated ring affinity",
            )));
            return;
        }

        let _ = self.driver_mut().submit_native_recv(fd, buf, offset, reply);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_send(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        fd: RawFd,
        buf: Vec<u8>,
        offset: usize,
        reply: NativeBufReply,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring send violated ring affinity",
            )));
            return;
        }

        let _ = self.driver_mut().submit_native_send(fd, buf, offset, reply);
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

        let _ = self
            .driver_mut()
            .submit_native_send_batch(fd, bufs, window, reply);
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

        let _ = self.driver_mut().submit_native_recv_multishot(
            fd,
            buffer_len,
            buffer_count,
            bytes_target,
            reply,
        );
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

        let _ = self.driver_mut().submit_native_fsync(fd, reply);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_timeout(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        duration: Duration,
        reply: oneshot::Sender<std::io::Result<()>>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring timeout violated ring affinity",
            )));
            return;
        }

        let _ = self.driver_mut().submit_native_timeout(duration, reply);
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

        let _ = self
            .driver_mut()
            .submit_native_openat(path, flags, mode, reply);
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

        let _ = self
            .driver_mut()
            .submit_native_connect(socket, addr, addr_len, reply);
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

        let _ = self.driver_mut().submit_native_accept(fd, reply);
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    fn submit_native_unsafe(
        &mut self,
        current_shard: ShardId,
        origin_shard: ShardId,
        op: Box<dyn NativeUnsafeOpDriver>,
        stats: &RuntimeStatsInner,
    ) {
        if current_shard != origin_shard {
            stats
                .native_affinity_violations
                .fetch_add(1, Ordering::Relaxed);
            op.fail(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "native io_uring unsafe op violated ring affinity",
            ));
            return;
        }

        let _ = self.driver_mut().submit_native_unsafe(op);
    }

    fn shutdown(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.driver_mut().shutdown();
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
        offset: usize,
        reply: NativeBufReply,
    },
    Send {
        buf: Vec<u8>,
        offset: usize,
        reply: NativeBufReply,
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
    Timeout {
        timespec: Box<types::Timespec>,
        reply: oneshot::Sender<std::io::Result<()>>,
    },
    Unsafe {
        op: Box<dyn NativeUnsafeOpDriver>,
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
    coalesced_hot_msg_tags: Arc<Vec<bool>>,
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
        coalesced_hot_msg_tags: Arc<Vec<bool>>,
        stats: Arc<RuntimeStatsInner>,
        payload_queue_capacity: usize,
    ) -> Self {
        Self {
            shard_id,
            ring,
            ring_fds,
            payload_queues,
            coalesced_hot_msg_tags,
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

    fn submit_ring_msg_direct_batch(
        &mut self,
        target: ShardId,
        messages: &[(u16, u32)],
    ) -> (usize, Option<SendError>) {
        if messages.is_empty() {
            return (0, None);
        }

        let mut accepted = 0usize;
        for &(tag, val) in messages {
            match self.submit_ring_msg_direct_nowait(target, tag, val) {
                Ok(()) => accepted += 1,
                Err(err) => {
                    return (accepted, Some(err));
                }
            }
        }
        (accepted, None)
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

    fn submit_ring_msg_direct_nowait(
        &mut self,
        target: ShardId,
        tag: u16,
        val: u32,
    ) -> Result<(), SendError> {
        let target_fd = self.target_fd(target)?;
        let payload = pack_msg_userdata(self.shard_id, tag);
        let val_i32 = i32::from_ne_bytes(val.to_ne_bytes());
        let entry = opcode::MsgRingData::new(
            types::Fd(target_fd),
            val_i32,
            payload,
            Some(MSG_RING_CQE_FLAG),
        )
        .build()
        .user_data(0)
        .flags(io_uring::squeue::Flags::SKIP_SUCCESS);
        self.push_entry(entry)?;
        self.mark_submission_pending()
    }

    fn submit_ring_msg_batch(
        &mut self,
        target: ShardId,
        messages: &[(u16, u32)],
    ) -> (usize, Option<SendError>) {
        if messages.is_empty() {
            return (0, None);
        }

        let (accepted, was_empty) = {
            let Some(per_source) = self.payload_queues.get(usize::from(target)) else {
                return (0, Some(SendError::Closed));
            };
            let Some(queue) = per_source.get(usize::from(self.shard_id)) else {
                return (0, Some(SendError::Closed));
            };

            let mut queue = queue.lock().expect("payload queue lock poisoned");
            let was_empty = queue.is_empty();
            let mut accepted = 0usize;
            for &(tag, val) in messages {
                if self.enqueue_payload_locked(&mut queue, tag, val).is_err() {
                    break;
                }
                accepted += 1;
            }
            (accepted, was_empty)
        };

        if accepted == 0 {
            self.stats
                .ring_msgs_backpressure
                .fetch_add(messages.len() as u64, Ordering::Relaxed);
            return (0, Some(SendError::Backpressure));
        }

        if was_empty && self.submit_doorbell(target).is_err() {
            self.rollback_last_payloads(target, accepted);
            return (0, Some(SendError::Closed));
        }

        if accepted < messages.len() {
            self.stats
                .ring_msgs_backpressure
                .fetch_add((messages.len() - accepted) as u64, Ordering::Relaxed);
            return (accepted, Some(SendError::Backpressure));
        }

        (accepted, None)
    }

    fn enqueue_payload(&self, target: ShardId, tag: u16, val: u32) -> Result<bool, SendError> {
        let Some(per_source) = self.payload_queues.get(usize::from(target)) else {
            return Err(SendError::Closed);
        };
        let Some(queue) = per_source.get(usize::from(self.shard_id)) else {
            return Err(SendError::Closed);
        };

        let mut queue = queue.lock().expect("payload queue lock poisoned");
        let was_empty = queue.is_empty();
        if self.enqueue_payload_locked(&mut queue, tag, val).is_err() {
            self.stats
                .ring_msgs_backpressure
                .fetch_add(1, Ordering::Relaxed);
            return Err(SendError::Backpressure);
        }
        Ok(was_empty)
    }

    fn enqueue_payload_locked(
        &self,
        queue: &mut VecDeque<DoorbellPayload>,
        tag: u16,
        val: u32,
    ) -> Result<(), SendError> {
        if !is_hot_msg_tag(self.coalesced_hot_msg_tags.as_ref(), tag) {
            if queue.len() >= self.payload_queue_capacity {
                return Err(SendError::Backpressure);
            }
            queue.push_back(DoorbellPayload { tag, val });
            return Ok(());
        }

        if let Some(last) = queue.back().copied().filter(|last| last.tag == tag) {
            let max = u64::from(u32::MAX);
            let total = u64::from(last.val).saturating_add(u64::from(val));
            let overflow = total.saturating_sub(max);
            let extra_slots = if overflow == 0 {
                0
            } else {
                ((overflow - 1) / max + 1) as usize
            };
            if queue.len().saturating_add(extra_slots) > self.payload_queue_capacity {
                return Err(SendError::Backpressure);
            }

            if let Some(last_mut) = queue.back_mut() {
                last_mut.val = total.min(max) as u32;
            }

            let mut remaining = overflow;
            while remaining > 0 {
                let chunk = remaining.min(max) as u32;
                queue.push_back(DoorbellPayload { tag, val: chunk });
                remaining -= u64::from(chunk);
            }
            return Ok(());
        }

        if queue.len() >= self.payload_queue_capacity {
            return Err(SendError::Backpressure);
        }
        queue.push_back(DoorbellPayload { tag, val });
        Ok(())
    }

    fn rollback_last_payload(&self, target: ShardId) {
        self.rollback_last_payloads(target, 1);
    }

    fn rollback_last_payloads(&self, target: ShardId, count: usize) {
        let Some(per_source) = self.payload_queues.get(usize::from(target)) else {
            return;
        };
        let Some(queue) = per_source.get(usize::from(self.shard_id)) else {
            return;
        };

        let mut queue = queue.lock().expect("payload queue lock poisoned");
        for _ in 0..count {
            if queue.pop_back().is_none() {
                break;
            }
        }
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
    fn submit_native_unsafe(&mut self, op: Box<dyn NativeUnsafeOpDriver>) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Unsafe { op });
        self.on_native_submit();

        let entry = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Unsafe { op }) => match op.build_entry() {
                Ok(entry) => entry.user_data(native_to_userdata(native_index)),
                Err(err) => {
                    self.on_native_complete();
                    let op = self.native_ops.remove(native_index);
                    if let NativeIoOp::Unsafe { op } = op {
                        op.fail(err);
                    }
                    return Ok(());
                }
            },
            _ => unreachable!("native unsafe op kind mismatch"),
        };

        if self.push_entry(entry).is_err() {
            self.fail_native_op(native_index);
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
        offset: usize,
        reply: NativeBufReply,
    ) -> Result<(), SendError> {
        if offset > buf.len() {
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native recv offset exceeds buffer length",
            )));
            return Ok(());
        }
        let len = buf.len().saturating_sub(offset);
        let Ok(len_u32) = u32::try_from(len) else {
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native recv length exceeds u32::MAX",
            )));
            return Ok(());
        };

        if len == 0 {
            reply.complete(Ok((0, buf)));
            return Ok(());
        }

        let native_index = self
            .native_ops
            .insert(NativeIoOp::Recv { buf, offset, reply });
        self.on_native_submit();

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Recv { buf, offset, .. }) => buf.as_mut_ptr().wrapping_add(*offset),
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
        offset: usize,
        reply: NativeBufReply,
    ) -> Result<(), SendError> {
        if offset > buf.len() {
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native send offset exceeds buffer length",
            )));
            return Ok(());
        }
        let len = buf.len().saturating_sub(offset);
        let Ok(len_u32) = u32::try_from(len) else {
            reply.complete(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native send length exceeds u32::MAX",
            )));
            return Ok(());
        };

        if len == 0 {
            reply.complete(Ok((0, buf)));
            return Ok(());
        }

        let native_index = self
            .native_ops
            .insert(NativeIoOp::Send { buf, offset, reply });
        self.on_native_submit();

        let buf_ptr = match self.native_ops.get_mut(native_index) {
            Some(NativeIoOp::Send { buf, offset, .. }) => buf.as_ptr().wrapping_add(*offset),
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
    fn submit_native_timeout(
        &mut self,
        duration: Duration,
        reply: oneshot::Sender<std::io::Result<()>>,
    ) -> Result<(), SendError> {
        let native_index = self.native_ops.insert(NativeIoOp::Timeout {
            timespec: Box::new(duration.into()),
            reply,
        });
        self.on_native_submit();

        let ts_ptr = match self.native_ops.get(native_index) {
            Some(NativeIoOp::Timeout { timespec, .. }) => {
                timespec.as_ref() as *const types::Timespec
            }
            _ => unreachable!("native timeout op kind mismatch"),
        };

        let entry = opcode::Timeout::new(ts_ptr)
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
                    reply.complete(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv failed",
                    )));
                }
                NativeIoOp::Send { reply, .. } => {
                    reply.complete(Err(std::io::Error::new(
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
                NativeIoOp::Timeout { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring timeout failed",
                    )));
                }
                NativeIoOp::Unsafe { op } => {
                    op.fail(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring unsafe op failed",
                    ));
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
                    reply.complete(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring recv canceled",
                    )));
                }
                NativeIoOp::Send { reply, .. } => {
                    reply.complete(Err(std::io::Error::new(
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
                NativeIoOp::Timeout { reply, .. } => {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring timeout canceled",
                    )));
                }
                NativeIoOp::Unsafe { op } => {
                    op.fail(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "native io_uring unsafe op canceled",
                    ));
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

    fn reap(
        &mut self,
        event_state: &EventState,
        hot_event_state: &EventState,
        hot_counter_state: &HotCounterState,
        hot_msg_tags: &[bool],
        coalesced_hot_msg_tags: &[bool],
    ) {
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
            self.drain_payload_queue(
                from,
                event_state,
                hot_event_state,
                hot_counter_state,
                hot_msg_tags,
                coalesced_hot_msg_tags,
            );
        }
        if !msg_events.is_empty() {
            self.stats
                .ring_msgs_completed
                .fetch_add(msg_events.len() as u64, Ordering::Relaxed);
            push_ring_msg_batch(
                event_state,
                hot_event_state,
                hot_counter_state,
                hot_msg_tags,
                coalesced_hot_msg_tags,
                msg_events,
            );
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

    fn drain_payload_queue(
        &self,
        from: ShardId,
        event_state: &EventState,
        hot_event_state: &EventState,
        hot_counter_state: &HotCounterState,
        hot_msg_tags: &[bool],
        coalesced_hot_msg_tags: &[bool],
    ) {
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
        push_ring_msg_batch(
            event_state,
            hot_event_state,
            hot_counter_state,
            hot_msg_tags,
            coalesced_hot_msg_tags,
            drained
                .into_iter()
                .map(|payload| (from, payload.tag, payload.val)),
        );
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
            NativeIoOp::Recv { buf, reply, .. } => {
                if result < 0 {
                    reply.complete(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                reply.complete(Ok((result as usize, buf)));
            }
            NativeIoOp::Send { buf, reply, .. } => {
                if result < 0 {
                    reply.complete(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                reply.complete(Ok((result as usize, buf)));
            }
            NativeIoOp::RecvMulti { .. } => {}
            NativeIoOp::Fsync { reply } => {
                if result < 0 {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok(()));
            }
            NativeIoOp::Timeout { reply, .. } => {
                if result < 0 && -result != libc::ETIME {
                    let _ = reply.send(Err(std::io::Error::from_raw_os_error(-result)));
                    return;
                }
                let _ = reply.send(Ok(()));
            }
            NativeIoOp::Unsafe { op } => {
                op.complete(UringCqe { result, flags });
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

fn build_stealable_inboxes(shards: usize, backend: StealableQueueBackend) -> StealableInboxes {
    let mut queues = Vec::with_capacity(shards);
    for _ in 0..shards {
        let queue = match backend {
            StealableQueueBackend::Mutex => StealableInbox::Mutex {
                queue: CachePadded::new(Mutex::new(VecDeque::new())),
            },
            StealableQueueBackend::SegQueueExperimental => StealableInbox::SegQueue {
                queue: SegQueue::new(),
                len: CachePadded::new(AtomicUsize::new(0)),
            },
        };
        queues.push(Arc::new(queue));
    }
    Arc::new(queues)
}

fn build_stealable_wake_flags(shards: usize) -> StealableWakeFlags {
    let mut flags = Vec::with_capacity(shards);
    for _ in 0..shards {
        flags.push(CachePadded::new(AtomicBool::new(false)));
    }
    Arc::new(flags)
}

fn build_hot_msg_tag_lookup(tags: &[u16]) -> Vec<bool> {
    let mut lookup = vec![false; HOT_MSG_TAG_COUNT];
    for &tag in tags {
        lookup[usize::from(tag)] = true;
    }
    lookup
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

#[inline]
fn is_hot_msg_tag(hot_msg_tags: &[bool], tag: u16) -> bool {
    hot_msg_tags.get(usize::from(tag)).copied().unwrap_or(false)
}

#[inline]
fn push_ring_msg(
    event_state: &EventState,
    hot_event_state: &EventState,
    hot_counter_state: &HotCounterState,
    hot_msg_tags: &[bool],
    coalesced_hot_msg_tags: &[bool],
    from: ShardId,
    tag: u16,
    val: u32,
) {
    if is_hot_msg_tag(coalesced_hot_msg_tags, tag) {
        hot_counter_state.add_many(std::iter::once((tag, u64::from(val))));
    } else if is_hot_msg_tag(hot_msg_tags, tag) {
        hot_event_state.push(Event::RingMsg { from, tag, val });
    } else {
        event_state.push(Event::RingMsg { from, tag, val });
    }
}

fn push_ring_msg_batch<I>(
    event_state: &EventState,
    hot_event_state: &EventState,
    hot_counter_state: &HotCounterState,
    hot_msg_tags: &[bool],
    coalesced_hot_msg_tags: &[bool],
    messages: I,
) where
    I: IntoIterator<Item = (ShardId, u16, u32)>,
{
    let mut regular = Vec::new();
    let mut hot = Vec::new();
    let mut coalesced_sums: HashMap<u16, u64> = HashMap::new();
    for (from, tag, val) in messages {
        if is_hot_msg_tag(hot_msg_tags, tag) {
            if is_hot_msg_tag(coalesced_hot_msg_tags, tag) {
                if let Some(sum) = coalesced_sums.get_mut(&tag) {
                    *sum = sum.saturating_add(u64::from(val));
                } else {
                    coalesced_sums.insert(tag, u64::from(val));
                }
            } else {
                hot.push(Event::RingMsg { from, tag, val });
            }
        } else {
            regular.push(Event::RingMsg { from, tag, val });
        }
    }

    if !coalesced_sums.is_empty() {
        hot_counter_state.add_many(coalesced_sums);
    }

    if !regular.is_empty() {
        event_state.push_many(regular);
    }
    if !hot.is_empty() {
        hot_event_state.push_many(hot);
    }
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
    stealable_wake_flags: StealableWakeFlags,
    hot_msg_tags: Arc<Vec<bool>>,
    coalesced_hot_msg_tags: Arc<Vec<bool>>,
    hot_counter_wake_threshold: u64,
    steal_budget: usize,
    steal_policy: StealPolicyConfig,
    idle_strategy: IdleStrategy,
    mut backend: ShardBackend,
    stats: Arc<RuntimeStatsInner>,
) {
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let event_state = Arc::new(EventState::default());
    let hot_event_state = Arc::new(EventState::default());
    let hot_counter_state = Arc::new(HotCounterState::new(hot_counter_wake_threshold));
    let local_commands = Rc::new(RefCell::new(VecDeque::new()));
    let ctx = ShardCtx {
        inner: Rc::new(ShardCtxInner {
            runtime_id,
            shard_id,
            event_state: event_state.clone(),
            hot_event_state: hot_event_state.clone(),
            hot_counter_state: hot_counter_state.clone(),
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
    let mut steal_state = StealLoopState::default();
    if let Some(flag) = stealable_wake_flags.get(usize::from(shard_id)) {
        flag.store(false, Ordering::Release);
    }

    let mut stop = false;
    while !stop {
        if let Some(flag) = stealable_wake_flags.get(usize::from(shard_id)) {
            flag.store(false, Ordering::Release);
        }
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
            steal_policy,
            &mut steal_cursor,
            &mut steal_state,
            &spawner,
            &stats,
        );
        backend.poll(
            &event_state,
            &hot_event_state,
            &hot_counter_state,
            &hot_msg_tags,
            &coalesced_hot_msg_tags,
        );

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
                        &hot_event_state,
                        &hot_counter_state,
                        &hot_msg_tags,
                        &coalesced_hot_msg_tags,
                        &stats,
                        &local_commands,
                        &stealable_wake_flags,
                        &mut steal_state,
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
                steal_policy,
                &mut steal_cursor,
                &mut steal_state,
                &spawner,
                &stats,
            );
            apply_idle_strategy(idle_strategy, &backend);
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
            steal_policy,
            &mut steal_cursor,
            &mut steal_state,
            &spawner,
            &stats,
        );
        backend.poll(
            &event_state,
            &hot_event_state,
            &hot_counter_state,
            &hot_msg_tags,
            &coalesced_hot_msg_tags,
        );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleAction {
    Noop,
    Yield,
    Sleep(Duration),
}

fn resolve_idle_action(strategy: IdleStrategy, backend_prefers_busy_poll: bool) -> IdleAction {
    match strategy {
        IdleStrategy::BackendDefault => {
            if backend_prefers_busy_poll {
                IdleAction::Yield
            } else {
                IdleAction::Noop
            }
        }
        IdleStrategy::Yield => IdleAction::Yield,
        IdleStrategy::Sleep(duration) => {
            if duration.is_zero() {
                IdleAction::Yield
            } else {
                IdleAction::Sleep(duration)
            }
        }
    }
}

fn apply_idle_strategy(strategy: IdleStrategy, backend: &ShardBackend) {
    match resolve_idle_action(strategy, backend.prefers_busy_poll()) {
        IdleAction::Noop => {}
        IdleAction::Yield => thread::yield_now(),
        IdleAction::Sleep(duration) => thread::sleep(duration),
    }
}

#[cfg(test)]
mod idle_strategy_tests {
    use super::{IdleAction, IdleStrategy, resolve_idle_action};
    use std::time::Duration;

    #[test]
    fn resolve_idle_action_backend_default_depends_on_backend_preference() {
        assert_eq!(
            resolve_idle_action(IdleStrategy::BackendDefault, true),
            IdleAction::Yield
        );
        assert_eq!(
            resolve_idle_action(IdleStrategy::BackendDefault, false),
            IdleAction::Noop
        );
    }

    #[test]
    fn resolve_idle_action_yield_is_always_yield() {
        assert_eq!(
            resolve_idle_action(IdleStrategy::Yield, true),
            IdleAction::Yield
        );
        assert_eq!(
            resolve_idle_action(IdleStrategy::Yield, false),
            IdleAction::Yield
        );
    }

    #[test]
    fn resolve_idle_action_sleep_zero_falls_back_to_yield() {
        assert_eq!(
            resolve_idle_action(IdleStrategy::Sleep(Duration::ZERO), true),
            IdleAction::Yield
        );
        assert_eq!(
            resolve_idle_action(IdleStrategy::Sleep(Duration::ZERO), false),
            IdleAction::Yield
        );
    }

    #[test]
    fn resolve_idle_action_sleep_nonzero_preserves_duration() {
        let duration = Duration::from_millis(2);
        assert_eq!(
            resolve_idle_action(IdleStrategy::Sleep(duration), true),
            IdleAction::Sleep(duration)
        );
        assert_eq!(
            resolve_idle_action(IdleStrategy::Sleep(duration), false),
            IdleAction::Sleep(duration)
        );
    }
}

fn handle_command(
    cmd: Command,
    shard_id: ShardId,
    spawner: &LocalSpawner,
    event_state: &EventState,
    hot_event_state: &EventState,
    hot_counter_state: &HotCounterState,
    hot_msg_tags: &[bool],
    coalesced_hot_msg_tags: &[bool],
    stats: &RuntimeStatsInner,
    _local_commands: &RefCell<VecDeque<LocalCommand>>,
    stealable_wake_flags: &StealableWakeFlags,
    steal_state: &mut StealLoopState,
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
            push_ring_msg(
                event_state,
                hot_event_state,
                hot_counter_state,
                hot_msg_tags,
                coalesced_hot_msg_tags,
                from,
                tag,
                val,
            );
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
                .push_back(op.into_local(shard_id));
            false
        }
        Command::StealableWake => {
            if let Some(flag) = stealable_wake_flags.get(usize::from(shard_id)) {
                flag.store(false, Ordering::Release);
            }
            steal_state.failed_streak = 0;
            steal_state.cooldown_remaining = 0;
            false
        }
        Command::Shutdown => true,
    }
}

fn drain_stealable_tasks(
    shard_id: ShardId,
    stealable_deques: &StealableInboxes,
    steal_budget: usize,
    steal_policy: StealPolicyConfig,
    steal_cursor: &mut usize,
    steal_state: &mut StealLoopState,
    spawner: &LocalSpawner,
    stats: &RuntimeStatsInner,
) -> bool {
    fn spawn_task(
        task: StealableTask,
        stolen: bool,
        spawner: &LocalSpawner,
        stats: &RuntimeStatsInner,
    ) {
        stats.stealable_executed.fetch_add(1, Ordering::Relaxed);
        if stolen {
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
    let mut local_hits = 0u64;

    while remaining > 0 {
        let task = local_deque.pop_local();
        let Some(task) = task else {
            break;
        };
        drained = true;
        remaining -= 1;
        local_hits += 1;
        spawn_task(task, false, spawner, stats);
    }
    if local_hits > 0 {
        stats
            .stealable_local_hits
            .fetch_add(local_hits, Ordering::Relaxed);
        steal_state.failed_streak = 0;
        steal_state.cooldown_remaining = 0;
    }

    if remaining == 0 || stealable_deques.len() <= 1 {
        return drained;
    }

    if steal_state.cooldown_remaining > 0 {
        steal_state.cooldown_remaining -= 1;
        stats.steal_skipped_backoff.fetch_add(1, Ordering::Relaxed);
        return drained;
    }

    let shard_count = stealable_deques.len();
    let stride = steal_policy.victim_stride.max(1) % shard_count.max(1);
    let stride = if stride == 0 { 1 } else { stride };
    let max_attempts = shard_count.saturating_sub(1).max(1);
    let mut attempts = 0usize;
    let mut stole_any = false;
    while remaining > 0 && attempts < max_attempts {
        let mut best_victim = None;
        let mut best_len = 0usize;
        let probes = steal_policy.victim_probe_count.max(1);

        for _ in 0..probes {
            if attempts >= max_attempts {
                break;
            }

            let mut candidate_idx = *steal_cursor % shard_count;
            *steal_cursor = (*steal_cursor + stride) % shard_count;
            if candidate_idx == local_idx {
                candidate_idx = *steal_cursor % shard_count;
                *steal_cursor = (*steal_cursor + stride) % shard_count;
            }
            if candidate_idx == local_idx {
                continue;
            }

            attempts += 1;
            stats.steal_attempts.fetch_add(1, Ordering::Relaxed);
            let candidate_len = stealable_deques[candidate_idx].len_estimate();
            if candidate_len > best_len {
                best_len = candidate_len;
                best_victim = Some(candidate_idx);
            }
        }

        let Some(victim_idx) = best_victim else {
            break;
        };
        stats.steal_scans.fetch_add(1, Ordering::Relaxed);

        let estimated_time_saved = best_len;
        let fail_streak_cost = steal_state.failed_streak.min(4);
        let estimated_migration_cost =
            1usize.saturating_add(fail_streak_cost.saturating_mul(steal_policy.fail_cost.max(1)));
        if estimated_time_saved
            <= estimated_migration_cost.saturating_add(steal_policy.locality_margin)
        {
            stats.steal_skipped_locality.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let dynamic_batch = if best_len
            > estimated_migration_cost
                .saturating_add(steal_policy.locality_margin)
                .saturating_add(1)
        {
            steal_policy.batch_size.max(1).min(best_len)
        } else {
            1
        };
        let to_take = dynamic_batch.min(remaining).max(1);

        let mut stolen_now = 0u64;
        for _ in 0..to_take {
            let Some(task) = stealable_deques[victim_idx].pop_stolen() else {
                break;
            };
            stolen_now += 1;
            remaining -= 1;
            drained = true;
            spawn_task(task, true, spawner, stats);
        }

        if stolen_now > 0 {
            stats.steal_success.fetch_add(stolen_now, Ordering::Relaxed);
            stole_any = true;
        }
    }

    if stole_any {
        steal_state.failed_streak = 0;
        steal_state.cooldown_remaining = 0;
    } else if remaining > 0 {
        steal_state.failed_streak = steal_state.failed_streak.saturating_add(1);
        stats.observe_failed_streak(steal_state.failed_streak);

        let shift = steal_state.failed_streak.saturating_sub(1).min(8) as u32;
        let scale = 1usize.checked_shl(shift).unwrap_or(usize::MAX);
        let cooldown = steal_policy
            .backoff_min
            .saturating_mul(scale)
            .min(steal_policy.backoff_max.max(steal_policy.backoff_min));
        steal_state.cooldown_remaining = cooldown.max(1);
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
                backend.submit_ring_msg_batch(shard_id, target, messages, command_txs, stats);
            }
            LocalCommand::SubmitRingMsgDirectBatch { target, messages } => {
                backend.submit_ring_msg_direct_batch(
                    shard_id,
                    target,
                    messages,
                    command_txs,
                    stats,
                );
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
                offset,
                reply,
            } => backend.submit_native_recv(shard_id, origin_shard, fd, buf, offset, reply, stats),
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeSendOwned {
                origin_shard,
                fd,
                buf,
                offset,
                reply,
            } => backend.submit_native_send(shard_id, origin_shard, fd, buf, offset, reply, stats),
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
            LocalCommand::SubmitNativeTimeout {
                origin_shard,
                duration,
                reply,
            } => backend.submit_native_timeout(shard_id, origin_shard, duration, reply, stats),
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
            #[cfg(all(feature = "uring-native", target_os = "linux"))]
            LocalCommand::SubmitNativeUnsafe { origin_shard, op } => {
                backend.submit_native_unsafe(shard_id, origin_shard, op, stats)
            }
        }
    }
}
