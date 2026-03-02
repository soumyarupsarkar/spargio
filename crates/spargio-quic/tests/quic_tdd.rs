use futures::channel::oneshot;
use futures::executor::block_on;
use spargio_quic::{
    NativeProtoDriver, NativeProtoDriverOptions, NativeProtoEvent, NativeProtoFaultSpec,
    NativeProtoPerfGate, NativeProtoRolloutStage, NativeProtoTransmit, NativeProtoTransportTuning,
    QuicBackend, QuicBridge, QuicEndpoint, QuicEndpointOptions, QuicMetricsSnapshot, QuicOptions,
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
    let server = QuicEndpoint::server_with_options(server_config, localhost_addr(0), options)
        .expect("server");

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
fn quic_endpoint_options_default_to_native_backend() {
    assert_eq!(
        QuicEndpointOptions::default().backend(),
        QuicBackend::Native
    );
}

#[test]
fn quic_endpoint_default_backend_dispatches_native_ops() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        let server_task = async {
            let conn = server
                .accept()
                .await
                .expect("accept")
                .expect("incoming connection");
            conn.close(0, b"done");
        };
        let client_task = async {
            let conn = client
                .connect(server_addr, "localhost")
                .await
                .expect("connect");
            conn.close(0, b"done");
        };
        futures::join!(server_task, client_task);
    });

    let server_snapshot = server.metrics_snapshot();
    let client_snapshot = client.metrics_snapshot();
    assert!(server_snapshot.native_ops_dispatched >= 1);
    assert_eq!(server_snapshot.bridge_ops_dispatched, 0);
    assert!(client_snapshot.native_ops_dispatched >= 1);
    assert_eq!(client_snapshot.bridge_ops_dispatched, 0);
}

#[test]
fn quic_endpoint_bridge_backend_dispatches_bridge_ops() {
    let (server_config, client_config) = test_server_and_client_configs();
    let options = QuicEndpointOptions::default().with_backend(QuicBackend::Bridge);
    let server = QuicEndpoint::server_with_options(server_config, localhost_addr(0), options)
        .expect("server endpoint");
    let mut client =
        QuicEndpoint::client_with_options(localhost_addr(0), options).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        let server_task = async {
            let conn = server
                .accept()
                .await
                .expect("accept")
                .expect("incoming connection");
            conn.close(0, b"done");
        };
        let client_task = async {
            let conn = client
                .connect(server_addr, "localhost")
                .await
                .expect("connect");
            conn.close(0, b"done");
        };
        futures::join!(server_task, client_task);
    });

    let server_snapshot = server.metrics_snapshot();
    let client_snapshot = client.metrics_snapshot();
    assert!(server_snapshot.bridge_ops_dispatched >= 1);
    assert!(client_snapshot.bridge_ops_dispatched >= 1);
}

#[test]
fn quic_connection_native_backend_dispatches_connection_ops() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        let (server_conn, client_conn) = futures::join!(
            async {
                server
                    .accept()
                    .await
                    .expect("accept")
                    .expect("incoming connection")
            },
            async { client.connect(server_addr, "localhost").await.expect("connect") },
        );

        let server_before = server.metrics_snapshot();
        let client_before = client.metrics_snapshot();

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(128).await.expect("read");
            assert_eq!(msg, b"hello");
            send.write_all(b"world").await.expect("write");
            send.finish().expect("finish");
        };

        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"hello").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(128).await.expect("read");
            assert_eq!(msg, b"world");
        };

        futures::join!(server_task, client_task);

        let server_after = server.metrics_snapshot();
        let client_after = client.metrics_snapshot();
        assert!(
            server_after.native_ops_dispatched > server_before.native_ops_dispatched,
            "expected native dispatch count to increase for server connection ops"
        );
        assert!(
            client_after.native_ops_dispatched > client_before.native_ops_dispatched,
            "expected native dispatch count to increase for client connection ops"
        );
    });
}

#[test]
fn quic_connection_bridge_backend_dispatches_connection_ops() {
    let (server_config, client_config) = test_server_and_client_configs();
    let options = QuicEndpointOptions::default().with_backend(QuicBackend::Bridge);
    let server = QuicEndpoint::server_with_options(server_config, localhost_addr(0), options)
        .expect("server endpoint");
    let mut client =
        QuicEndpoint::client_with_options(localhost_addr(0), options).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        let (server_conn, client_conn) = futures::join!(
            async {
                server
                    .accept()
                    .await
                    .expect("accept")
                    .expect("incoming connection")
            },
            async { client.connect(server_addr, "localhost").await.expect("connect") },
        );

        let server_before = server.metrics_snapshot();
        let client_before = client.metrics_snapshot();

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(128).await.expect("read");
            assert_eq!(msg, b"bridge-hello");
            send.write_all(b"bridge-world").await.expect("write");
            send.finish().expect("finish");
        };

        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"bridge-hello").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(128).await.expect("read");
            assert_eq!(msg, b"bridge-world");
        };

        futures::join!(server_task, client_task);

        let server_after = server.metrics_snapshot();
        let client_after = client.metrics_snapshot();
        assert!(
            server_after.bridge_ops_dispatched > server_before.bridge_ops_dispatched,
            "expected bridge dispatch count to increase for server connection ops"
        );
        assert!(
            client_after.bridge_ops_dispatched > client_before.bridge_ops_dispatched,
            "expected bridge dispatch count to increase for client connection ops"
        );
    });
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
        let err = driver
            .probe()
            .await
            .expect_err("probe should fail after shutdown");
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
            payload: vec![0; 42],
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
                    payload: vec![0; size],
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
        let accepted = driver
            .accept_uni_on_connection(conn)
            .await
            .expect("accept uni");
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
        let accepted = driver
            .accept_bi_on_connection(conn)
            .await
            .expect("accept bi");
        assert_eq!(accepted, opened);
    });
}

#[test]
fn native_proto_driver_closed_connection_rejects_stream_ops() {
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
        driver
            .close_connection_for_test(conn)
            .await
            .expect("close connection");

        let err = driver
            .open_uni_on_connection(conn)
            .await
            .expect_err("stream open after close should fail");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn native_proto_driver_connection_datagram_roundtrip_tracks_state() {
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
        driver
            .send_datagram_on_connection_for_test(conn, b"hello".to_vec())
            .await
            .expect("send dgram");
        let payload = driver
            .recv_datagram_on_connection_for_test(conn)
            .await
            .expect("recv dgram");
        assert_eq!(payload, b"hello");

        let state = driver.connection_state(conn).await.expect("state");
        assert!(!state.closed);
        assert_eq!(state.datagrams_sent, 1);
        assert_eq!(state.datagrams_received, 1);
    });
}

#[test]
fn native_proto_driver_connect_for_test_generates_initial_transmit() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (_server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let connection_id = driver
            .connect_for_test(client_config, localhost_addr(5557), "localhost")
            .await
            .expect("connect for test");

        let state = driver.connection_state(connection_id).await.expect("state");
        assert!(!state.closed);

        let transmits = driver.drain_transmits(64).await.expect("drain");
        assert!(
            !transmits.is_empty(),
            "client connect should produce initial protocol transmits"
        );
    });
}

#[test]
fn native_proto_driver_local_send_connect_for_test_roundtrips() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (_server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let local = driver.to_local();
        let send = local.to_send_handle();

        let connection_id = send
            .connect_for_test(client_config, localhost_addr(5558), "localhost")
            .await
            .expect("connect for test");
        let state = local.connection_state(connection_id).await.expect("state");
        assert!(!state.closed);

        let transmits = send.drain_transmits(64).await.expect("drain");
        assert!(
            !transmits.is_empty(),
            "connect_for_test via local/send wrappers should emit protocol transmits"
        );
    });
}

#[test]
fn native_proto_driver_connect_for_test_open_uni_respects_proto_stream_credit() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (_server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let connection_id = driver
            .connect_for_test(client_config, localhost_addr(5560), "localhost")
            .await
            .expect("connect for test");

        let err = driver
            .open_uni_on_connection(connection_id)
            .await
            .expect_err("open uni should follow proto stream credit");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    });
}

#[test]
fn native_proto_driver_connect_for_test_open_bi_respects_proto_stream_credit() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (_server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let connection_id = driver
            .connect_for_test(client_config, localhost_addr(5561), "localhost")
            .await
            .expect("connect for test");

        let err = driver
            .open_bi_on_connection(connection_id)
            .await
            .expect_err("open bi should follow proto stream credit");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    });
}

#[test]
fn native_proto_driver_close_connection_for_test_emits_close_transmit_for_proto_connection() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (_server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        let connection_id = driver
            .connect_for_test(client_config, localhost_addr(5562), "localhost")
            .await
            .expect("connect for test");
        let _ = driver.drain_transmits(64).await.expect("drain initial");

        driver
            .close_connection_for_test(connection_id)
            .await
            .expect("close connection");
        let close_transmits = driver.drain_transmits(64).await.expect("drain close");
        assert!(
            !close_transmits.is_empty(),
            "closing a protocol-backed connection should emit close transmits"
        );
    });
}

#[test]
fn native_proto_driver_server_config_accepts_client_transmits() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let server = NativeProtoDriver::start(
            &rt.handle(),
            NativeProtoDriverOptions::default().with_server_config(server_config),
        )
        .await
        .expect("start server native driver");
        let client = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start client native driver");

        let server_addr = localhost_addr(5650);
        let client_addr = localhost_addr(5651);
        let _client_conn = client
            .connect_for_test(client_config, server_addr, "localhost")
            .await
            .expect("connect for test");

        for _ in 0..64 {
            let mut progressed = false;

            let client_tx = client.drain_transmits(64).await.expect("drain client");
            for tx in client_tx {
                progressed = true;
                assert_eq!(tx.destination, server_addr);
                let _ = server
                    .submit_datagram(client_addr, tx.payload)
                    .await
                    .expect("deliver client->server");
            }

            let server_tx = server.drain_transmits(64).await.expect("drain server");
            for tx in server_tx {
                progressed = true;
                assert_eq!(tx.destination, client_addr);
                let _ = client
                    .submit_datagram(server_addr, tx.payload)
                    .await
                    .expect("deliver server->client");
            }

            if !progressed {
                break;
            }
        }

        let server_stats = server.stats().await.expect("server stats");
        assert!(
            server_stats.connections_registered >= 1,
            "server driver should register incoming connection when configured with server config"
        );
        let server_events = server.drain_events(64).await.expect("server events");
        assert!(
            server_events
                .iter()
                .any(|event| matches!(event, NativeProtoEvent::ConnectionRegistered { .. })),
            "server should emit connection-registered event for accepted incoming"
        );
    });
}

#[test]
fn native_proto_driver_post_handshake_bi_stream_open_is_accepted_by_server() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let server = NativeProtoDriver::start(
            &rt.handle(),
            NativeProtoDriverOptions::default().with_server_config(server_config),
        )
        .await
        .expect("start server native driver");
        let client = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start client native driver");

        let server_addr = localhost_addr(5660);
        let client_addr = localhost_addr(5661);
        let client_conn = client
            .connect_for_test(client_config, server_addr, "localhost")
            .await
            .expect("connect for test");

        exchange_driver_transmits(&client, client_addr, &server, server_addr, 64).await;
        let server_conn = server
            .drain_events(64)
            .await
            .expect("server events")
            .into_iter()
            .find_map(|event| match event {
                NativeProtoEvent::ConnectionRegistered { connection_id } => Some(connection_id),
                _ => None,
            })
            .expect("server connection id");

        let opened = client
            .open_bi_on_connection(client_conn)
            .await
            .expect("client open bi after handshake");
        client
            .finish_stream(client_conn, opened.0)
            .await
            .expect("client finish opened stream");
        exchange_driver_transmits(&client, client_addr, &server, server_addr, 64).await;
        let accepted = server
            .accept_bi_on_connection(server_conn)
            .await
            .expect("server accept bi");
        assert_eq!(accepted, opened);
    });
}

#[test]
fn native_proto_driver_remote_close_marks_peer_connection_closed() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let server = NativeProtoDriver::start(
            &rt.handle(),
            NativeProtoDriverOptions::default().with_server_config(server_config),
        )
        .await
        .expect("start server native driver");
        let client = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start client native driver");

        let server_addr = localhost_addr(5670);
        let client_addr = localhost_addr(5671);
        let client_conn = client
            .connect_for_test(client_config, server_addr, "localhost")
            .await
            .expect("connect for test");
        exchange_driver_transmits(&client, client_addr, &server, server_addr, 64).await;

        let server_conn = server
            .drain_events(64)
            .await
            .expect("server events")
            .into_iter()
            .find_map(|event| match event {
                NativeProtoEvent::ConnectionRegistered { connection_id } => Some(connection_id),
                _ => None,
            })
            .expect("server connection id");

        server
            .close_connection_for_test(server_conn)
            .await
            .expect("server close");
        exchange_driver_transmits(&server, server_addr, &client, client_addr, 64).await;

        let client_state = client.connection_state(client_conn).await.expect("client state");
        assert!(
            client_state.closed,
            "peer close should be reflected in client-side connection state"
        );
    });
}

#[test]
fn native_proto_driver_remote_close_emits_connection_closed_event() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let (server_config, client_config) = test_server_and_client_configs();
    block_on(async {
        let server = NativeProtoDriver::start(
            &rt.handle(),
            NativeProtoDriverOptions::default().with_server_config(server_config),
        )
        .await
        .expect("start server native driver");
        let client = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start client native driver");

        let server_addr = localhost_addr(5680);
        let client_addr = localhost_addr(5681);
        let client_conn = client
            .connect_for_test(client_config, server_addr, "localhost")
            .await
            .expect("connect for test");
        exchange_driver_transmits(&client, client_addr, &server, server_addr, 64).await;

        let server_conn = server
            .drain_events(64)
            .await
            .expect("server events")
            .into_iter()
            .find_map(|event| match event {
                NativeProtoEvent::ConnectionRegistered { connection_id } => Some(connection_id),
                _ => None,
            })
            .expect("server connection id");

        server
            .close_connection_for_test(server_conn)
            .await
            .expect("server close");
        exchange_driver_transmits(&server, server_addr, &client, client_addr, 64).await;

        let client_events = client.drain_events(64).await.expect("client events");
        assert!(
            client_events.iter().any(|event| matches!(
                event,
                NativeProtoEvent::ConnectionClosed { connection_id } if *connection_id == client_conn
            )),
            "peer close should emit ConnectionClosed event for client-side connection id"
        );
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

        driver
            .reset_stream(conn, stream)
            .await
            .expect("reset stream");
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
        let opened = local.open_uni_on_connection(conn).await.expect("open uni");
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
        let conn = driver
            .register_connection_for_test()
            .await
            .expect("register conn");
        let err = driver
            .send_datagram_on_connection_for_test(conn, vec![1u8; 64])
            .await
            .expect_err("oversized datagram should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    });
}

#[test]
fn native_proto_driver_stats_track_key_operations() {
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
        let _ = driver.open_uni_on_connection(conn).await.expect("open uni");
        let _ = driver.open_bi_on_connection(conn).await.expect("open bi");
        let _ = driver
            .submit_datagram(localhost_addr(12345), vec![0u8; 16])
            .await
            .expect("submit datagram");
        let _ = driver
            .schedule_timeout(Duration::from_millis(10))
            .await
            .expect("schedule");
        let _ = driver
            .advance_clock_for_test(Duration::from_millis(20))
            .await
            .expect("advance");

        let stats = driver.stats().await.expect("stats");
        assert!(stats.operations_total >= 6);
        assert_eq!(stats.connections_registered, 1);
        assert_eq!(stats.streams_opened_uni, 1);
        assert_eq!(stats.streams_opened_bi, 1);
        assert_eq!(stats.timeouts_fired, 1);
    });
}

#[test]
fn native_proto_driver_event_log_captures_timeout_and_backpressure() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_max_pending_transmits(1);
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");

        let _ = driver
            .schedule_timeout(Duration::from_millis(5))
            .await
            .expect("schedule");
        let _ = driver
            .advance_clock_for_test(Duration::from_millis(10))
            .await
            .expect("advance");

        let tx = NativeProtoTransmit {
            destination: localhost_addr(8888),
            ecn: None,
            size: 1,
            segment_size: None,
            src_ip: None,
            payload: vec![0; 1],
        };
        driver
            .enqueue_transmit_for_test(tx.clone())
            .await
            .expect("first enqueue");
        let _ = driver
            .enqueue_transmit_for_test(tx)
            .await
            .expect_err("backpressure");

        let events = driver.drain_events(16).await.expect("events");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, NativeProtoEvent::TimeoutFired { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, NativeProtoEvent::Backpressure { .. }))
        );
    });
}

#[test]
fn native_proto_driver_fault_injection_drops_ingress_and_tracks_stats() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        driver
            .set_fault_spec(NativeProtoFaultSpec::default().with_drop_inbound(true))
            .await
            .expect("set fault spec");

        let report = driver
            .submit_datagram(localhost_addr(9090), vec![0u8; 12])
            .await
            .expect("submit");
        assert_eq!(report.generated_transmits, 0);

        let stats = driver.stats().await.expect("stats");
        assert!(stats.operations_total >= 2);
        let fault_stats = driver.fault_stats().await.expect("fault stats");
        assert!(fault_stats.inbound_dropped >= 1);
    });
}

#[test]
fn native_proto_driver_reorders_egress_when_fault_enabled() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), NativeProtoDriverOptions::default())
            .await
            .expect("start native driver");
        driver
            .set_fault_spec(NativeProtoFaultSpec::default().with_reorder_egress(true))
            .await
            .expect("set fault spec");
        for size in [11usize, 22] {
            driver
                .enqueue_transmit_for_test(NativeProtoTransmit {
                    destination: localhost_addr(6000 + u16::try_from(size).expect("size fits")),
                    ecn: None,
                    size,
                    segment_size: None,
                    src_ip: None,
                    payload: vec![0; size],
                })
                .await
                .expect("enqueue");
        }
        let drained = driver.drain_transmits(2).await.expect("drain");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].size, 22);
        assert_eq!(drained[1].size, 11);
        let fault_stats = driver.fault_stats().await.expect("fault stats");
        assert!(fault_stats.egress_reorders >= 1);
    });
}

#[test]
fn native_proto_perf_gate_marks_material_regression_as_fail() {
    let gate = NativeProtoPerfGate::default()
        .with_max_p95_regression_pct(10.0)
        .with_max_p99_regression_pct(12.5);
    let verdict = gate.evaluate(100.0, 105.0, 100.0, 118.0);
    assert!(!verdict.pass);
    assert!(verdict.p99_regression_pct > gate.max_p99_regression_pct);
}

#[test]
fn native_proto_rollout_stage_is_experimental_for_now() {
    assert_eq!(
        NativeProtoDriver::rollout_stage(),
        NativeProtoRolloutStage::Experimental
    );
}

fn localhost_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

async fn exchange_driver_transmits(
    client: &NativeProtoDriver,
    client_addr: SocketAddr,
    server: &NativeProtoDriver,
    server_addr: SocketAddr,
    rounds: usize,
) {
    for _ in 0..rounds {
        let mut progressed = false;

        let client_tx = client.drain_transmits(64).await.expect("drain client");
        for tx in client_tx {
            progressed = true;
            assert_eq!(tx.destination, server_addr);
            let _ = server
                .submit_datagram(client_addr, tx.payload)
                .await
                .expect("deliver client->server");
        }

        let server_tx = server.drain_transmits(64).await.expect("drain server");
        for tx in server_tx {
            progressed = true;
            assert_eq!(tx.destination, client_addr);
            let _ = client
                .submit_datagram(server_addr, tx.payload)
                .await
                .expect("deliver server->client");
        }

        if !progressed {
            break;
        }
    }
}

fn test_server_and_client_configs() -> (
    spargio_quic::quinn::ServerConfig,
    spargio_quic::quinn::ClientConfig,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
    let priv_key =
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into();

    let server_config =
        spargio_quic::quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], priv_key)
            .expect("server config");

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("add root cert");
    let client_config = spargio_quic::quinn::ClientConfig::with_root_certificates(Arc::new(roots))
        .expect("client config");

    (server_config, client_config)
}
