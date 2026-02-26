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

    fn try_build_io_uring_runtime_shards(shards: usize) -> Option<Runtime> {
        match Runtime::builder()
            .shards(shards)
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

    #[test]
    fn uring_native_unbound_requires_io_uring_backend() {
        let rt = Runtime::builder()
            .shards(1)
            .backend(BackendKind::Queue)
            .build()
            .expect("runtime");
        match rt.handle().uring_native_unbound() {
            Ok(_) => panic!("expected unsupported backend error"),
            Err(err) => assert!(matches!(err, RuntimeError::UnsupportedBackend(_))),
        }
    }

    #[test]
    fn uring_native_unbound_selector_distributes_when_depths_equal() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let native_any = rt.handle().uring_native_unbound().expect("native any");
        let mut hits = [0usize; 2];
        for _ in 0..16 {
            let shard = native_any.select_shard(None).expect("select shard");
            if let Some(slot) = hits.get_mut(usize::from(shard)) {
                *slot += 1;
            }
        }
        assert!(hits[0] > 0, "expected shard 0 to be selected");
        assert!(hits[1] > 0, "expected shard 1 to be selected");
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
    async fn uring_bound_file_supports_read_at_into_reuse() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("uring-native-read-into");
        let mut file = File::create(&path).expect("create file");
        file.write_all(b"0123456789").expect("seed data");
        drop(file);

        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open file");
        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_file(file);

        let (got, buf) = bound
            .read_at_into(2, vec![0u8; 4])
            .await
            .expect("read_at_into");
        assert_eq!(got, 4);
        assert_eq!(&buf[..4], b"2345");

        let (got2, buf2) = bound
            .read_at_into(6, buf)
            .await
            .expect("read_at_into reuse");
        assert_eq!(got2, 4);
        assert_eq!(&buf2[..4], b"6789");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_file_session_supports_repeated_reads() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("uring-native-file-session");
        let mut file = File::create(&path).expect("create file");
        file.write_all(b"abcdefghij").expect("seed data");
        drop(file);

        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open file");
        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_file(file);
        let session = bound.start_file_session().expect("file session");

        let first = session.read_at(0, 4).await.expect("first read");
        assert_eq!(&first, b"abcd");

        let (got, buf) = session
            .read_at_into(6, vec![0u8; 4])
            .await
            .expect("second read");
        assert_eq!(got, 4);
        assert_eq!(&buf[..4], b"ghij");

        session.shutdown().await.expect("session shutdown");
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
    async fn uring_bound_tcp_stream_supports_send_all_batch() {
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
        let sender = std::thread::spawn(move || {
            let mut out = [0u8; 9];
            client_rx.read_exact(&mut out).expect("client read");
            out
        });

        let bufs = vec![b"hel".to_vec(), b"lo-".to_vec(), b"you".to_vec()];
        let (sent, returned) = bound.send_all_batch(bufs, 3).await.expect("send_all_batch");
        assert_eq!(sent, 9);
        assert_eq!(returned.len(), 3);
        assert_eq!(&returned[0], b"hel");
        assert_eq!(&returned[1], b"lo-");
        assert_eq!(&returned[2], b"you");

        let echoed = sender.join().expect("join sender");
        assert_eq!(&echoed, b"hello-you");
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
    async fn uring_bound_tcp_stream_supports_recv_multishot_segments() {
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
            client.write_all(b"segmented-data").expect("client write");
        });

        let out = match bound.recv_multishot_segments(1024, 8, 13).await {
            Ok(out) => out,
            Err(err) => {
                let raw = err.raw_os_error().unwrap_or_default();
                if raw == libc::EINVAL || raw == libc::ENOSYS || raw == libc::EOPNOTSUPP {
                    let _ = sender.join();
                    return;
                }
                panic!("recv_multishot_segments failed unexpectedly: {err}");
            }
        };
        let _ = sender.join();

        let mut flat = Vec::new();
        for seg in out.segments {
            let end = seg.offset.saturating_add(seg.len).min(out.buffer.len());
            if seg.offset < end {
                flat.extend_from_slice(&out.buffer[seg.offset..end]);
            }
        }
        assert!(flat.starts_with(b"segmented-data"), "got: {:?}", flat);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_bound_tcp_stream_reuses_recv_multishot_path_across_calls() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let lane = rt.handle().uring_native_lane(0).expect("native lane");
        let bound = lane.bind_tcp_stream(server);

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let sender = std::thread::spawn(move || {
            client.write_all(b"abcdefgh").expect("write first");
            let _ = rx.recv();
            client.write_all(b"ijklmnop").expect("write second");
        });

        let first = match bound.recv_multishot(1024, 8, 8).await {
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
        let first_bytes: usize = first.iter().map(|chunk| chunk.len()).sum();
        assert!(
            first_bytes >= 8,
            "expected at least 8 bytes, got {first_bytes}"
        );
        let _ = tx.send(());

        let second = bound
            .recv_multishot(1024, 8, 8)
            .await
            .expect("second recv_multishot");
        let second_bytes: usize = second.iter().map(|chunk| chunk.len()).sum();
        assert!(
            second_bytes >= 8,
            "expected at least 8 bytes on second call, got {second_bytes}"
        );

        let _ = sender.join();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_file_ops_work() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let path = unique_temp_path("uring-native-any-file");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"abcde12345").expect("seed");
        drop(file);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");
        let fd = file.as_raw_fd();
        let any = rt.handle().uring_native_unbound().expect("native any");

        let got = any.read_at(fd, 0, 5).await.expect("read");
        assert_eq!(&got, b"abcde");

        let wrote = any.write_at(fd, 5, b"XYZ").await.expect("write");
        assert_eq!(wrote, 3);
        any.fsync(fd).await.expect("fsync");

        let check = any.read_at(fd, 0, 10).await.expect("read all");
        assert_eq!(&check, b"abcdeXYZ45");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_stream_ops_preserve_affinity_and_order() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let any = rt.handle().uring_native_unbound().expect("native any");
        let fd = server.as_raw_fd();
        let mut client_rx = client.try_clone().expect("clone");
        let mut client_tx = client;
        let io_thread = std::thread::spawn(move || {
            client_tx.write_all(b"ping").expect("client write");
            let mut got = [0u8; 4];
            client_rx.read_exact(&mut got).expect("client read");
            got
        });

        let (received, recv_buf) = any.recv_owned(fd, vec![0u8; 4]).await.expect("recv");
        assert_eq!(received, 4);
        assert_eq!(&recv_buf[..4], b"ping");

        let (sent, returned) = any
            .send_all_batch(fd, vec![b"po".to_vec(), b"ng".to_vec()], 2)
            .await
            .expect("send all batch");
        assert_eq!(sent, 4);
        assert_eq!(returned.len(), 2);

        let affinity = any.fd_affinity_shard(fd);
        assert!(affinity.is_some(), "expected stream fd affinity lease");

        let echoed = io_thread.join().expect("join io");
        assert_eq!(&echoed, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_multishot_releases_hard_affinity_after_completion() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");
        let fd = server.as_raw_fd();
        let any = rt.handle().uring_native_unbound().expect("native any");

        let sender = std::thread::spawn(move || {
            client.write_all(b"multishot-data").expect("client write");
        });

        let out = match any.recv_multishot_segments(fd, 1024, 8, 14).await {
            Ok(out) => out,
            Err(err) => {
                let raw = err.raw_os_error().unwrap_or_default();
                if raw == libc::EINVAL || raw == libc::ENOSYS || raw == libc::EOPNOTSUPP {
                    let _ = sender.join();
                    return;
                }
                panic!("recv_multishot_segments failed unexpectedly: {err}");
            }
        };
        let _ = sender.join();
        assert!(!out.segments.is_empty(), "expected at least one segment");
        assert_eq!(
            any.fd_affinity_shard(fd),
            None,
            "hard multishot affinity should be released"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_tracks_active_op_routes_for_inflight_work() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");
        let fd = server.as_raw_fd();
        let any = rt.handle().uring_native_unbound().expect("native any");
        let recv_any = any.clone();

        let recv_task = tokio::spawn(async move { recv_any.recv(fd, 4).await.expect("recv") });

        let mut seen_active = false;
        for _ in 0..50 {
            if any.active_native_op_count() > 0 {
                seen_active = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            seen_active,
            "expected active unbound op route while recv is pending"
        );

        client.write_all(b"done").expect("send payload");
        let out = recv_task.await.expect("recv join");
        assert_eq!(&out, b"done");

        for _ in 0..50 {
            if any.active_native_op_count() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("expected active op routes to drain to zero");
    }
}
