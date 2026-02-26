#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_compat_stream_tests {
    use msg_ring_runtime::Runtime;
    use msg_ring_runtime::tokio_compat::PollReactorError;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_stream_fd_reads_and_writes() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (left, right) = UnixStream::pair().expect("pair");
        let mut writer = lane
            .compat_stream_fd(left.as_raw_fd())
            .expect("writer wrapper");
        let mut reader = lane
            .compat_stream_fd(right.as_raw_fd())
            .expect("reader wrapper");

        writer.write_all(b"hello").await.expect("write");
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.expect("read exact");
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_stream_fd_pending_read_wakes_on_write() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (left, right) = UnixStream::pair().expect("pair");
        let mut writer = lane
            .compat_stream_fd(left.as_raw_fd())
            .expect("writer wrapper");
        let mut reader = lane
            .compat_stream_fd(right.as_raw_fd())
            .expect("reader wrapper");

        let read_task = tokio::spawn(async move {
            let mut buf = [0u8; 3];
            reader.read_exact(&mut buf).await.expect("read exact");
            buf
        });

        tokio::time::sleep(Duration::from_millis(5)).await;
        writer.write_all(b"abc").await.expect("write");

        let got = read_task.await.expect("read task join");
        assert_eq!(&got, b"abc");
    }
}
