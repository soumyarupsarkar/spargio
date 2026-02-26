#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_compat_tests {
    use spargio::tokio_compat::{PollInterest, PollReactor, PollReactorError};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn poll_reactor_register_readable_gets_event() {
        let mut reactor = match PollReactor::new(64) {
            Ok(r) => r,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected reactor init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let token = reactor
            .register(reader.as_raw_fd(), PollInterest::Readable)
            .expect("register");
        writer.write_all(b"x").expect("write");

        let event = reactor.wait_one().expect("wait one");
        assert_eq!(event.token(), token);
        assert!(event.is_readable());
    }

    #[test]
    fn poll_reactor_deregister_twice_reports_not_found() {
        let mut reactor = match PollReactor::new(64) {
            Ok(r) => r,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected reactor init error: {err:?}"),
        };

        let (_writer, reader) = UnixStream::pair().expect("pair");
        let token = reactor
            .register(reader.as_raw_fd(), PollInterest::Readable)
            .expect("register");

        reactor.deregister(token).expect("first deregister");
        let err = reactor
            .deregister(token)
            .expect_err("second deregister should fail");
        assert_eq!(err, PollReactorError::NotFound);
    }

    #[test]
    fn poll_reactor_register_returns_unique_tokens() {
        let mut reactor = match PollReactor::new(64) {
            Ok(r) => r,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected reactor init error: {err:?}"),
        };

        let (_w1, r1) = UnixStream::pair().expect("pair1");
        let (_w2, r2) = UnixStream::pair().expect("pair2");

        let a = reactor
            .register(r1.as_raw_fd(), PollInterest::Readable)
            .expect("register a");
        let b = reactor
            .register(r2.as_raw_fd(), PollInterest::Writable)
            .expect("register b");

        assert_ne!(a, b);
    }
}
