use futures::executor::block_on;
use msg_ring_runtime::{Event, RingMsg, Runtime, ShardCtx};

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
