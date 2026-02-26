#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_compat_stream_hardening_tests {
    use msg_ring_runtime::Runtime;
    use msg_ring_runtime::tokio_compat::PollReactorError;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use tokio::io::AsyncReadExt;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_fd_into_stream_reads_bytes() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let mut stream = lane
            .compat_fd(reader.as_raw_fd())
            .into_stream()
            .expect("into stream");
        writer.write_all(b"xyz").expect("write");

        let mut buf = [0u8; 3];
        stream.read_exact(&mut buf).await.expect("read exact");
        assert_eq!(&buf, b"xyz");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compat_stream_reads_eof_as_zero() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (writer, reader) = UnixStream::pair().expect("pair");
        drop(writer);
        let mut stream = lane
            .compat_stream_fd(reader.as_raw_fd())
            .expect("stream wrapper");

        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await.expect("read");
        assert_eq!(n, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lane_compat_stream_helper_wraps_asrawfd() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let mut stream = lane.compat_stream(&reader).expect("compat stream");
        writer.write_all(b"m").expect("write");

        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await.expect("read exact");
        assert_eq!(&buf, b"m");
    }
}
