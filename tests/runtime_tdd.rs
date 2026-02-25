use futures::executor::block_on;
use msg_ring_runtime::{BackendKind, Event, RingMsg, Runtime, RuntimeError, ShardCtx};

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
