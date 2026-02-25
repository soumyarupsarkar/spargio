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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use io_uring::{IoUring, opcode, types};
#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub type ShardId = u16;
const EXTERNAL_SENDER: ShardId = ShardId::MAX;
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "linux")]
const MSG_RING_CQE_FLAG: u32 = 1 << 8;

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

#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    shards: usize,
    thread_prefix: String,
    idle_wait: Duration,
    backend: BackendKind,
    ring_entries: u32,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            shards: std::thread::available_parallelism().map_or(1, usize::from),
            thread_prefix: "msg-ring-shard".to_owned(),
            idle_wait: Duration::from_millis(1),
            backend: BackendKind::Queue,
            ring_entries: 256,
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
                    for _ in 0..self.shards {
                        let ring =
                            IoUring::new(self.ring_entries).map_err(RuntimeError::IoUringInit)?;
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

    pub fn spawn_on<F, T>(&self, shard: ShardId, fut: F) -> Result<JoinHandle<T>, RuntimeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.shared
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
        let current = ShardCtx::current().filter(|ctx| ctx.runtime_id() == self.shared.runtime_id);
        let (ack_tx, ack_rx) = oneshot::channel();

        let send_result = match (self.shared.backend, current) {
            (BackendKind::IoUring, Some(ctx)) => self.shared.send_to(
                ctx.shard_id(),
                Command::SubmitRingMsg {
                    target: self.id,
                    tag,
                    val,
                    ack: ack_tx,
                },
            ),
            (_, maybe_ctx) => {
                let from = maybe_ctx.map_or(EXTERNAL_SENDER, |ctx| ctx.shard_id());
                self.shared.send_to(
                    self.id,
                    Command::InjectRawMessage {
                        from,
                        tag,
                        val,
                        ack: ack_tx,
                    },
                )
            }
        };

        if send_result.is_err() {
            return Err(SendError::Closed);
        }

        Ok(SendTicket { rx: Some(ack_rx) })
    }

    pub fn send<M: RingMsg>(&self, msg: M) -> Result<SendTicket, SendError> {
        let (tag, val) = msg.encode();
        self.send_raw(tag, val)
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

enum Command {
    Spawn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    InjectRawMessage {
        from: ShardId,
        tag: u16,
        val: u32,
        ack: oneshot::Sender<Result<(), SendError>>,
    },
    SubmitRingMsg {
        target: ShardId,
        tag: u16,
        val: u32,
        ack: oneshot::Sender<Result<(), SendError>>,
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
        ack: oneshot::Sender<Result<(), SendError>>,
        command_txs: &[Sender<Command>],
    ) {
        match self {
            Self::Queue => {
                let Some(tx) = command_txs.get(usize::from(target)) else {
                    let _ = ack.send(Err(SendError::Closed));
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

    fn shutdown(&mut self) {
        #[cfg(target_os = "linux")]
        if let Self::IoUring(driver) = self {
            driver.shutdown();
        }
    }
}

#[cfg(target_os = "linux")]
struct IoUringDriver {
    shard_id: ShardId,
    ring: IoUring,
    ring_fds: Arc<Vec<RawFd>>,
    next_ticket: u64,
    send_waiters: HashMap<u64, oneshot::Sender<Result<(), SendError>>>,
}

#[cfg(target_os = "linux")]
impl IoUringDriver {
    fn new(shard_id: ShardId, ring: IoUring, ring_fds: Arc<Vec<RawFd>>) -> Self {
        Self {
            shard_id,
            ring,
            ring_fds,
            next_ticket: 1,
            send_waiters: HashMap::new(),
        }
    }

    fn submit_ring_msg(
        &mut self,
        target: ShardId,
        tag: u16,
        val: u32,
        ack: oneshot::Sender<Result<(), SendError>>,
    ) -> Result<(), SendError> {
        let Some(&target_fd) = self.ring_fds.get(usize::from(target)) else {
            let _ = ack.send(Err(SendError::Closed));
            return Err(SendError::Closed);
        };

        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1).max(1);
        let payload = pack_msg_userdata(self.shard_id, tag);
        let val_i32 = i32::from_ne_bytes(val.to_ne_bytes());

        let entry = opcode::MsgRingData::new(
            types::Fd(target_fd),
            val_i32,
            payload,
            Some(MSG_RING_CQE_FLAG),
        )
        .build()
        .user_data(ticket);

        if self.push_entry(entry).is_err() {
            let _ = ack.send(Err(SendError::Closed));
            return Err(SendError::Closed);
        }

        self.send_waiters.insert(ticket, ack);
        if self.ring.submit().is_err() {
            if let Some(waiter) = self.send_waiters.remove(&ticket) {
                let _ = waiter.send(Err(SendError::Closed));
            }
            return Err(SendError::Closed);
        }

        Ok(())
    }

    fn push_entry(&mut self, entry: io_uring::squeue::Entry) -> Result<(), SendError> {
        for _ in 0..2 {
            let mut sq = self.ring.submission();
            if unsafe { sq.push(&entry) }.is_ok() {
                return Ok(());
            }
            drop(sq);

            if self.ring.submit().is_err() {
                return Err(SendError::Closed);
            }
        }

        Err(SendError::Closed)
    }

    fn reap(&mut self, event_state: &EventState) {
        let mut cq = self.ring.completion();
        for cqe in &mut cq {
            if cqe.flags() & MSG_RING_CQE_FLAG != 0 {
                let (from, tag) = unpack_msg_userdata(cqe.user_data());
                let val = u32::from_ne_bytes(cqe.result().to_ne_bytes());
                event_state.push(Event::RingMsg { from, tag, val });
                continue;
            }

            if let Some(waiter) = self.send_waiters.remove(&cqe.user_data()) {
                let _ = if cqe.result() >= 0 {
                    waiter.send(Ok(()))
                } else {
                    waiter.send(Err(SendError::Closed))
                };
            }
        }
    }

    fn shutdown(&mut self) {
        for (_, waiter) in self.send_waiters.drain() {
            let _ = waiter.send(Err(SendError::Closed));
        }
    }
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
    let ctx = ShardCtx {
        inner: Rc::new(ShardCtxInner {
            runtime_id,
            shard_id,
            event_state: event_state.clone(),
            spawner: spawner.clone(),
            remotes,
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
        backend.poll(&event_state);

        let mut drained = false;
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    drained = true;
                    stop = handle_command(
                        cmd,
                        shard_id,
                        &spawner,
                        &event_state,
                        &mut backend,
                        &command_txs,
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
            if backend.prefers_busy_poll() {
                thread::yield_now();
            } else {
                match rx.recv_timeout(idle_wait) {
                    Ok(cmd) => {
                        stop = handle_command(
                            cmd,
                            shard_id,
                            &spawner,
                            &event_state,
                            &mut backend,
                            &command_txs,
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => stop = true,
                }
            }
        }

        backend.poll(&event_state);
    }

    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            Command::InjectRawMessage { ack, .. } | Command::SubmitRingMsg { ack, .. } => {
                let _ = ack.send(Err(SendError::Closed));
            }
            Command::Spawn(_) | Command::Shutdown => {}
        }
    }
    backend.shutdown();

    CURRENT_SHARD.with(|slot| {
        slot.borrow_mut().take();
    });
}

fn handle_command(
    cmd: Command,
    shard_id: ShardId,
    spawner: &LocalSpawner,
    event_state: &EventState,
    backend: &mut ShardBackend,
    command_txs: &[Sender<Command>],
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
            let _ = ack.send(Ok(()));
            false
        }
        Command::SubmitRingMsg {
            target,
            tag,
            val,
            ack,
        } => {
            backend.submit_ring_msg(shard_id, target, tag, val, ack, command_txs);
            false
        }
        Command::Shutdown => true,
    }
}
