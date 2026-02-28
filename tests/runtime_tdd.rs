use futures::executor::block_on;
use spargio::{
    BackendKind, Event, RingMsg, Runtime, RuntimeError, ShardCtx, run, run_local_on, run_with,
    sleep,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Ready,
    Ack,
}

impl RingMsg for Control {
    fn encode(self) -> (u16, u32) {
        match self {
            Self::Ready => (1, 0),
            Self::Ack => (2, 0),
        }
    }

    fn decode(tag: u16, _val: u32) -> Self {
        match tag {
            1 => Self::Ready,
            2 => Self::Ack,
            _ => panic!("unexpected tag {tag}"),
        }
    }
}

#[test]
fn spawn_local_runs_on_shard() {
    let rt = Runtime::builder().shards(1).build().expect("runtime");
    let join = rt
        .spawn_on(0, async {
            let local = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.spawn_local(async { 7u32 })
            };
            local.await.expect("local join")
        })
        .expect("spawn");
    let out = block_on(join).expect("join");
    assert_eq!(out, 7);
}

#[test]
fn send_raw_delivers_event_and_sender_id() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_raw(42, 7)
                .expect("queue send")
                .await
                .expect("send");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let event = block_on(recv).expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 42,
            val: 7
        }
    );
}

#[test]
fn typed_send_round_trips() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            match next.await {
                Event::RingMsg { tag, val, .. } => Control::decode(tag, val),
            }
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send(Control::Ack)
                .expect("queue send")
                .await
                .expect("send");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let got = block_on(recv).expect("receiver join");
    assert_eq!(got, Control::Ack);
}

#[test]
fn send_raw_nowait_delivers_event() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote.send_raw_nowait(77, 5).expect("send nowait");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let event = block_on(recv).expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 77,
            val: 5
        }
    );
}

#[test]
fn send_raw_direct_nowait_delivers_event() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_raw_direct_nowait(78, 6)
                .expect("direct send nowait");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let event = block_on(recv).expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 78,
            val: 6
        }
    );
}

#[test]
fn send_many_raw_nowait_delivers_in_order() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let mut out = Vec::new();
            for _ in 0..3 {
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                }
                .await;
                out.push(event);
            }
            out
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(10, 1), (11, 2), (12, 3)])
                .expect("send many");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let events = block_on(recv).expect("receiver join");
    assert_eq!(
        events,
        vec![
            Event::RingMsg {
                from: 0,
                tag: 10,
                val: 1
            },
            Event::RingMsg {
                from: 0,
                tag: 11,
                val: 2
            },
            Event::RingMsg {
                from: 0,
                tag: 12,
                val: 3
            }
        ]
    );
}

#[test]
fn send_many_raw_direct_nowait_delivers_in_order() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let mut out = Vec::new();
            for _ in 0..3 {
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                }
                .await;
                out.push(event);
            }
            out
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_direct_nowait([(20, 4), (21, 5), (22, 6)])
                .expect("direct send many");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let events = block_on(recv).expect("receiver join");
    assert_eq!(
        events,
        vec![
            Event::RingMsg {
                from: 0,
                tag: 20,
                val: 4
            },
            Event::RingMsg {
                from: 0,
                tag: 21,
                val: 5
            },
            Event::RingMsg {
                from: 0,
                tag: 22,
                val: 6
            }
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_msg_tag_routes_to_hot_event_lane() {
    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(55)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_hot_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote.send_raw_nowait(55, 9).expect("send nowait");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let event = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 55,
            val: 9
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_hot_msg_tag_remains_on_regular_event_lane() {
    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(55)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote.send_raw_nowait(56, 10).expect("send nowait");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let event = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 56,
            val: 10
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_hot_msg_tag_aggregates_batch_values() {
    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(57)
        .coalesced_hot_msg_tag(57)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_hot_count(57)
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(57, 1), (57, 2), (57, 3)])
                .expect("send many");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let event = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(event, 6);

    let second = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_hot_count(57)
            };
            next.await
        })
        .expect("spawn second receiver");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), second)
            .await
            .is_err(),
        "expected coalesced lane to emit a single event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_coalesced_hot_msg_tag_preserves_batch_events() {
    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(58)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let mut out = Vec::new();
            for _ in 0..3 {
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_hot_event()
                }
                .await;
                out.push(event);
            }
            out
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(58, 1), (58, 2), (58, 3)])
                .expect("send many");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let events = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(
        events,
        vec![
            Event::RingMsg {
                from: 0,
                tag: 58,
                val: 1
            },
            Event::RingMsg {
                from: 0,
                tag: 58,
                val: 2
            },
            Event::RingMsg {
                from: 0,
                tag: 58,
                val: 3
            }
        ]
    );
}

#[test]
fn coalesced_hot_tag_absorbs_batch_under_tight_queue_capacity() {
    let rt = Runtime::builder()
        .shards(2)
        .msg_ring_queue_capacity(1)
        .hot_msg_tag(59)
        .coalesced_hot_msg_tag(59)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_hot_count(59)
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(59, 1), (59, 2), (59, 3)])
                .expect("coalesced batch should fit in tight queue");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let count = block_on(recv).expect("receiver join");
    assert_eq!(count, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_hot_count_accumulates_across_batches() {
    const HOT_TAG: u16 = 61;
    const BARRIER_TAG: u16 = 63;

    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(HOT_TAG)
        .coalesced_hot_msg_tag(HOT_TAG)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let mut waited = 0u64;
            loop {
                let next = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                };
                let event = next.await;
                if let Event::RingMsg {
                    tag: BARRIER_TAG, ..
                } = event
                {
                    let total = {
                        let ctx = ShardCtx::current().expect("on shard");
                        ctx.try_take_hot_count(HOT_TAG)
                    };
                    return total.unwrap_or(0);
                }
                if waited >= 1_000 {
                    panic!("timed out waiting for coalesced hot count");
                }
                sleep(Duration::from_millis(1)).await;
                waited += 1;
            }
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(HOT_TAG, 1), (HOT_TAG, 2)])
                .expect("send first batch");
            remote.flush().expect("flush ticket").await.expect("flush");
            remote
                .send_raw_nowait(HOT_TAG, 3)
                .expect("send second batch");
            remote
                .send_raw_nowait(BARRIER_TAG, 1)
                .expect("send barrier");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let total = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(total, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_counter_threshold_does_not_starve_first_update() {
    let rt = Runtime::builder()
        .shards(2)
        .hot_msg_tag(62)
        .coalesced_hot_msg_tag(62)
        .hot_counter_wake_threshold(128)
        .build()
        .expect("runtime");

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_hot_count(62)
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote.send_raw_nowait(62, 1).expect("send nowait");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    tokio::time::timeout(Duration::from_secs(1), send)
        .await
        .expect("sender timeout")
        .expect("sender join");
    let count = tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .expect("receiver timeout")
        .expect("receiver join");
    assert_eq!(count, 1);
}

#[test]
fn flush_completes_without_messages() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_helper_executes_top_level_future() {
    let out = run(|handle| async move {
        let join = handle.spawn_stealable(async { 41usize }).expect("spawn");
        join.await.expect("join") + 1
    })
    .await
    .expect("run");
    assert_eq!(out, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_with_applies_custom_builder() {
    let out = match run_with(
        Runtime::builder().shards(1).backend(BackendKind::IoUring),
        |handle| async move {
            let join = handle
                .spawn_pinned(0, async {
                    ShardCtx::current().expect("on shard").shard_id()
                })
                .expect("spawn");
            join.await.expect("join")
        },
    )
    .await
    {
        Ok(out) => out,
        #[cfg(target_os = "linux")]
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => return,
        Err(err) => panic!("unexpected run_with error: {err:?}"),
    };
    assert_eq!(out, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_with_propagates_builder_errors() {
    let err = run_with(Runtime::builder().shards(0), |_| async { 1usize })
        .await
        .expect_err("invalid runtime config");
    assert!(matches!(err, RuntimeError::InvalidConfig(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_local_on_accepts_non_send_future() {
    let out = match run_local_on(
        Runtime::builder().shards(1).backend(BackendKind::IoUring),
        0,
        |ctx| {
            let local = Rc::new(RefCell::new(40usize));
            async move {
                assert_eq!(ctx.shard_id(), 0);
                *local.borrow_mut() += 2;
                *local.borrow()
            }
        },
    )
    .await
    {
        Ok(out) => out,
        #[cfg(target_os = "linux")]
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => return,
        Err(err) => panic!("unexpected run_local_on error: {err:?}"),
    };
    assert_eq!(out, 42);
}

#[test]
fn runtime_handle_spawn_local_on_accepts_non_send_future() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();
    let join = handle
        .spawn_local_on(1, |ctx| {
            let local = Rc::new(RefCell::new(5usize));
            async move {
                assert_eq!(ctx.shard_id(), 1);
                *local.borrow_mut() += 1;
                (ctx.shard_id(), *local.borrow())
            }
        })
        .expect("spawn local on");
    let (shard, out) = block_on(join).expect("join");
    assert_eq!(shard, 1);
    assert_eq!(out, 6);
}

#[test]
fn external_flush_is_noop_ok() {
    let rt = Runtime::builder().shards(1).build().expect("runtime");
    let remote = rt.remote(0).expect("remote shard");
    block_on(remote.flush().expect("flush ticket")).expect("flush");
}

#[cfg(target_os = "linux")]
#[test]
fn io_uring_backend_delivers_message() {
    let rt = match Runtime::builder()
        .backend(BackendKind::IoUring)
        .shards(2)
        .build()
    {
        Ok(rt) => rt,
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => return,
        Err(err) => panic!("unexpected build error: {err:?}"),
    };

    let recv = rt
        .spawn_on(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_raw(55, 99)
                .expect("queue send")
                .await
                .expect("send");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let event = block_on(recv).expect("receiver join");
    assert_eq!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 55,
            val: 99
        }
    );
}

#[cfg(target_os = "linux")]
#[test]
fn io_uring_send_many_nowait_delivers_messages() {
    let rt = match Runtime::builder()
        .backend(BackendKind::IoUring)
        .shards(2)
        .build()
    {
        Ok(rt) => rt,
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => return,
        Err(err) => panic!("unexpected build error: {err:?}"),
    };

    let recv = rt
        .spawn_on(1, async {
            let mut out = Vec::new();
            for _ in 0..3 {
                let event = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                }
                .await;
                out.push(event);
            }
            out
        })
        .expect("spawn receiver");

    let send = rt
        .spawn_on(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote shard")
            };
            remote
                .send_many_raw_nowait([(90, 9), (91, 10), (92, 11)])
                .expect("send many");
            remote.flush().expect("flush ticket").await.expect("flush");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let events = block_on(recv).expect("receiver join");
    assert_eq!(
        events,
        vec![
            Event::RingMsg {
                from: 0,
                tag: 90,
                val: 9
            },
            Event::RingMsg {
                from: 0,
                tag: 91,
                val: 10
            },
            Event::RingMsg {
                from: 0,
                tag: 92,
                val: 11
            }
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_shutdown_is_idempotent() {
    let mut rt = match Runtime::builder()
        .shards(1)
        .backend(BackendKind::IoUring)
        .build()
    {
        Ok(rt) => rt,
        #[cfg(target_os = "linux")]
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => return,
        Err(err) => panic!("unexpected build error: {err:?}"),
    };

    let join = rt.spawn_on(0, async { 1usize }).expect("spawn");
    assert_eq!(join.await.expect("join"), 1);

    rt.shutdown().await;
    rt.shutdown().await;
}
