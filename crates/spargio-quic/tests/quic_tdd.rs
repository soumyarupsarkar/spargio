use futures::executor::block_on;
use spargio_quic::{QuicBridge, QuicOptions};
use std::io;
use std::time::Duration;

#[test]
fn quic_bridge_runs_async_work() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let bridge = QuicBridge::new();

    let out = block_on(async {
        bridge
            .run(&rt.handle(), || async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok::<usize, io::Error>(7)
            })
            .await
            .expect("bridge run")
    });
    assert_eq!(out, 7);
}

#[test]
fn quic_bridge_timeout_is_enforced() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let bridge = QuicBridge::new()
        .with_options(QuicOptions::default().with_timeout(Duration::from_millis(5)));

    let err = block_on(async {
        bridge
            .run(&rt.handle(), || async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<usize, io::Error>(7)
            })
            .await
            .expect_err("timeout")
    });
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}
