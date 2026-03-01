use futures::executor::block_on;
use futures::channel::oneshot;
use spargio_quic::{
    NativeProtoDriver, NativeProtoDriverOptions, NativeProtoTransportTuning, NativeProtoTransmit,
    QuicBridge, QuicEndpoint, QuicEndpointOptions, QuicMetricsSnapshot, QuicOptions,
};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
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

#[test]
fn quic_endpoint_connects_and_exchanges_uni_stream_data() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);

    let server_addr = server.local_addr().expect("server addr");
    block_on(async {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let server_task = async {
            let conn = server
                .accept()
                .await
                .expect("accept")
                .expect("incoming connection");
            let (mut send, mut recv) = conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(1024).await.expect("read");
            assert_eq!(msg, b"ping");
            send.write_all(b"pong").await.expect("write");
            send.finish().expect("finish");
            let _ = done_rx.await;
        };

        let client_task = async {
            let conn = client
                .connect(server_addr, "localhost")
                .await
                .expect("connect");
            let (mut send, mut recv) = conn.open_bi().await.expect("open bi");
            send.write_all(b"ping").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(1024).await.expect("read");
            assert_eq!(msg, b"pong");
            let _ = done_tx.send(());
        };

        futures::join!(server_task, client_task);
    });
}

#[test]
fn quic_endpoint_datagram_roundtrip_updates_metrics() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);

    let server_addr = server.local_addr().expect("server addr");
    block_on(async {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let server_task = async {
            let conn = server
                .accept()
                .await
                .expect("accept")
                .expect("incoming connection");
            let incoming = conn.read_datagram().await.expect("read datagram");
            assert_eq!(incoming, b"hello-dgram");
            conn.send_datagram(b"ack-dgram".to_vec())
                .expect("send datagram");
            let _ = done_rx.await;
        };

        let client_task = async {
            let conn = client
                .connect(server_addr, "localhost")
                .await
                .expect("connect");
            conn.send_datagram(b"hello-dgram".to_vec())
                .expect("send datagram");
            let incoming = conn.read_datagram().await.expect("read datagram");
            assert_eq!(incoming, b"ack-dgram");
            let _ = done_tx.send(());
        };

        futures::join!(server_task, client_task);
    });

    let server_snapshot = server.metrics_snapshot();
    let client_snapshot = client.metrics_snapshot();
    assert!(server_snapshot.accepts_succeeded >= 1);
    assert!(server_snapshot.datagrams_received >= 1);
    assert!(server_snapshot.datagrams_sent >= 1);
    assert!(client_snapshot.connects_succeeded >= 1);
    assert!(client_snapshot.datagrams_sent >= 1);
    assert!(client_snapshot.datagrams_received >= 1);
}

#[test]
fn quic_endpoint_accept_backpressure_is_enforced() {
    let (server_config, _client_config) = test_server_and_client_configs();
    let options = QuicEndpointOptions::default()
        .with_accept_timeout(Duration::from_millis(50))
        .with_max_inflight_ops(1);
    let server =
        QuicEndpoint::server_with_options(server_config, localhost_addr(0), options).expect("server");

    block_on(async {
        let (a, b) = futures::join!(server.accept(), server.accept());
        let mut saw_timeout = false;
        let mut saw_would_block = false;
        for err in [a.err().expect("first err"), b.err().expect("second err")] {
            if err.kind() == io::ErrorKind::TimedOut {
                saw_timeout = true;
            }
            if err.kind() == io::ErrorKind::WouldBlock {
                saw_would_block = true;
            }
        }
        assert!(saw_timeout, "expected one accept timeout");
        assert!(saw_would_block, "expected one accept backpressure error");
    });
}

#[test]
fn quic_connection_local_to_send_handoff_preserves_identity() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let server_task = async {
            let conn = server
                .accept()
                .await
                .expect("accept")
                .expect("incoming connection");
            let (mut send, mut recv) = conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(1024).await.expect("read");
            assert_eq!(msg, b"handoff");
            send.write_all(b"ack").await.expect("write");
            send.finish().expect("finish");
            let _ = done_rx.await;
        };

        let client_task = async {
            let conn = client
                .connect(server_addr, "localhost")
                .await
                .expect("connect");
            let local = conn.to_local();
            let send_handle = local.to_send_handle();
            assert_eq!(conn.stable_id(), local.stable_id());
            assert_eq!(conn.stable_id(), send_handle.stable_id());

            let (mut send, mut recv) = local.open_bi().await.expect("open bi");
            send.write_all(b"handoff").await.expect("write");
            send.finish().expect("finish");
            let ack = recv.read_to_end(16).await.expect("read");
            assert_eq!(ack, b"ack");
            let _ = done_tx.send(());
            drop(send_handle);
        };

        futures::join!(server_task, client_task);
    });
}

#[test]
fn quic_endpoint_metrics_snapshot_has_expected_counters() {
    let snapshot = QuicMetricsSnapshot::default();
    assert_eq!(snapshot.connects_started, 0);
    assert_eq!(snapshot.connects_succeeded, 0);
    assert_eq!(snapshot.datagrams_sent, 0);
}

#[test]
fn native_proto_driver_runs_on_owner_shard() {
    let rt = spargio::Runtime::builder()
        .shards(2)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_owner_shard(1);

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");
        let probe = driver.probe().await.expect("probe");
        assert_eq!(usize::from(probe.owner_shard), 1);
        assert_eq!(probe.owner_shard, probe.executing_shard);
        assert_eq!(probe.endpoint_id, driver.endpoint_id());
    });
}

#[test]
fn native_proto_driver_stable_ids_are_monotonic() {
    let rt = spargio::Runtime::builder()
        .shards(2)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_owner_shard(0);

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");
        let a = driver.allocate_connection_id().await.expect("conn id");
        let b = driver.allocate_connection_id().await.expect("conn id");
        let s0 = driver.allocate_stream_id().await.expect("stream id");
        let s1 = driver.allocate_stream_id().await.expect("stream id");
        assert!(b > a);
        assert!(s1 > s0);
    });
}

#[test]
fn native_proto_driver_rejects_commands_after_shutdown() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        driver.shutdown().await.expect("shutdown");
        let err = driver.probe().await.expect_err("probe should fail after shutdown");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn native_proto_driver_ingests_datagrams_and_supports_bounded_drain() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");

        let ingested = driver
            .submit_datagram(localhost_addr(4444), vec![0u8; 64])
            .await
            .expect("submit datagram");
        assert!(ingested.generated_transmits <= 1);

        let drained = driver.drain_transmits(4).await.expect("drain");
        assert!(drained.len() <= 4);
    });
}

#[test]
fn native_proto_driver_egress_queue_applies_backpressure() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_max_pending_transmits(1);

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");
        let tx = NativeProtoTransmit {
            destination: localhost_addr(5555),
            ecn: None,
            size: 42,
            segment_size: None,
            src_ip: None,
        };
        driver
            .enqueue_transmit_for_test(tx.clone())
            .await
            .expect("first enqueue");
        let err = driver
            .enqueue_transmit_for_test(tx)
            .await
            .expect_err("second enqueue should backpressure");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    });
}

#[test]
fn native_proto_driver_drain_is_fifo_and_batch_limited() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_max_pending_transmits(8);

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");
        for size in [10usize, 20, 30] {
            driver
                .enqueue_transmit_for_test(NativeProtoTransmit {
                    destination: localhost_addr(7000 + u16::try_from(size).expect("size fits")),
                    ecn: None,
                    size,
                    segment_size: None,
                    src_ip: None,
                })
                .await
                .expect("enqueue");
        }

        let first = driver.drain_transmits(2).await.expect("drain first");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].size, 10);
        assert_eq!(first[1].size, 20);

        let second = driver.drain_transmits(2).await.expect("drain second");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].size, 30);
    });
}

#[test]
fn native_proto_driver_timers_fire_when_deadline_passes() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");

        let generation = driver
            .schedule_timeout(Duration::from_millis(50))
            .await
            .expect("schedule");
        assert!(generation > 0);

        let state_before = driver
            .advance_clock_for_test(Duration::from_millis(20))
            .await
            .expect("advance clock");
        assert_eq!(state_before.timeout_fires, 0);
        assert!(state_before.next_deadline.is_some());

        let state_after = driver
            .advance_clock_for_test(Duration::from_millis(40))
            .await
            .expect("advance clock");
        assert_eq!(state_after.timeout_fires, 1);
        assert!(state_after.next_deadline.is_none());
    });
}

#[test]
fn native_proto_driver_newer_deadline_supersedes_older() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");

        let first = driver
            .schedule_timeout(Duration::from_millis(100))
            .await
            .expect("first deadline");
        let second = driver
            .schedule_timeout(Duration::from_millis(10))
            .await
            .expect("second deadline");
        assert!(second > first);

        let state = driver
            .advance_clock_for_test(Duration::from_millis(20))
            .await
            .expect("advance");
        assert_eq!(state.timeout_fires, 1);
        assert_eq!(state.last_fired_generation, Some(second));

        let later = driver
            .advance_clock_for_test(Duration::from_millis(200))
            .await
            .expect("advance");
        assert_eq!(later.timeout_fires, 1);
    });
}

#[test]
fn native_proto_driver_open_uni_roundtrips_to_accept_uni() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let conn = driver
            .register_connection_for_test()
            .await
            .expect("register conn");
        let opened = driver.open_uni_on_connection(conn).await.expect("open uni");
        let accepted = driver.accept_uni_on_connection(conn).await.expect("accept uni");
        assert_eq!(accepted, opened);
    });
}

#[test]
fn native_proto_driver_open_bi_roundtrips_to_accept_bi() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let conn = driver
            .register_connection_for_test()
            .await
            .expect("register conn");
        let opened = driver.open_bi_on_connection(conn).await.expect("open bi");
        let accepted = driver.accept_bi_on_connection(conn).await.expect("accept bi");
        assert_eq!(accepted, opened);
    });
}

#[test]
fn native_proto_driver_finish_and_reset_stream_are_observable() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let conn = driver
            .register_connection_for_test()
            .await
            .expect("register conn");
        let stream = driver.open_uni_on_connection(conn).await.expect("open uni");

        driver
            .finish_stream(conn, stream)
            .await
            .expect("finish stream");
        let finished = driver.stream_state(conn, stream).await.expect("state");
        assert!(finished.finished);
        assert!(!finished.reset);

        driver.reset_stream(conn, stream).await.expect("reset stream");
        let reset = driver.stream_state(conn, stream).await.expect("state");
        assert!(reset.finished);
        assert!(reset.reset);
    });
}

#[test]
fn native_proto_driver_local_send_handoff_preserves_identity() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let local = driver.to_local();
        let send = local.to_send_handle();
        assert_eq!(driver.endpoint_id(), local.endpoint_id());
        assert_eq!(driver.endpoint_id(), send.endpoint_id());

        let conn = local
            .register_connection_for_test()
            .await
            .expect("register conn");
        let opened = local
            .open_uni_on_connection(conn)
            .await
            .expect("open uni");
        let accepted = send
            .accept_uni_on_connection(conn)
            .await
            .expect("accept uni");
        assert_eq!(opened, accepted);
    });
}

#[test]
fn native_proto_driver_send_handle_respects_shutdown() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let send = driver.to_send_handle();
        send.shutdown().await.expect("shutdown");
        let err = send
            .probe()
            .await
            .expect_err("probe should fail after shutdown");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn native_proto_driver_transport_tuning_roundtrip() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");

        let tuning = NativeProtoTransportTuning::default()
            .with_max_datagram_size(48)
            .with_send_window(128 * 1024)
            .with_receive_window(64 * 1024)
            .with_keep_alive_interval(Some(Duration::from_millis(250)))
            .with_mtu_discovery_enabled(false);
        driver
            .set_transport_tuning(tuning)
            .await
            .expect("set tuning");
        let got = driver.transport_tuning().await.expect("get tuning");
        assert_eq!(got, tuning);
    });
}

#[test]
fn native_proto_driver_rejects_oversized_datagram_per_tuning() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");

        driver
            .set_transport_tuning(NativeProtoTransportTuning::default().with_max_datagram_size(16))
            .await
            .expect("set tuning");
        let err = driver
            .submit_datagram(localhost_addr(5556), vec![1u8; 64])
            .await
            .expect_err("oversized datagram should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    });
}

fn localhost_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn test_server_and_client_configs() -> (spargio_quic::quinn::ServerConfig, spargio_quic::quinn::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
    let priv_key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into();

    let server_config = spargio_quic::quinn::ServerConfig::with_single_cert(
        vec![cert_der.clone()],
        priv_key,
    )
    .expect("server config");

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("add root cert");
    let client_config =
        spargio_quic::quinn::ClientConfig::with_root_certificates(Arc::new(roots))
            .expect("client config");

    (server_config, client_config)
}
