use futures::executor::block_on;
use msg_ring_runtime::{Event, Runtime, ShardCtx};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_handle_spawn_pinned_runs_on_requested_shard() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();

    let join = handle
        .spawn_pinned(1, async {
            let ctx = ShardCtx::current().expect("on shard");
            ctx.shard_id()
        })
        .expect("spawn pinned");

    let shard = join.await.expect("join");
    assert_eq!(shard, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_handle_spawn_stealable_round_robins_shards() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();

    let a = handle
        .spawn_stealable(async {
            let ctx = ShardCtx::current().expect("on shard");
            ctx.shard_id()
        })
        .expect("spawn a");
    let b = handle
        .spawn_stealable(async {
            let ctx = ShardCtx::current().expect("on shard");
            ctx.shard_id()
        })
        .expect("spawn b");
    let c = handle
        .spawn_stealable(async {
            let ctx = ShardCtx::current().expect("on shard");
            ctx.shard_id()
        })
        .expect("spawn c");
    let d = handle
        .spawn_stealable(async {
            let ctx = ShardCtx::current().expect("on shard");
            ctx.shard_id()
        })
        .expect("spawn d");

    let out = vec![
        a.await.expect("join a"),
        b.await.expect("join b"),
        c.await.expect("join c"),
        d.await.expect("join d"),
    ];
    assert_eq!(out, vec![0, 1, 0, 1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_handle_remote_send_from_tokio_task_delivers_event() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();
    let remote = handle.remote(1).expect("remote");

    let recv = handle
        .spawn_pinned(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let sender = tokio::spawn(async move {
        remote.send_raw(77, 9).expect("send").await.expect("ticket");
    });

    sender.await.expect("tokio sender");
    let event = recv.await.expect("receiver");
    assert_eq!(
        event,
        Event::RingMsg {
            from: u16::MAX,
            tag: 77,
            val: 9
        }
    );
}

#[test]
fn runtime_handle_is_cloneable_and_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    let rt = Runtime::builder().shards(1).build().expect("runtime");
    let handle = rt.handle();
    let cloned = handle.clone();
    assert_eq!(cloned.shard_count(), 1);
    assert_send_sync::<msg_ring_runtime::RuntimeHandle>();
    let join = cloned.spawn_pinned(0, async { 1u8 }).expect("spawn");
    let out = block_on(join).expect("join");
    assert_eq!(out, 1u8);
}
