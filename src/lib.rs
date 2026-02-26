//! Sharded async runtime API centered around `msg_ring`-style signaling.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, unbounded};
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner};
use futures::task::LocalSpawnExt;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};
#[cfg(target_os = "linux")]
use slab::Slab;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
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
    #[cfg(target_os = "linux")]
    io_uring: IoUringBuildConfig,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            shards: std::thread::available_parallelism().map_or(1, usize::from),
            thread_prefix: "msg-ring-shard".to_owned(),
            idle_wait: Duration::from_millis(1),
            backend: BackendKind::Queue,
            ring_entries: 256,
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

        let shared = Arc::new(RuntimeShared {
            runtime_id,
            backend: self.backend,
            command_txs: senders.clone(),
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
                    let thread_name = format!("{}-{}", self.thread_prefix, idx);
                    let idle_wait = self.idle_wait;
                    let runtime_id = shared.runtime_id;
                    let backend = ShardBackend::Queue;

                    let join = match thread::Builder::new().name(thread_name).spawn(move || {
                        run_shard(
                            runtime_id,
                            idx as ShardId,
                            rx,
                            remotes_for_shard,
                            idle_wait,
                            backend,
                        )
                    }) {
                        Ok(j) => j,
                        Err(err) => {
                            shutdown_spawned(&shared.command_txs, &mut joins);
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
                        let thread_name = format!("{}-{}", self.thread_prefix, idx);
                        let idle_wait = self.idle_wait;
                        let runtime_id = shared.runtime_id;
                        let backend = ShardBackend::IoUring(IoUringDriver::new(
                            idx as ShardId,
                            ring,
                            ring_fds.clone(),
                            payload_queues.clone(),
                        ));

                        let join = match thread::Builder::new().name(thread_name).spawn(move || {
                            run_shard(
                                runtime_id,
                                idx as ShardId,
                                rx,
                                remotes_for_shard,
                                idle_wait,
                                backend,
                            )
                        }) {
                            Ok(j) => j,
                            Err(err) => {
                                shutdown_spawned(&shared.command_txs, &mut joins);
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

fn shutdown_spawned(command_txs: &[Sender<Command>], joins: &mut Vec<thread::JoinHandle<()>>) {
    for tx in command_txs {
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

        for tx in &self.shared.command_txs {
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
        spawn_on_shared(&self.inner.shared, shard, fut)
    }

    pub fn spawn_stealable<F, T>(&self, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let shards = self.shard_count();
        if shards == 0 {
            return Err(RuntimeError::Closed);
        }

        let next = self.inner.next_shard.fetch_add(1, Ordering::Relaxed);
        let shard = (next % shards) as ShardId;
        self.spawn_pinned(shard, fut)
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

#[derive(Clone)]
struct RuntimeShared {
    runtime_id: u64,
    backend: BackendKind,
    command_txs: Vec<Sender<Command>>,
}

impl RuntimeShared {
    fn send_to(&self, shard: ShardId, cmd: Command) -> Result<(), ()> {
        let Some(tx) = self.command_txs.get(usize::from(shard)) else {
            return Err(());
        };
        tx.send(cmd).map_err(|_| ())
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

    pub fn flush(&self) -> Result<SendTicket, SendError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inner
            .local_commands
            .borrow_mut()
            .push_back(LocalCommand::Flush { ack: ack_tx });
        Ok(SendTicket { rx: Some(ack_rx) })
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
    UnsupportedBackend(&'static str),
    #[cfg(target_os = "linux")]
    IoUringInit(std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    Closed,
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
    Flush {
        ack: oneshot::Sender<Result<(), SendError>>,
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
    ) {
        match self {
            Self::Queue => {
                let Some(tx) = command_txs.get(usize::from(target)) else {
                    if let Some(ack) = ack {
                        let _ = ack.send(Err(SendError::Closed));
                    }
                    return;
                };
                let _ = tx.send(Command::InjectRawMessage {
                    from,
                    tag,
                    val,
                    ack,
                });
            }
            #[cfg(target_os = "linux")]
            Self::IoUring(driver) => {
                if driver.submit_ring_msg(target, tag, val, ack).is_err() {
                    // submit_ring_msg already completed ack with an error
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

#[cfg(target_os = "linux")]
struct IoUringDriver {
    shard_id: ShardId,
    ring: IoUring,
    ring_fds: Arc<Vec<RawFd>>,
    payload_queues: PayloadQueues,
    send_waiters: Slab<oneshot::Sender<Result<(), SendError>>>,
    pending_submit: usize,
    submit_batch_limit: usize,
}

#[cfg(target_os = "linux")]
impl IoUringDriver {
    fn new(
        shard_id: ShardId,
        ring: IoUring,
        ring_fds: Arc<Vec<RawFd>>,
        payload_queues: PayloadQueues,
    ) -> Self {
        Self {
            shard_id,
            ring,
            ring_fds,
            payload_queues,
            send_waiters: Slab::new(),
            pending_submit: 0,
            submit_batch_limit: IOURING_SUBMIT_BATCH,
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

    fn fail_all_waiters(&mut self) {
        let waiters = std::mem::take(&mut self.send_waiters);
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(SendError::Closed));
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
            event_state.push(Event::RingMsg { from, tag, val });
        }
        for (user_data, result) in waiter_completions {
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

        for payload in drained {
            event_state.push(Event::RingMsg {
                from,
                tag: payload.tag,
                val: payload.val,
            });
        }
    }

    fn shutdown(&mut self) {
        self.fail_all_waiters();
    }
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
    usize::try_from(user_data.checked_sub(1)?).ok()
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
    idle_wait: Duration,
    mut backend: ShardBackend,
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

    let mut stop = false;
    while !stop {
        pool.run_until_stalled();
        drain_local_commands(shard_id, &local_commands, &mut backend, &command_txs);
        backend.poll(&event_state);

        let mut drained = false;
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    drained = true;
                    stop = handle_command(cmd, &spawner, &event_state);
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
            drain_local_commands(shard_id, &local_commands, &mut backend, &command_txs);
            if backend.prefers_busy_poll() {
                thread::yield_now();
            } else {
                match rx.recv_timeout(idle_wait) {
                    Ok(cmd) => {
                        stop = handle_command(cmd, &spawner, &event_state);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => stop = true,
                }
            }
        }

        drain_local_commands(shard_id, &local_commands, &mut backend, &command_txs);
        backend.poll(&event_state);
    }

    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            Command::InjectRawMessage { ack, .. } => {
                if let Some(ack) = ack {
                    let _ = ack.send(Err(SendError::Closed));
                }
            }
            Command::Spawn(_) | Command::Shutdown => {}
        }
    }
    backend.shutdown();

    CURRENT_SHARD.with(|slot| {
        slot.borrow_mut().take();
    });
}

fn handle_command(cmd: Command, spawner: &LocalSpawner, event_state: &EventState) -> bool {
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
            if let Some(ack) = ack {
                let _ = ack.send(Ok(()));
            }
            false
        }
        Command::Shutdown => true,
    }
}

fn drain_local_commands(
    shard_id: ShardId,
    local_commands: &RefCell<VecDeque<LocalCommand>>,
    backend: &mut ShardBackend,
    command_txs: &[Sender<Command>],
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
            } => backend.submit_ring_msg(shard_id, target, tag, val, ack, command_txs),
            LocalCommand::SubmitRingMsgBatch { target, messages } => {
                for (tag, val) in messages {
                    backend.submit_ring_msg(shard_id, target, tag, val, None, command_txs);
                }
            }
            LocalCommand::Flush { ack } => backend.flush(ack),
        }
    }
}
