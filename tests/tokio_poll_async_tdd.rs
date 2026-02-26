#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_compat_async_tests {
    use spargio::tokio_compat::{PollInterest, PollReactorError, TokioPollReactor};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_poll_reactor_wait_one_gets_readable_event() {
        let reactor = match TokioPollReactor::new(64) {
            Ok(r) => r,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected reactor init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let token = reactor
            .register(reader.as_raw_fd(), PollInterest::Readable)
            .expect("register");
        writer.write_all(b"x").expect("write");

        let event = reactor.wait_one().await.expect("wait one");
        assert_eq!(event.token(), token);
        assert!(event.is_readable());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_poll_reactor_async_deregister_reports_not_found_on_second_remove() {
        let reactor = match TokioPollReactor::new(64) {
            Ok(r) => r,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected reactor init error: {err:?}"),
        };

        let (_writer, reader) = UnixStream::pair().expect("pair");
        let token = reactor
            .register(reader.as_raw_fd(), PollInterest::Readable)
            .expect("register");

        reactor.deregister(token).await.expect("first deregister");
        let err = reactor
            .deregister(token)
            .await
            .expect_err("second deregister should fail");
        assert_eq!(err, PollReactorError::NotFound);
    }
}
