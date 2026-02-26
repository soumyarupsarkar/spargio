#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_runtime_wait_tests {
    use msg_ring_runtime::tokio_compat::PollReactorError;
    use msg_ring_runtime::Runtime;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lane_wait_readable_and_wait_writable_work() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");

        lane.wait_writable(writer.as_raw_fd())
            .await
            .expect("wait writable");

        let waiter = {
            let lane = lane.clone();
            tokio::spawn(async move { lane.wait_readable(reader.as_raw_fd()).await })
        };
        writer.write_all(b"x").expect("write");
        waiter.await.expect("join").expect("wait readable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lane_wait_readable_cancellation_cleans_up_registration() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (_writer, reader) = UnixStream::pair().expect("pair");

        let task = {
            let lane = lane.clone();
            tokio::spawn(async move { lane.wait_readable(reader.as_raw_fd()).await })
        };

        let mut saw_registration = false;
        for _ in 0..100 {
            if lane.debug_poll_registered_count() > 0 {
                saw_registration = true;
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(saw_registration, "wait_readable never registered a poll token");

        task.abort();
        let _ = task.await;

        for _ in 0..200 {
            if lane.debug_poll_registered_count() == 0 {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("poll registration leak after cancellation");
    }
}
