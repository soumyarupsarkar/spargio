#[cfg(all(feature = "uring-native", target_os = "linux"))]
mod linux_uring_native_tests {
    use libc;
    use spargio::{BackendKind, Runtime, RuntimeError};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    fn uring_native_unbound_is_available_on_io_uring_backend() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let _ = rt.handle().uring_native_unbound().expect("native any");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_sleep_uses_timeout_path() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let any = rt.handle().uring_native_unbound().expect("native any");
        let start = Instant::now();
        any.sleep(Duration::from_millis(12)).await.expect("sleep");
        assert!(
            start.elapsed() >= Duration::from_millis(8),
            "native sleep returned too early: {:?}",
            start.elapsed()
        );
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

        let (count, reused) = any
            .read_at_into(fd, 5, vec![0u8; 5])
            .await
            .expect("read_at_into");
        assert_eq!(count, 5);
        assert_eq!(&reused[..5], b"12345");

        let wrote = any.write_at(fd, 5, b"XYZ").await.expect("write");
        assert_eq!(wrote, 3);
        any.fsync(fd).await.expect("fsync");

        let check = any.read_at(fd, 0, 10).await.expect("read all");
        assert_eq!(&check, b"abcdeXYZ45");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_writes_file_at_offset() {
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
        let any = rt.handle().uring_native_unbound().expect("native any");

        let wrote = any
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
    async fn uring_native_unbound_stream_supports_owned_send_and_recv_buffers() {
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
        let sender = std::thread::spawn(move || {
            client_tx.write_all(b"ping").expect("client write");
            let mut buf = [0u8; 4];
            client_rx.read_exact(&mut buf).expect("client read");
            buf
        });

        let recv_buf = vec![0u8; 4];
        let (received, mut recv_buf) = any.recv_owned(fd, recv_buf).await.expect("recv_owned");
        assert_eq!(received, 4);
        assert_eq!(&recv_buf[..4], b"ping");

        recv_buf.copy_from_slice(b"pong");
        let (sent, send_buf) = any.send_owned(fd, recv_buf).await.expect("send_owned");
        assert_eq!(sent, 4);
        assert_eq!(&send_buf[..4], b"pong");

        let echoed = sender.join().expect("join sender");
        assert_eq!(&echoed, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_tcp_stream_supports_recv_multishot() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");

        let any = rt.handle().uring_native_unbound().expect("native any");
        let fd = server.as_raw_fd();

        let sender = std::thread::spawn(move || {
            client.write_all(b"pingpong").expect("client write");
        });

        let chunks = match any.recv_multishot(fd, 1024, 8, 8).await {
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
    async fn uring_native_unbound_udp_socket_supports_send_and_recv() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
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

        let any = rt.handle().uring_native_unbound().expect("native any");
        let fd = sock_a.as_raw_fd();

        let sent = any.send(fd, b"ab").await.expect("send");
        assert_eq!(sent, 2);
        let mut buf = [0u8; 2];
        let got_b = sock_b.recv(&mut buf).expect("recv b");
        assert_eq!(got_b, 2);
        assert_eq!(&buf, b"ab");

        let wrote = sock_b.send(b"cd").expect("send b");
        assert_eq!(wrote, 2);
        let got = any.recv(fd, 2).await.expect("recv");
        assert_eq!(&got, b"cd");
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
    async fn uring_native_unbound_multishot_segments_use_compact_buffer_copy() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect client");
        let (server, _) = listener.accept().expect("accept");
        let fd = server.as_raw_fd();
        let any = rt.handle().uring_native_unbound().expect("native any");

        let buffer_len = 512usize;
        let buffer_count = 8u16;
        let total_pool_len = buffer_len * usize::from(buffer_count);
        let payload = vec![0x5Au8; 32];
        let sender = std::thread::spawn(move || {
            client.write_all(&payload).expect("client write");
        });

        let out = match any
            .recv_multishot_segments(fd, buffer_len, buffer_count, 32)
            .await
        {
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

        let touched: usize = out.segments.iter().map(|seg| seg.len).sum();
        assert!(
            out.buffer.len() <= touched,
            "expected compact multishot buffer copy (<= touched bytes), got {} with touched {}",
            out.buffer.len(),
            touched
        );
        assert!(
            out.buffer.len() < total_pool_len,
            "expected compact multishot buffer smaller than full pool clone, got {} vs pool {total_pool_len}",
            out.buffer.len()
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_records_command_envelope_submission() {
        let Some(rt) = try_build_io_uring_runtime_shards(2) else {
            return;
        };
        let path = unique_temp_path("uring-native-any-envelope");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"abcdef").expect("seed");
        drop(file);

        let file = OpenOptions::new().read(true).open(&path).expect("open");
        let fd = file.as_raw_fd();
        let any = rt.handle().uring_native_unbound().expect("native any");
        let got = any.read_at(fd, 0, 3).await.expect("read");
        assert_eq!(&got, b"abc");

        let stats = rt.handle().stats_snapshot();
        assert!(
            stats.native_any_envelope_submitted > 0,
            "expected command-envelope submissions to be recorded"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uring_native_unbound_records_local_fast_path_submission() {
        let Some(rt) = try_build_io_uring_runtime_shards(1) else {
            return;
        };
        let path = unique_temp_path("uring-native-any-local-fastpath");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"xyz123").expect("seed");
        drop(file);

        let file = OpenOptions::new().read(true).open(&path).expect("open");
        let fd = file.as_raw_fd();
        let handle_for_task = rt.handle();
        let join = rt
            .spawn_on(0, async move {
                let any = handle_for_task
                    .uring_native_unbound()
                    .expect("native any")
                    .with_preferred_shard(0)
                    .expect("preferred shard");
                any.read_at(fd, 0, 3).await.expect("read")
            })
            .expect("spawn");
        let got = join.await.expect("join");
        assert_eq!(&got, b"xyz");

        let stats = rt.handle().stats_snapshot();
        assert!(
            stats.native_any_local_fastpath_submitted > 0,
            "expected local fast-path submissions to be recorded"
        );
        let _ = std::fs::remove_file(path);
    }
}
