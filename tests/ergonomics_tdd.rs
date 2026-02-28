#[cfg(all(feature = "uring-native", target_os = "linux"))]
mod ergonomics_tests {
    use spargio::{BackendKind, Runtime, RuntimeError};
    use std::io::{Read, Write};
    use std::net::TcpListener;
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
            .shards(2)
            .backend(BackendKind::IoUring)
            .build()
        {
            Ok(rt) => Some(rt),
            Err(RuntimeError::IoUringInit(_)) => None,
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        }
    }

    fn is_nonblocking(fd: i32) -> bool {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return false;
        }
        flags & libc::O_NONBLOCK != 0
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_open_read_to_end_and_write_at() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("spargio-fs-ergonomics");
        std::fs::write(&path, b"abcdef").expect("seed file");

        let file = spargio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(rt.handle(), &path)
            .await
            .expect("open");

        let initial = file.read_to_end_at(0).await.expect("read_to_end_at");
        assert_eq!(&initial, b"abcdef");

        let wrote = file.write_at(2, b"ZZ").await.expect("write_at");
        assert_eq!(wrote, 2);
        file.fsync().await.expect("fsync");

        let out = file.read_to_end_at(0).await.expect("read modified");
        assert_eq!(&out, b"abZZef");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_open_options_create_new_reports_already_exists() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("spargio-fs-create-new");
        std::fs::write(&path, b"seed").expect("seed file");

        let err = spargio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(rt.handle(), &path)
            .await
            .err()
            .expect("create_new on existing path should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_open_options_append_and_truncate_is_invalid() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let path = unique_temp_path("spargio-fs-append-truncate");
        std::fs::write(&path, b"seed").expect("seed file");

        let err = spargio::fs::OpenOptions::new()
            .append(true)
            .truncate(true)
            .open(rt.handle(), &path)
            .await
            .err()
            .expect("append+truncate should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_connect_supports_read_write_all() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).expect("read_exact");
            assert_eq!(&buf, b"ping");
            socket.write_all(b"pong").expect("write_all");
        });

        let stream = spargio::net::TcpStream::connect(rt.handle(), addr)
            .await
            .expect("connect");
        assert!(
            is_nonblocking(stream.as_raw_fd()),
            "connected spargio tcp stream should be nonblocking"
        );
        stream.write_all(b"ping").await.expect("write_all");
        let mut recv = [0u8; 4];
        stream.read_exact(&mut recv).await.expect("read_exact");
        assert_eq!(&recv, b"pong");

        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_connect_socket_addr_supports_read_write_all() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).expect("read_exact");
            assert_eq!(&buf, b"ping");
            socket.write_all(b"pong").expect("write_all");
        });

        let stream = spargio::net::TcpStream::connect_socket_addr(rt.handle(), addr)
            .await
            .expect("connect");
        stream.write_all(b"ping").await.expect("write_all");
        let mut recv = [0u8; 4];
        stream.read_exact(&mut recv).await.expect("read_exact");
        assert_eq!(&recv, b"pong");

        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_owned_buffers_support_read_write_all() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).expect("read_exact");
            assert_eq!(&buf, b"ping");
            socket.write_all(b"pong").expect("write_all");
        });

        let stream = spargio::net::TcpStream::connect(rt.handle(), addr)
            .await
            .expect("connect");

        let mut payload = vec![0u8; 4];
        payload.copy_from_slice(b"ping");
        let payload = stream
            .write_all_owned(payload)
            .await
            .expect("write_all_owned");
        assert_eq!(&payload, b"ping");

        let recv = stream
            .read_exact_owned(vec![0u8; 4])
            .await
            .expect("read_exact_owned");
        assert_eq!(&recv, b"pong");

        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_listener_bind_accepts_and_wraps_stream() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = spargio::net::TcpListener::bind(rt.handle(), "127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).expect("client connect");
            stream.write_all(b"abc").expect("client write");
            let mut out = [0u8; 3];
            stream.read_exact(&mut out).expect("client read");
            out
        });

        let (stream, _addr) = listener.accept().await.expect("accept");
        assert!(
            is_nonblocking(stream.as_raw_fd()),
            "accepted spargio tcp stream should be nonblocking"
        );
        let mut recv = [0u8; 3];
        stream.read_exact(&mut recv).await.expect("server read");
        assert_eq!(&recv, b"abc");
        stream.write_all(b"xyz").await.expect("server write");

        let echoed = client.join().expect("join client");
        assert_eq!(&echoed, b"xyz");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_listener_bind_socket_addr_accepts_and_wraps_stream() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = spargio::net::TcpListener::bind_socket_addr(
            rt.handle(),
            "127.0.0.1:0".parse().expect("socket addr"),
        )
        .await
        .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).expect("client connect");
            stream.write_all(b"abc").expect("client write");
            let mut out = [0u8; 3];
            stream.read_exact(&mut out).expect("client read");
            out
        });

        let (stream, _addr) = listener.accept().await.expect("accept");
        let mut recv = [0u8; 3];
        stream.read_exact(&mut recv).await.expect("server read");
        assert_eq!(&recv, b"abc");
        stream.write_all(b"xyz").await.expect("server write");

        let echoed = client.join().expect("join client");
        assert_eq!(&echoed, b"xyz");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_session_path_does_not_track_unbound_op_routes() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let mut client = std::net::TcpStream::connect(addr).expect("client connect");
        let (server, _) = listener.accept().expect("accept");
        let stream =
            spargio::net::TcpStream::from_std(rt.handle(), server).expect("wrap std tcp stream");

        let any = rt
            .handle()
            .uring_native_unbound()
            .expect("native unbound handle");

        let read_task = tokio::spawn(async move {
            stream
                .read_exact_owned(vec![0u8; 4])
                .await
                .expect("read_exact_owned")
        });

        for _ in 0..30 {
            assert_eq!(
                any.active_native_op_count(),
                0,
                "session-path tcp ops should bypass tracked unbound op routes"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        client.write_all(b"done").expect("client write");
        let recv = read_task.await.expect("join read task");
        assert_eq!(&recv, b"done");
        assert_eq!(any.active_native_op_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_connect_round_robin_distributes_session_shards() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let accept_count = 8usize;
        let server = std::thread::spawn(move || {
            let mut accepted = Vec::with_capacity(accept_count);
            for _ in 0..accept_count {
                let (socket, _) = listener.accept().expect("accept");
                accepted.push(socket);
            }
            accepted
        });

        let streams =
            spargio::net::TcpStream::connect_many_round_robin(rt.handle(), addr, accept_count)
                .await
                .expect("connect_many_round_robin");

        let mut seen = std::collections::BTreeSet::new();
        for stream in &streams {
            seen.insert(stream.session_shard());
        }
        assert!(
            seen.len() >= 2,
            "expected round-robin stream sessions to spread across shards, got {:?}",
            seen
        );

        drop(streams);
        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_spawn_on_session_runs_on_stream_session_shard() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let client =
            std::thread::spawn(move || std::net::TcpStream::connect(addr).expect("connect client"));
        let (server, _) = listener.accept().expect("accept");
        let _ = client.join();

        let stream =
            spargio::net::TcpStream::from_std(rt.handle(), server).expect("wrap std tcp stream");
        let expected = stream.session_shard();
        let handle = rt.handle();
        let join = stream
            .spawn_on_session(&handle, async move {
                spargio::ShardCtx::current()
                    .expect("current shard")
                    .shard_id()
            })
            .expect("spawn_on_session");
        let observed = join.await.expect("join");
        assert_eq!(observed, expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_tcp_stream_spawn_on_session_uses_local_direct_native_fastpath() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).expect("client connect");
            stream.write_all(b"ping").expect("client write");
            let mut out = [0u8; 4];
            stream.read_exact(&mut out).expect("client read");
            out
        });
        let (server, _) = listener.accept().expect("accept");

        let stream =
            spargio::net::TcpStream::from_std(rt.handle(), server).expect("wrap std tcp stream");
        let handle = rt.handle();
        let before = handle.stats_snapshot();

        let task_stream = stream.clone();
        let join = stream
            .spawn_on_session(&handle, async move {
                let recv = task_stream
                    .read_exact_owned(vec![0u8; 4])
                    .await
                    .expect("read_exact_owned");
                assert_eq!(&recv, b"ping");

                let sent = task_stream
                    .write_all_owned(b"pong".to_vec())
                    .await
                    .expect("write_all_owned");
                assert_eq!(&sent, b"pong");
            })
            .expect("spawn_on_session");

        join.await.expect("join");
        let echoed = client.join().expect("join client");
        assert_eq!(&echoed, b"pong");

        let after = handle.stats_snapshot();
        assert!(
            after.native_any_local_direct_submitted > before.native_any_local_direct_submitted,
            "expected local direct native fastpath submissions to increase"
        );
        assert_eq!(
            after.native_any_envelope_submitted, before.native_any_envelope_submitted,
            "session-pinned local stream ops should not use cross-shard native envelopes"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_path_helpers_cover_common_workflows() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let handle = rt.handle();

        let mut base = std::env::temp_dir();
        base.push(format!(
            "spargio-fs-helpers-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let nested = base.join("a").join("b");
        let file = nested.join("payload.txt");
        let renamed = nested.join("renamed.txt");

        spargio::fs::create_dir(&handle, &base)
            .await
            .expect("create_dir");
        spargio::fs::create_dir_all(&handle, &nested)
            .await
            .expect("create_dir_all");
        spargio::fs::write(&handle, &file, b"hello world".to_vec())
            .await
            .expect("write");
        let bytes = spargio::fs::read(&handle, &file).await.expect("read");
        assert_eq!(&bytes, b"hello world");
        let text = spargio::fs::read_to_string(&handle, &file)
            .await
            .expect("read_to_string");
        assert_eq!(text, "hello world");
        let meta = spargio::fs::metadata(&handle, &file)
            .await
            .expect("metadata");
        assert_eq!(meta.len(), 11);
        let canonical = spargio::fs::canonicalize(&handle, &file)
            .await
            .expect("canonicalize");
        assert!(canonical.ends_with("payload.txt"));

        spargio::fs::rename(&handle, &file, &renamed)
            .await
            .expect("rename");
        spargio::fs::remove_file(&handle, &renamed)
            .await
            .expect("remove_file");
        spargio::fs::remove_dir(&handle, &nested)
            .await
            .expect("remove nested");
        spargio::fs::remove_dir(&handle, base.join("a"))
            .await
            .expect("remove parent");
        spargio::fs::remove_dir(&handle, &base)
            .await
            .expect("remove base");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_link_helpers_support_symlink_and_hard_link() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let handle = rt.handle();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "spargio-fs-links-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        spargio::fs::create_dir_all(&handle, &dir)
            .await
            .expect("mkdir");
        let src = dir.join("src.txt");
        let hard = dir.join("hard.txt");
        let sym = dir.join("sym.txt");
        spargio::fs::write(&handle, &src, b"abc".to_vec())
            .await
            .expect("seed");
        spargio::fs::hard_link(&handle, &src, &hard)
            .await
            .expect("hard_link");
        spargio::fs::symlink(&handle, &src, &sym)
            .await
            .expect("symlink");
        let hard_data = spargio::fs::read(&handle, &hard).await.expect("read hard");
        assert_eq!(&hard_data, b"abc");
        let sym_meta = spargio::fs::symlink_metadata(&handle, &sym)
            .await
            .expect("symlink_metadata");
        assert!(sym_meta.file_type().is_symlink());

        let _ = spargio::fs::remove_file(&handle, &sym).await;
        let _ = spargio::fs::remove_file(&handle, &hard).await;
        let _ = spargio::fs::remove_file(&handle, &src).await;
        let _ = spargio::fs::remove_dir(&handle, &dir).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_udp_socket_supports_send_recv_and_send_to_recv_from() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let handle = rt.handle();
        let a = spargio::net::UdpSocket::bind(handle.clone(), "127.0.0.1:0")
            .await
            .expect("bind a");
        let b = spargio::net::UdpSocket::bind(handle.clone(), "127.0.0.1:0")
            .await
            .expect("bind b");
        let a_addr = a.local_addr().expect("a addr");
        let b_addr = b.local_addr().expect("b addr");

        a.send_to(b"ping", b_addr).await.expect("send_to");
        let (data, from) = b.recv_from(64).await.expect("recv_from");
        assert_eq!(&data, b"ping");
        assert_eq!(from, a_addr);

        a.connect(b_addr).await.expect("connect a");
        b.connect(a_addr).await.expect("connect b");
        a.send(b"pong").await.expect("send");
        let recv = b.recv(64).await.expect("recv");
        assert_eq!(&recv, b"pong");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn net_unix_stream_listener_and_datagram_cover_core_paths() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };
        let handle = rt.handle();

        let mut base = std::env::temp_dir();
        base.push(format!(
            "spargio-unix-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("base dir");
        let stream_path = base.join("stream.sock");
        let dgram_a_path = base.join("a.sock");
        let dgram_b_path = base.join("b.sock");

        let listener = spargio::net::UnixListener::bind(handle.clone(), &stream_path)
            .await
            .expect("bind unix listener");
        let client = tokio::spawn({
            let handle = handle.clone();
            let stream_path = stream_path.clone();
            async move {
                let c = spargio::net::UnixStream::connect(handle, &stream_path)
                    .await
                    .expect("connect unix stream");
                c.send(b"hello").await.expect("client send");
                c.recv(64).await.expect("client recv")
            }
        });

        let (server, _addr) = listener.accept().await.expect("accept unix stream");
        let got = server.recv(64).await.expect("server recv");
        assert_eq!(&got, b"hello");
        server.send(b"world").await.expect("server send");
        let echoed = client.await.expect("join client");
        assert_eq!(&echoed, b"world");

        let da = spargio::net::UnixDatagram::bind(handle.clone(), &dgram_a_path)
            .await
            .expect("bind dgram a");
        let db = spargio::net::UnixDatagram::bind(handle.clone(), &dgram_b_path)
            .await
            .expect("bind dgram b");
        da.send_to(b"one", &dgram_b_path).await.expect("send_to");
        let (bytes, _from) = db.recv_from(64).await.expect("recv_from");
        assert_eq!(&bytes, b"one");

        da.connect(&dgram_b_path).await.expect("connect a");
        db.connect(&dgram_a_path).await.expect("connect b");
        da.send(b"two").await.expect("send");
        let got = db.recv(64).await.expect("recv");
        assert_eq!(&got, b"two");

        let _ = std::fs::remove_file(&stream_path);
        let _ = std::fs::remove_file(&dgram_a_path);
        let _ = std::fs::remove_file(&dgram_b_path);
        let _ = std::fs::remove_dir(&base);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn io_helpers_split_copy_and_framed_work() {
        let Some(rt) = try_build_io_uring_runtime() else {
            return;
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut len_buf = [0u8; 4];
            socket.read_exact(&mut len_buf).expect("len");
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            socket.read_exact(&mut payload).expect("payload");
            assert_eq!(&payload, b"frame");
            socket.write_all(&5u32.to_be_bytes()).expect("resp len");
            socket.write_all(b"reply").expect("resp payload");
        });

        let stream = spargio::net::TcpStream::connect(rt.handle(), addr)
            .await
            .expect("connect");
        let (reader, writer) = spargio::io::split(stream.clone());
        let mut framed = spargio::io::framed::LengthDelimited::new(reader, writer);
        framed
            .write_frame(b"frame".to_vec())
            .await
            .expect("write frame");
        let reply = framed.read_frame().await.expect("read frame");
        assert_eq!(&reply, b"reply");

        let listener2 = TcpListener::bind("127.0.0.1:0").expect("bind2");
        let addr2 = listener2.local_addr().expect("addr2");
        let server2 = std::thread::spawn(move || {
            let (mut socket, _) = listener2.accept().expect("accept2");
            socket.write_all(b"copy-payload").expect("server write");
        });
        let stream2 = spargio::net::TcpStream::connect(rt.handle(), addr2)
            .await
            .expect("connect2");
        let (reader2, _) = spargio::io::split(stream2);
        let mut sink = Vec::new();
        let copied = spargio::io::copy_to_vec(&reader2, &mut sink, 32)
            .await
            .expect("copy_to_vec");
        assert_eq!(copied, sink.len());
        assert_eq!(&sink, b"copy-payload");

        let _ = server.join();
        let _ = server2.join();
    }
}
