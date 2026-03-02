use futures::executor::block_on;
use spargio_signal::SignalHub;
use std::time::Duration;

#[test]
fn signal_hub_broadcasts_to_multiple_subscribers() {
    let hub = SignalHub::new([signal_hook::consts::SIGUSR1]).expect("hub");
    let stream_a = hub.subscribe();
    let stream_b = hub.subscribe();

    signal_hook::low_level::raise(signal_hook::consts::SIGUSR1).expect("raise");

    block_on(async {
        let a = spargio::timeout(Duration::from_millis(250), stream_a.recv())
            .await
            .expect("a timeout")
            .expect("a recv");
        let b = spargio::timeout(Duration::from_millis(250), stream_b.recv())
            .await
            .expect("b timeout")
            .expect("b recv");
        assert_eq!(a, signal_hook::consts::SIGUSR1);
        assert_eq!(b, signal_hook::consts::SIGUSR1);
    });
}

#[test]
fn signal_stream_recv_timeout_returns_none() {
    let hub = SignalHub::new([signal_hook::consts::SIGWINCH]).expect("hub");
    let stream = hub.subscribe();
    let got = block_on(async {
        stream
            .recv_timeout(Duration::from_millis(10))
            .await
            .expect("recv timeout")
    });
    assert_eq!(got, None::<i32>);
}

#[test]
fn ctrl_c_stream_still_constructs() {
    let stream = spargio_signal::ctrl_c().expect("ctrl_c");
    let err = block_on(async {
        spargio::timeout(Duration::from_millis(1), stream.recv())
            .await
            .expect_err("expected timeout")
    });
    assert_eq!(err, spargio::TimeoutError);
}

#[test]
fn signal_stream_recv_matching_filters_unwanted_signal() {
    let hub =
        SignalHub::new([signal_hook::consts::SIGUSR1, signal_hook::consts::SIGUSR2]).expect("hub");
    let stream = hub.subscribe();

    signal_hook::low_level::raise(signal_hook::consts::SIGUSR1).expect("raise usr1");
    signal_hook::low_level::raise(signal_hook::consts::SIGUSR2).expect("raise usr2");

    let got = block_on(async {
        spargio::timeout(
            Duration::from_millis(250),
            stream.recv_matching(&[signal_hook::consts::SIGUSR2]),
        )
        .await
        .expect("timeout")
        .expect("recv matching")
    });
    assert_eq!(got, signal_hook::consts::SIGUSR2);
}
