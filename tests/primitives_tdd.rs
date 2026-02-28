use futures::executor::block_on;
use spargio::{
    CancellationToken, MissedTickBehavior, Runtime, Sleep, TaskGroup, TaskPlacement, TimeoutError,
    interval, interval_at, sleep, sleep_until, timeout, timeout_at,
};
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_waits_for_requested_duration() {
    let start = Instant::now();
    sleep(Duration::from_millis(15)).await;
    assert!(
        start.elapsed() >= Duration::from_millis(10),
        "sleep returned too early: {:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_returns_err_when_deadline_expires() {
    let out = timeout(Duration::from_millis(10), async {
        sleep(Duration::from_millis(40)).await;
        7usize
    })
    .await;

    assert!(matches!(out, Err(TimeoutError)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_returns_value_before_deadline() {
    let out = timeout(Duration::from_millis(100), async { 11usize }).await;
    match out {
        Ok(value) => assert_eq!(value, 11),
        Err(err) => panic!("unexpected timeout: {err:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_until_waits_for_deadline() {
    let deadline = Instant::now() + Duration::from_millis(15);
    sleep_until(deadline).await;
    assert!(
        Instant::now() >= deadline,
        "sleep_until returned before deadline"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_at_returns_err_when_deadline_expires() {
    let out = timeout_at(Instant::now() + Duration::from_millis(10), async {
        sleep(Duration::from_millis(40)).await;
        7usize
    })
    .await;

    assert!(matches!(out, Err(TimeoutError)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interval_ticks_with_configurable_missed_tick_behavior() {
    let mut ticker = interval(Duration::from_millis(10));
    let first = ticker.tick().await;
    let second = ticker.tick().await;
    assert!(second >= first);

    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    assert!(matches!(
        ticker.missed_tick_behavior(),
        MissedTickBehavior::Delay
    ));
    let _ = ticker.tick().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interval_at_uses_requested_start_deadline() {
    let start = Instant::now() + Duration::from_millis(12);
    let mut ticker = interval_at(start, Duration::from_millis(10));
    let first = ticker.tick().await;
    assert!(
        first >= start,
        "first tick should not occur before requested start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sleep_object_supports_deadline_reset_and_elapsed_state() {
    let mut sleeper = Sleep::new(Duration::from_millis(40));
    assert!(!sleeper.is_elapsed(), "new sleeper should not be elapsed");
    let sooner = Instant::now() + Duration::from_millis(10);
    sleeper.reset(sooner);
    assert_eq!(sleeper.deadline(), sooner);
    (&mut sleeper).await;
    assert!(sleeper.is_elapsed(), "sleep should be elapsed after await");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_token_notifies_waiters() {
    let token = CancellationToken::new();
    let waiter = token.clone();
    let task = tokio::spawn(async move {
        waiter.cancelled().await;
        1usize
    });

    token.cancel();
    let out = task.await.expect("waiter task");
    assert_eq!(out, 1);
    assert!(token.is_canceled());
}

#[test]
fn task_group_cancel_stops_pending_tasks() {
    let rt = Runtime::builder().shards(2).build().expect("runtime");
    let group = TaskGroup::new(rt.handle());

    let join = group
        .spawn_with_placement(TaskPlacement::StealablePreferred(0), async {
            sleep(Duration::from_millis(80)).await;
            99usize
        })
        .expect("spawn group task");
    group.cancel();

    let out = block_on(join).expect("group join");
    assert_eq!(out, None);
}

#[test]
fn task_group_completed_task_returns_value() {
    let rt = Runtime::builder().shards(1).build().expect("runtime");
    let group = TaskGroup::new(rt.handle());
    let join = group
        .spawn_with_placement(TaskPlacement::Pinned(0), async { 5usize })
        .expect("spawn");
    let out = block_on(join).expect("join");
    assert_eq!(out, Some(5));
}

#[test]
fn runtime_handle_spawn_blocking_executes_closure() {
    let rt = Runtime::builder().shards(1).build().expect("runtime");
    let join = rt
        .handle()
        .spawn_blocking(|| 123usize)
        .expect("spawn_blocking");
    let out = block_on(join).expect("join");
    assert_eq!(out, 123);
}
