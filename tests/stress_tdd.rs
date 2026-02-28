use futures::executor::block_on;
use spargio::{Runtime, ShardCtx, boundary};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[test]
#[ignore = "long-running soak test; run in nightly schedule"]
fn soak_stealable_burst_completes_without_dropping_tasks() {
    let rt = Runtime::builder().shards(4).build().expect("runtime");
    let handle = rt.handle();
    let completed = Arc::new(AtomicUsize::new(0));

    let mut joins = Vec::new();
    for _ in 0..2_000 {
        let completed = completed.clone();
        let join = handle
            .spawn_stealable(async move {
                let shard = ShardCtx::current().expect("on shard").shard_id();
                completed.fetch_add(1, Ordering::Relaxed);
                shard
            })
            .expect("spawn");
        joins.push(join);
    }

    let mut shards_seen = [false; 4];
    for join in joins {
        let shard = block_on(join).expect("join");
        if let Some(slot) = shards_seen.get_mut(usize::from(shard)) {
            *slot = true;
        }
    }

    assert_eq!(completed.load(Ordering::Relaxed), 2_000);
    assert!(
        shards_seen.into_iter().any(|seen| seen),
        "expected at least one shard to execute work"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "long-running soak test; run in nightly schedule"]
async fn soak_boundary_timeout_cancel_overload_paths_accumulate_stats() {
    boundary::reset_stats_for_tests();
    let (client, server) = boundary::channel::<u64, u64>(8);

    let producer = tokio::spawn(async move {
        for i in 0..2_000u64 {
            if i % 5 == 0 {
                let _ = client.try_call(i);
                continue;
            }
            let ticket = client.call(i).await.expect("queued");
            if i % 7 == 0 {
                drop(ticket);
            } else if i % 11 == 0 {
                let _ = ticket.wait_timeout(Duration::from_millis(1)).await;
            } else {
                let _ = ticket.await;
            }
        }
    });

    let consumer = tokio::spawn(async move {
        for _ in 0..2_000u64 {
            match server.recv_timeout(Duration::from_millis(1)).await {
                Ok(req) => {
                    let _ = req.respond(1);
                }
                Err(boundary::BoundaryError::Timeout) => {}
                Err(boundary::BoundaryError::Closed) => break,
                Err(err) => panic!("unexpected boundary error: {err:?}"),
            }
        }
    });

    let _ = producer.await;
    let _ = consumer.await;

    let stats = boundary::stats_snapshot();
    assert!(
        stats.timed_out > 0 || stats.canceled > 0 || stats.overloaded > 0,
        "expected soak run to hit timeout/cancel/overload paths"
    );
}
