use futures::executor::block_on;
use spargio_quic::{
    NativeProtoDriver, NativeProtoDriverOptions, NativeProtoFaultSpec, QuicEndpoint,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

#[test]
#[ignore = "long-window soak; run in nightly qualification lane"]
fn soak_connection_churn_roundtrip_stays_stable() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server(server_config, localhost_addr(0)).expect("server endpoint");
    let mut client = QuicEndpoint::client(localhost_addr(0)).expect("client endpoint");
    client.set_default_client_config(client_config);
    let server_addr = server.local_addr().expect("server addr");

    block_on(async {
        for _ in 0..200usize {
            let server_task = async {
                let conn = server
                    .accept()
                    .await
                    .expect("accept")
                    .expect("incoming connection");
                let (mut send, mut recv) = conn.accept_bi().await.expect("accept bi");
                let payload = recv.read_to_end(1024).await.expect("read");
                send.write_all(&payload).await.expect("write");
                send.finish().expect("finish");
                conn.close(0, b"done");
            };
            let client_task = async {
                let conn = client
                    .connect(server_addr, "localhost")
                    .await
                    .expect("connect");
                let (mut send, mut recv) = conn.open_bi().await.expect("open bi");
                send.write_all(b"ping").await.expect("write");
                send.finish().expect("finish");
                let echoed = recv.read_to_end(1024).await.expect("read");
                assert_eq!(echoed, b"ping");
                conn.close(0, b"done");
            };
            futures::join!(server_task, client_task);
        }
    });

    let server_metrics = server.metrics_snapshot();
    let client_metrics = client.metrics_snapshot();
    assert!(server_metrics.accepts_succeeded >= 200);
    assert!(client_metrics.connects_succeeded >= 200);
    assert_eq!(server_metrics.backpressure_rejections, 0);
    assert_eq!(client_metrics.backpressure_rejections, 0);
}

#[test]
#[ignore = "long-window soak; run in nightly qualification lane"]
fn soak_native_fault_injection_keeps_egress_queue_bounded() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let options = NativeProtoDriverOptions::default().with_max_pending_transmits(16);

    block_on(async {
        let driver = NativeProtoDriver::start(&rt.handle(), options)
            .await
            .expect("start native driver");
        driver
            .set_fault_spec(
                NativeProtoFaultSpec::default()
                    .with_drop_egress(true)
                    .with_reorder_egress(true),
            )
            .await
            .expect("set fault spec");

        for _ in 0..5_000usize {
            let _ = driver
                .submit_datagram(localhost_addr(5000), vec![0u8; 12])
                .await;
            let _ = driver.drain_transmits(8).await.expect("drain");
        }

        let tail = driver.drain_transmits(64).await.expect("final drain");
        assert!(tail.len() <= 16, "egress queue exceeded configured bound");
        let fault_stats = driver.fault_stats().await.expect("fault stats");
        assert!(fault_stats.egress_dropped > 0 || fault_stats.egress_reorders > 0);
    });
}

fn localhost_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
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
