use futures::channel::oneshot;
use futures::executor::block_on;
use spargio_quic::quinn::{self, Endpoint};
use spargio_quic::{QuicEndpoint, QuicEndpointOptions};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::thread;

#[test]
fn interop_spargio_client_to_raw_quinn_server_bi_stream() {
    let (server_config, client_config) = test_server_and_client_configs();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let server_thread = thread::spawn(move || -> io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;
        runtime.block_on(async move {
            let endpoint =
                Endpoint::server(server_config, localhost_addr(0)).map_err(io::Error::other)?;
            let addr = endpoint.local_addr().map_err(io::Error::other)?;
            addr_tx
                .send(addr)
                .map_err(|err| io::Error::other(format!("send addr failed: {err}")))?;

            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "server accept closed"))?;
            let conn = incoming.await.map_err(io::Error::other)?;
            let (mut send, mut recv) = conn.accept_bi().await.map_err(io::Error::other)?;
            let got = recv.read_to_end(1024).await.map_err(io::Error::other)?;
            if got != b"ping" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "server expected ping",
                ));
            }
            send.write_all(b"pong").await.map_err(io::Error::other)?;
            send.finish().map_err(io::Error::other)?;
            endpoint.wait_idle().await;
            Ok(())
        })
    });

    let server_addr = addr_rx.recv().expect("receive server addr");
    let client =
        QuicEndpoint::client_with_options(localhost_addr(0), QuicEndpointOptions::default())
            .expect("spargio client endpoint");
    let mut client = client;
    client.set_default_client_config(client_config);

    block_on(async {
        let conn = client
            .connect(server_addr, "localhost")
            .await
            .expect("spargio connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open bi");
        send.write_all(b"ping").await.expect("write ping");
        send.finish().expect("finish");
        let out = recv.read_to_end(1024).await.expect("read pong");
        assert_eq!(out, b"pong");
        conn.close(0, b"done");
    });

    server_thread
        .join()
        .expect("join raw quinn server")
        .expect("raw quinn server result");
}

#[test]
fn interop_raw_quinn_client_to_spargio_server_bi_stream() {
    let (server_config, client_config) = test_server_and_client_configs();
    let server = QuicEndpoint::server_with_options(
        server_config,
        localhost_addr(0),
        QuicEndpointOptions::default(),
    )
    .expect("spargio server endpoint");
    let server_addr = server.local_addr().expect("server addr");

    let (done_tx, done_rx) = oneshot::channel::<()>();
    let client_thread = thread::spawn(move || -> io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io::Error::other)?;
        runtime.block_on(async move {
            let endpoint = Endpoint::client(localhost_addr(0)).map_err(io::Error::other)?;
            let mut endpoint = endpoint;
            endpoint.set_default_client_config(client_config);
            let conn = endpoint
                .connect(server_addr, "localhost")
                .map_err(io::Error::other)?
                .await
                .map_err(io::Error::other)?;
            let (mut send, mut recv) = conn.open_bi().await.map_err(io::Error::other)?;
            send.write_all(b"hello").await.map_err(io::Error::other)?;
            send.finish().map_err(io::Error::other)?;
            let out = recv.read_to_end(1024).await.map_err(io::Error::other)?;
            if out != b"world" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "client expected world",
                ));
            }
            let _ = done_tx.send(());
            endpoint.wait_idle().await;
            Ok(())
        })
    });

    block_on(async {
        let conn = server
            .accept()
            .await
            .expect("spargio accept")
            .expect("incoming");
        let (mut send, mut recv) = conn.accept_bi().await.expect("accept bi");
        let payload = recv.read_to_end(1024).await.expect("read hello");
        assert_eq!(payload, b"hello");
        send.write_all(b"world").await.expect("write world");
        send.finish().expect("finish");
        let _ = done_rx.await;
        conn.close(0, b"done");
    });

    client_thread
        .join()
        .expect("join raw quinn client")
        .expect("raw quinn client result");
}

fn localhost_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn test_server_and_client_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
    let priv_key =
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into();

    let server_config = quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], priv_key)
        .expect("server config");

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).expect("add root cert");
    let client_config =
        quinn::ClientConfig::with_root_certificates(Arc::new(roots)).expect("client config");

    (server_config, client_config)
}
