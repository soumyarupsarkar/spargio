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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub type ShardId = u16;
const EXTERNAL_SENDER: ShardId = ShardId::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    RingMsg { from: ShardId, tag: u16, val: u32 },
}

pub trait RingMsg: Copy + Send + 'static {
    fn encode(self) -> (u16, u32);
    fn decode(tag: u16, val: u32) -> Self;
}

#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    shards: usize,
    thread_prefix: String,
    idle_wait: Duration,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            shards: std::thread::available_parallelism().map_or(1, usize::from),
            thread_prefix: "msg-ring-shard".to_owned(),
            idle_wait: Duration::from_millis(1),
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

    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.shards == 0 {
            return Err(RuntimeError::InvalidConfig("shards must be > 0"));
        }
        if self.shards > usize::from(ShardId::MAX) {
            return Err(RuntimeError::InvalidConfig(
                "shard count exceeds supported ShardId range",
            ));
        }

        let mut senders = Vec::with_capacity(self.shards);
        let mut receivers = Vec::with_capacity(self.shards);

        for _ in 0..self.shards {
            let (tx, rx) = unbounded();
            senders.push(tx);
            receivers.push(rx);
        }

        let remotes: Vec<RemoteShard> = senders
            .iter()
            .enumerate()
            .map(|(i, tx)| RemoteShard {
                id: i as ShardId,
                tx: tx.clone(),
            })
            .collect();

        let mut joins = Vec::with_capacity(self.shards);
        for (idx, rx) in receivers.into_iter().enumerate() {
            let remotes_for_shard = remotes.clone();
            let thread_name = format!("{}-{}", self.thread_prefix, idx);
            let idle_wait = self.idle_wait;
            let id = idx as ShardId;

            let join = thread::Builder::new()
                .name(thread_name)
                .spawn(move || run_shard(id, rx, remotes_for_shard, idle_wait))
                .map_err(RuntimeError::ThreadSpawn)?;

            joins.push(join);
        }

        Ok(Runtime {
            remotes,
            joins,
            is_shutdown: false,
        })
    }
}

#[derive(Debug)]
pub struct Runtime {
    remotes: Vec<RemoteShard>,
    joins: Vec<thread::JoinHandle<()>>,
    is_shutdown: bool,
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
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
        let remote = self
            .remote(shard)
            .ok_or(RuntimeError::InvalidShard(shard))?;
        let (tx, rx) = oneshot::channel();

        remote
            .tx
            .send(Command::Spawn(Box::pin(async move {
                let out = fut.await;
                let _ = tx.send(out);
            })))
            .map_err(|_| RuntimeError::Closed)?;

        Ok(JoinHandle { rx: Some(rx) })
    }

    pub fn shutdown(&mut self) {
        if self.is_shutdown {
            return;
        }
        self.is_shutdown = true;

        for remote in &self.remotes {
            let _ = remote.tx.send(Command::Shutdown);
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

#[derive(Debug, Clone)]
pub struct RemoteShard {
    id: ShardId,
    tx: Sender<Command>,
}

impl RemoteShard {
    pub fn id(&self) -> ShardId {
        self.id
    }

    pub fn send_raw(&self, tag: u16, val: u32) -> Result<SendTicket, SendError> {
        let from = ShardCtx::current().map_or(EXTERNAL_SENDER, |ctx| ctx.shard_id());
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(Command::RawMessage {
                from,
                tag,
                val,
                ack: ack_tx,
            })
            .map_err(|_| SendError::Closed)?;

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
    RawMessage {
        from: ShardId,
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

fn run_shard(id: ShardId, rx: Receiver<Command>, remotes: Vec<RemoteShard>, idle_wait: Duration) {
    let mut pool = LocalPool::new();
    let spawner = pool.spawner();
    let event_state = Arc::new(EventState::default());
    let ctx = ShardCtx {
        inner: Rc::new(ShardCtxInner {
            shard_id: id,
            event_state: event_state.clone(),
            spawner: spawner.clone(),
            remotes,
        }),
    };

    CURRENT_SHARD.with(|slot| {
        *slot.borrow_mut() = Some(ctx.clone());
    });

    let mut stop = false;
    while !stop {
        pool.run_until_stalled();

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
            match rx.recv_timeout(idle_wait) {
                Ok(cmd) => {
                    stop = handle_command(cmd, &spawner, &event_state);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => stop = true,
            }
        }
    }

    while let Ok(cmd) = rx.try_recv() {
        if let Command::RawMessage { ack, .. } = cmd {
            let _ = ack.send(Err(SendError::Closed));
        }
    }

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
        Command::RawMessage {
            from,
            tag,
            val,
            ack,
        } => {
            event_state.push(Event::RingMsg { from, tag, val });
            let _ = ack.send(Ok(()));
            false
        }
        Command::Shutdown => true,
    }
}
