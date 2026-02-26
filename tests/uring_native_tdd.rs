#[cfg(all(feature = "uring-native", target_os = "linux"))]
mod linux_uring_native_tests {
    use msg_ring_runtime::{BackendKind, Runtime, RuntimeError};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
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
}
