#[cfg(all(feature = "uring-native", target_os = "linux"))]
mod linux_uring_native_tests {
    use libc;
    use spargio::{BackendKind, Runtime, RuntimeError};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        path.push(format!("{prefix}-{}-{ts}.dat", std::process::id()));
        path
    }

    fn try_build_io_uring_runtime() -> Option<Runtime> {
        match Runtime::builder()
            .shards(1)
            .backend(BackendKind::IoUring)
            .build()
        {
            Ok(rt) => Some(rt),
            Err(RuntimeError::IoUringInit(_)) => None,
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        }
    }

    #[test]
    fn uring_native_lane_requires_io_uring_backend() {
        let rt = Runtime::builder()
            .shards(1)
            .backend(BackendKind::Queue)
            .build()
            .expect("runtime");
        match rt.handle().uring_native_lane(0) {
            Ok(_) => panic!("expected unsupported backend error"),
            Err(err) => assert!(matches!(err, RuntimeError::UnsupportedBackend(_))),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_lane_reads_file_at_offset() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("uring-native-read");
        let mut file = File::create(&path).expect("create file");
        file.write_all(b"hello-world").expect("seed data");
        drop(file);

        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open file");
        let lane = rt.handle().uring_native_lane(0).expect("native lane");

        let out = lane
            .read_at(file.as_raw_fd(), 6, 5)
            .await
            .expect("native read");

        assert_eq!(&out, b"world");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_lane_writes_file_at_offset() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("uring-native-write");
        let mut file = File::create(&path).expect("create file");
        file.write_all(b"abcdefghij").expect("seed data");
        drop(file);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file");
        let lane = rt.handle().uring_native_lane(0).expect("native lane");

        let wrote = lane
            .write_at(file.as_raw_fd(), 2, b"XYZ")
            .await
            .expect("native write");
        assert_eq!(wrote, 3);

        let mut check = OpenOptions::new().read(true).open(&path).expect("reopen");
        check.seek(SeekFrom::Start(0)).expect("seek");
        let mut data = String::new();
        check.read_to_string(&mut data).expect("read back");
        assert_eq!(data, "abXYZfghij");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_file_supports_write_read_and_fsync() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("uring-native-bound-file");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file");
        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_file(file);

        let wrote = bound.write_at(0, b"bound-file").await.expect("write");
        assert_eq!(wrote, 10);
        bound.fsync().await.expect("fsync");
        let out = bound.read_at(0, 10).await.expect("read");
        assert_eq!(&out, b"bound-file");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_tcp_stream_supports_send_and_recv() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_tcp_stream(server);

        let mut client_rx = client.try_clone().expect("clone");
        let mut client_tx = client;
        let sender = std::thread::spawn(move || {
            client_tx.write_all(b"ping").expect("client write");
            let mut buf = [0u8; 4];
            client_rx.read_exact(&mut buf).expect("client read");
            buf
        });

        let got = bound.recv(4).await.expect("recv");
        assert_eq!(&got, b"ping");
        let sent = bound.send(b"pong").await.expect("send");
        assert_eq!(sent, 4);

        let echoed = sender.join().expect("join sender");
        assert_eq!(&echoed, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_tcp_stream_supports_owned_send_and_recv_buffers() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_tcp_stream(server);

        let mut client_rx = client.try_clone().expect("clone");
        let mut client_tx = client;
        let sender = std::thread::spawn(move || {
            client_tx.write_all(b"ping").expect("client write");
            let mut buf = [0u8; 4];
            client_rx.read_exact(&mut buf).expect("client read");
            buf
        });

        let recv_buf = vec![0u8; 4];
        let (received, mut recv_buf) = bound.recv_owned(recv_buf).await.expect("recv_owned");
        assert_eq!(received, 4);
        assert_eq!(&recv_buf[..4], b"ping");

        recv_buf.copy_from_slice(b"pong");
        let (sent, send_buf) = bound.send_owned(recv_buf).await.expect("send_owned");
        assert_eq!(sent, 4);
        assert_eq!(&send_buf[..4], b"pong");

        let echoed = sender.join().expect("join sender");
        assert_eq!(&echoed, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_tcp_stream_supports_recv_into_and_send_batch() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_tcp_stream(server);

        let mut client_rx = client.try_clone().expect("clone");
        let mut client_tx = client;
        let sender = std::thread::spawn(move || {
            client_tx.write_all(b"ping").expect("client write");
            let mut out = [0u8; 4];
            client_rx.read_exact(&mut out).expect("client read");
            out
        });

        let (received, recv_buf) = bound.recv_into(vec![0u8; 4]).await.expect("recv_into");
        assert_eq!(received, 4);
        assert_eq!(&recv_buf[..4], b"ping");

        let send_bufs = vec![b"po".to_vec(), b"ng".to_vec()];
        let (sent, returned) = bound.send_batch(send_bufs, 2).await.expect("send_batch");
        assert_eq!(sent, 4);
        assert_eq!(returned.len(), 2);

        let echoed = sender.join().expect("join sender");
        assert_eq!(&echoed, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_tcp_stream_supports_recv_multishot() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_tcp_stream(server);

        let sender = std::thread::spawn(move || {
            client.write_all(b"pingpong").expect("client write");
        });

        let chunks = match bound.recv_multishot(1024, 8, 8).await {
            Ok(chunks) => chunks,
            Err(err) => {
                let raw = err.raw_os_error().unwrap_or_default();
                if raw == libc::EINVAL || raw == libc::ENOSYS || raw == libc::EOPNOTSUPP {
                    let _ = sender.join();
                    return;
                }
                panic!("recv_multishot failed unexpectedly: {err}");
            }
        };
        let _ = sender.join();

        let mut total = Vec::new();
        for chunk in chunks {
            total.extend_from_slice(&chunk);
        }
        assert!(
            total.starts_with(b"pingpong"),
            "got total bytes: {:?}",
            total
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_udp_socket_supports_send_and_recv() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let sock_a = UdpSocket::bind("127.0.0.1:0").expect("bind a");
        let sock_b = UdpSocket::bind("127.0.0.1:0").expect("bind b");
        sock_a
            .connect(sock_b.local_addr().expect("addr b"))
            .expect("connect a");
        sock_b
            .connect(sock_a.local_addr().expect("addr a"))
            .expect("connect b");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_udp_socket(sock_a);

        let sent = bound.send(b"ab").await.expect("send");
        assert_eq!(sent, 2);
        let mut buf = [0u8; 2];
        let got_b = sock_b.recv(&mut buf).expect("recv b");
        assert_eq!(got_b, 2);
        assert_eq!(&buf, b"ab");

        let wrote = sock_b.send(b"cd").expect("send b");
        assert_eq!(wrote, 2);
        let got = bound.recv(2).await.expect("recv");
        assert_eq!(&got, b"cd");
    }
}
