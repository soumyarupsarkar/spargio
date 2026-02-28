use futures::executor::block_on;
use spargio_protocols::{BlockingOptions, tls_blocking_with_options};
use std::io;
use std::time::Duration;

#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::net::{TcpListener, TcpStream};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio_protocols::io_compat;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

#[test]
fn blocking_options_enforce_timeout() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    let err = block_on(async {
        tls_blocking_with_options(
            &rt.handle(),
            BlockingOptions::default().with_timeout(Duration::from_millis(5)),
            || {
                std::thread::sleep(Duration::from_millis(30));
                Ok::<usize, io::Error>(1)
            },
        )
        .await
        .expect_err("expected timeout")
    });

    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

#[test]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn futures_io_adapter_roundtrip_over_tcp_stream() {
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
                    let (stream, _) = listener.accept().await.expect("accept");
                    let mut buf = [0u8; 4];
                    stream.read_exact(&mut buf).await.expect("read");
                    stream.write_all(&buf).await.expect("write");
                }
            })
            .expect("spawn server");

        let stream = TcpStream::connect_socket_addr(handle.clone(), addr)
            .await
            .expect("connect");
        let mut compat = io_compat::FuturesTcpStream::new(stream);
        compat.write_all(b"ping").await.expect("write");
        compat.flush().await.expect("flush");
        let mut buf = [0u8; 4];
        compat.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"ping");

        server.await.expect("server join");
    });
}
