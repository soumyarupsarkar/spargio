use futures::executor::block_on;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use spargio::net::TcpListener;
use spargio_tls::{HandshakeOptions, TlsAcceptor, TlsConnector};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn tls_connector_connect_socket_addr_timeout_is_enforced() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let listener = TcpListener::bind_socket_addr(handle.clone(), bind_addr)
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                async move {
                    let (_stream, _) = listener.accept().await.expect("accept");
                    spargio::sleep(Duration::from_millis(100)).await;
                }
            })
            .expect("spawn");

        let config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth(),
        );
        let connector = TlsConnector::new(config)
            .with_options(HandshakeOptions::default().with_timeout(Duration::from_millis(10)));

        let err = connector
            .connect_socket_addr(
                handle.clone(),
                addr,
                ServerName::try_from("localhost")
                    .expect("server name")
                    .to_owned(),
            )
            .await
            .expect_err("expected timeout");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        server.await.expect("server");
    });
}

#[test]
fn tls_connector_and_acceptor_interop_roundtrip() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let certified = generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
        let cert_der = certified.cert.der().clone();
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

        let server_config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der.clone()], key_der)
                .expect("server config"),
        );
        let mut roots = RootCertStore::empty();
        roots.add(cert_der).expect("add root");
        let client_config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let connector = TlsConnector::new(client_config)
            .with_options(HandshakeOptions::default().with_timeout(Duration::from_millis(250)));
        let acceptor = TlsAcceptor::new(server_config)
            .with_options(HandshakeOptions::default().with_timeout(Duration::from_millis(250)));

        let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let listener = TcpListener::bind_socket_addr(handle.clone(), bind_addr)
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                let acceptor = acceptor.clone();
                async move {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let mut tls = acceptor.accept(stream).await.expect("tls accept");
                    let mut in_buf = [0u8; 5];
                    tls.read_exact(&mut in_buf).await.expect("read");
                    tls.write_all(&in_buf).await.expect("write");
                    tls.flush().await.expect("flush");
                }
            })
            .expect("spawn");

        let mut client = connector
            .connect_socket_addr(
                handle.clone(),
                addr,
                ServerName::try_from("localhost")
                    .expect("server name")
                    .to_owned(),
            )
            .await
            .expect("tls connect");

        client.write_all(b"hello").await.expect("client write");
        client.flush().await.expect("client flush");
        let mut out = [0u8; 5];
        client.read_exact(&mut out).await.expect("client read");
        assert_eq!(&out, b"hello");

        server.await.expect("server");
    });
}

#[test]
fn tls_connector_rejects_server_name_mismatch() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let certified = generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
        let cert_der = certified.cert.der().clone();
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

        let server_config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der.clone()], key_der)
                .expect("server config"),
        );
        let mut roots = RootCertStore::empty();
        roots.add(cert_der).expect("add root");
        let client_config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let connector = TlsConnector::new(client_config)
            .with_options(HandshakeOptions::default().with_timeout(Duration::from_millis(250)));
        let acceptor = TlsAcceptor::new(server_config)
            .with_options(HandshakeOptions::default().with_timeout(Duration::from_millis(250)));

        let bind_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let listener = TcpListener::bind_socket_addr(handle.clone(), bind_addr)
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                let acceptor = acceptor.clone();
                async move {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let _ = acceptor.accept(stream).await;
                }
            })
            .expect("spawn");

        let err = connector
            .connect_socket_addr(
                handle.clone(),
                addr,
                ServerName::try_from("example.com")
                    .expect("server name")
                    .to_owned(),
            )
            .await
            .expect_err("expected name mismatch");
        assert_ne!(err.kind(), io::ErrorKind::TimedOut);
        assert_ne!(err.kind(), io::ErrorKind::WouldBlock);
        server.await.expect("server");
    });
}
