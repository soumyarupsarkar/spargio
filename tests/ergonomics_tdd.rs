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
}
