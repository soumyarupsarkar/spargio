use futures::executor::block_on;
use spargio::{BackendKind, Event, Runtime, RuntimeError, ShardCtx, TaskPlacement};
use std::time::Duration;

#[test]
fn spawn_with_placement_sticky_routes_same_key_to_same_shard() {
    let rt = Runtime::builder().shards(4).build().expect("runtime");
    let handle = rt.handle();

    let a = handle
        .spawn_with_placement(TaskPlacement::Sticky(0xABCD), async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn a");
    let b = handle
        .spawn_with_placement(TaskPlacement::Sticky(0xABCD), async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn b");

    let sa = block_on(a).expect("join a");
    let sb = block_on(b).expect("join b");
    assert_eq!(sa, sb);
}

#[test]
fn spawn_with_placement_pinned_routes_to_requested_shard() {
    let rt = Runtime::builder().shards(3).build().expect("runtime");
    let handle = rt.handle();

    let join = handle
        .spawn_with_placement(TaskPlacement::Pinned(2), async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn");

    assert_eq!(block_on(join).expect("join"), 2);
}

#[test]
fn spawn_with_placement_round_robin_cycles_shards() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();

    let a = handle
        .spawn_with_placement(TaskPlacement::RoundRobin, async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn a");
    let b = handle
        .spawn_with_placement(TaskPlacement::RoundRobin, async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn b");
    let c = handle
        .spawn_with_placement(TaskPlacement::RoundRobin, async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn c");
    let d = handle
        .spawn_with_placement(TaskPlacement::RoundRobin, async {
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("spawn d");

    let out = vec![
        block_on(a).expect("join a"),
        block_on(b).expect("join b"),
        block_on(c).expect("join c"),
        block_on(d).expect("join d"),
    ];
    assert_eq!(out, vec![0, 1, 0, 1]);
}

#[test]
fn stealable_preferred_tasks_can_run_on_another_shard_under_load() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();
    let (started_tx, started_rx) = std::sync::mpsc::channel();

    let blocker = handle
        .spawn_pinned(0, async move {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(75));
            ShardCtx::current().expect("on shard").shard_id()
        })
        .expect("blocker");
    started_rx.recv().expect("blocker started");

    let mut joins = Vec::new();
    for _ in 0..8 {
        let join = handle
            .spawn_stealable_on(0, async {
                ShardCtx::current().expect("on shard").shard_id()
            })
            .expect("spawn stealable");
        joins.push(join);
    }

    let mut ran_on_other = false;
    for join in joins {
        if block_on(join).expect("join") == 1 {
            ran_on_other = true;
        }
    }
    assert!(
        ran_on_other,
        "expected at least one preferred shard-0 task to run on shard 1"
    );

    let block_shard = block_on(blocker).expect("blocker join");
    assert_eq!(block_shard, 0);

    let stats = handle.stats_snapshot();
    assert!(stats.stealable_stolen > 0);
}

#[test]
fn stats_snapshot_tracks_messages_and_spawns() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let handle = rt.handle();

    let recv = handle
        .spawn_pinned(1, async {
            let next = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.next_event()
            };
            next.await
        })
        .expect("spawn receiver");

    let send = handle
        .spawn_pinned(0, async {
            let remote = {
                let ctx = ShardCtx::current().expect("on shard");
                ctx.remote(1).expect("remote")
            };
            remote.send_raw(9, 42).expect("send").await.expect("ticket");
        })
        .expect("spawn sender");

    block_on(send).expect("sender join");
    let event = block_on(recv).expect("receiver join");
    assert!(matches!(
        event,
        Event::RingMsg {
            from: 0,
            tag: 9,
            val: 42
        }
    ));

    let stats = handle.stats_snapshot();
    assert_eq!(stats.shard_command_depths.len(), 2);
    assert!(stats.spawn_pinned_submitted >= 2);
    assert!(stats.ring_msgs_submitted >= 1);
    assert!(stats.ring_msgs_completed >= 1);
}

#[cfg(target_os = "linux")]
#[test]
fn io_uring_stealable_dispatch_uses_msg_ring_wake() {
    let rt = match Runtime::builder()
        .backend(BackendKind::IoUring)
        .shards(2)
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    let handle = rt.handle();
    let submitter = handle.clone();

    let join = handle
        .spawn_pinned(0, async move {
            submitter
                .spawn_stealable_on(1, async {
                    ShardCtx::current().expect("on shard").shard_id()
                })
                .expect("spawn stealable")
                .await
                .expect("stealable join")
        })
        .expect("spawn submitter");
    let _ = block_on(join).expect("submitter join");

    let stats = handle.stats_snapshot();
    assert!(stats.ring_msgs_submitted >= 1);
}

#[test]
fn stealable_queue_capacity_applies_backpressure() {
    let rt = Runtime::builder()
        .shards(1)
        .stealable_queue_capacity(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();
    let (started_tx, started_rx) = std::sync::mpsc::channel();

    let blocker = handle
        .spawn_pinned(0, async move {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(75));
            1usize
        })
        .expect("blocker");
    started_rx.recv().expect("started");

    let first = handle
        .spawn_stealable_on(0, async {
            std::thread::sleep(Duration::from_millis(10));
            7usize
        })
        .expect("first stealable");
    match handle.spawn_stealable_on(0, async { 8usize }) {
        Ok(_) => panic!("expected overloaded error"),
        Err(RuntimeError::Overloaded) => {}
        Err(err) => panic!("unexpected error: {err:?}"),
    }

    let _ = block_on(first).expect("first join");
    let _ = block_on(blocker).expect("blocker join");
}

#[test]
fn steal_stats_track_attempts_and_success() {
    let rt = Runtime::builder()
        .shards(3)
        .steal_budget(64)
        .build()
        .expect("runtime");
    let handle = rt.handle();
    let (started_tx, started_rx) = std::sync::mpsc::channel();

    let blocker = handle
        .spawn_pinned(0, async move {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(90));
            0usize
        })
        .expect("blocker");
    started_rx.recv().expect("blocker started");

    let mut joins = Vec::new();
    for _ in 0..32 {
        let join = handle
            .spawn_stealable_on(0, async {
                std::thread::sleep(Duration::from_millis(1));
                ShardCtx::current().expect("on shard").shard_id()
            })
            .expect("spawn stealable");
        joins.push(join);
    }

    let mut ran_elsewhere = false;
    for join in joins {
        let shard = block_on(join).expect("join");
        if shard != 0 {
            ran_elsewhere = true;
        }
    }
    assert!(
        ran_elsewhere,
        "expected stolen work to run on non-owner shard"
    );
    let _ = block_on(blocker).expect("blocker join");

    let stats = handle.stats_snapshot();
    assert!(stats.steal_attempts > 0);
    assert!(stats.steal_success > 0);
}
