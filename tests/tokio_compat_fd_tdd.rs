#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_compat_fd_tests {
    use msg_ring_runtime::tokio_compat::PollReactorError;
    use msg_ring_runtime::Runtime;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_fd_readable_and_writable_work() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let writer_fd = lane.compat_fd(writer.as_raw_fd());
        let reader_fd = lane.compat_fd(reader.as_raw_fd());

        writer_fd.writable().await.expect("wait writable");
        assert_eq!(writer_fd.fd(), writer.as_raw_fd());
        assert_eq!(reader_fd.fd(), reader.as_raw_fd());

        let reader_wait = tokio::spawn(async move { reader_fd.readable().await });
        writer.write_all(b"x").expect("write");
        reader_wait.await.expect("join").expect("wait readable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_fd_is_cloneable() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (_writer, reader) = UnixStream::pair().expect("pair");
        let fd = lane.compat_fd(reader.as_raw_fd());
        let cloned = fd.clone();
        assert_eq!(fd.fd(), cloned.fd());
    }
}
