use std::fs;
use std::path::Path;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::net::TcpListener;

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    path.push(format!("{prefix}-{}-{ts}", std::process::id()));
    path
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn try_build_runtime() -> Option<spargio::Runtime> {
    match spargio::Runtime::builder().shards(1).build() {
        Ok(rt) => Some(rt),
        Err(spargio::RuntimeError::IoUringInit(_))
        | Err(spargio::RuntimeError::UnsupportedBackend(_)) => None,
        Err(err) => panic!("unexpected runtime init error: {err:?}"),
    }
}

#[test]
fn readme_documents_dns_and_deferred_fs_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).expect("read README.md");
    assert!(
        readme.contains("ToSocketAddrs") && readme.contains("SocketAddr"),
        "expected README to document DNS/SocketAddr contract"
    );
    for helper in [
        "create_dir_all",
        "canonicalize",
        "metadata",
        "symlink_metadata",
        "set_permissions",
        "metadata_lite",
    ] {
        assert!(
            readme.contains(helper),
            "expected README deferred-fs section to mention {helper}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn dns_hostname_connect_and_socket_addr_connect_both_work() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let listener = TcpListener::bind("localhost:0")
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4];
            socket.read_exact(&mut buf).expect("read");
            socket.write_all(&buf).expect("write");
        }
    });

    let hostname_stream =
        spargio::net::TcpStream::connect(rt.handle(), format!("localhost:{}", addr.port()))
            .await
            .expect("hostname connect");
    hostname_stream
        .write_all(b"ping")
        .await
        .expect("write hostname");
    let mut out = [0u8; 4];
    hostname_stream
        .read_exact(&mut out)
        .await
        .expect("read hostname");
    assert_eq!(&out, b"ping");

    let socket_addr_stream = spargio::net::TcpStream::connect_socket_addr(rt.handle(), addr)
        .await
        .expect("socket addr connect");
    socket_addr_stream
        .write_all(b"pong")
        .await
        .expect("write socket");
    let mut out2 = [0u8; 4];
    socket_addr_stream
        .read_exact(&mut out2)
        .await
        .expect("read socket");
    assert_eq!(&out2, b"pong");

    server.join().expect("server join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn deferred_fs_helpers_execute_and_metadata_lite_is_available() {
    let Some(rt) = try_build_runtime() else {
        return;
    };
    let handle = rt.handle();
    let root = unique_temp_dir("spargio-deferred-fs");
    let nested = root.join("a/b/c");
    let file = root.join("a/b/c/file.txt");

    spargio::fs::reset_create_dir_all_blocking_fallback_count_for_test();
    spargio::fs::create_dir_all(&handle, &nested)
        .await
        .expect("create_dir_all");
    assert_eq!(
        spargio::fs::create_dir_all_blocking_fallback_count_for_test(),
        0,
        "simple create_dir_all paths should avoid direct blocking fallback"
    );
    let canonical = spargio::fs::canonicalize(&handle, &nested)
        .await
        .expect("canonicalize");
    assert!(canonical.is_absolute());

    fs::write(&file, b"abc123").expect("seed file");
    let meta = spargio::fs::metadata(&handle, &file)
        .await
        .expect("metadata");
    assert_eq!(meta.len(), 6);
    let symlink_meta = spargio::fs::symlink_metadata(&handle, &file)
        .await
        .expect("symlink_metadata");
    assert_eq!(symlink_meta.len(), 6);

    let lite = spargio::fs::metadata_lite(&handle, &file)
        .await
        .expect("metadata_lite");
    assert_eq!(lite.size, 6);

    let mut permissions = meta.permissions();
    permissions.set_readonly(true);
    spargio::fs::set_permissions(&handle, &file, permissions)
        .await
        .expect("set_permissions");

    let _ = fs::remove_dir_all(&root);
}
