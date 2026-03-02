use futures::executor::block_on;
use spargio_quic::{
    QuicBackend, QuicEndpoint, QuicEndpointOptions, bridge_runtime_context_enter_count,
    bridge_runtime_spawn_count, reset_bridge_runtime_context_enter_count,
    reset_bridge_runtime_spawn_count,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

static BRIDGE_COUNT_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn native_backend_data_path_avoids_bridge_task_spawn() {
    let _guard = BRIDGE_COUNT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_bridge_runtime_spawn_count();

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

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"native");
            send.write_all(b"ok").await.expect("write");
            send.finish().expect("finish");
        };
        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"native").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ok");
        };
        futures::join!(server_task, client_task);

        server_conn.close(0, b"done");
        client_conn.close(0, b"done");
        server.wait_idle().await.expect("server idle");
        client.wait_idle().await.expect("client idle");
    });

    assert_eq!(
        bridge_runtime_spawn_count(),
        0,
        "native backend should avoid bridge task spawn on data path"
    );
}

#[test]
fn bridge_backend_data_path_uses_bridge_task_spawn() {
    let _guard = BRIDGE_COUNT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_bridge_runtime_spawn_count();

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

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"bridge");
            send.write_all(b"ok").await.expect("write");
            send.finish().expect("finish");
        };
        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"bridge").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ok");
        };
        futures::join!(server_task, client_task);

        server_conn.close(0, b"done");
        client_conn.close(0, b"done");
        server.wait_idle().await.expect("server idle");
        client.wait_idle().await.expect("client idle");
    });

    assert!(
        bridge_runtime_spawn_count() >= 1,
        "bridge backend should use bridge task spawn on data path"
    );
}

#[test]
fn native_backend_endpoint_lifecycle_avoids_bridge_runtime_context_entry() {
    let _guard = BRIDGE_COUNT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_bridge_runtime_context_enter_count();

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

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ctx-native");
            send.write_all(b"ok").await.expect("write");
            send.finish().expect("finish");
        };
        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"ctx-native").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ok");
        };
        futures::join!(server_task, client_task);

        server_conn.close(0, b"done");
        client_conn.close(0, b"done");
        server.wait_idle().await.expect("server idle");
        client.wait_idle().await.expect("client idle");
    });

    drop(client);
    drop(server);

    assert_eq!(
        bridge_runtime_context_enter_count(),
        0,
        "native backend should avoid bridge runtime context entry"
    );
}

#[test]
fn bridge_backend_endpoint_lifecycle_uses_bridge_runtime_context_entry() {
    let _guard = BRIDGE_COUNT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_bridge_runtime_context_enter_count();

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

        let server_task = async {
            let (mut send, mut recv) = server_conn.accept_bi().await.expect("accept bi");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ctx-bridge");
            send.write_all(b"ok").await.expect("write");
            send.finish().expect("finish");
        };
        let client_task = async {
            let (mut send, mut recv) = client_conn.open_bi().await.expect("open bi");
            send.write_all(b"ctx-bridge").await.expect("write");
            send.finish().expect("finish");
            let msg = recv.read_to_end(256).await.expect("read");
            assert_eq!(msg, b"ok");
        };
        futures::join!(server_task, client_task);

        server_conn.close(0, b"done");
        client_conn.close(0, b"done");
        server.wait_idle().await.expect("server idle");
        client.wait_idle().await.expect("client idle");
    });

    drop(client);
    drop(server);

    assert!(
        bridge_runtime_context_enter_count() >= 2,
        "bridge backend should enter bridge runtime context for endpoint lifecycle"
    );
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
