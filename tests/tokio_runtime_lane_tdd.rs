#[cfg(all(feature = "tokio-compat", target_os = "linux"))]
mod linux_tokio_runtime_lane_tests {
    use msg_ring_runtime::tokio_compat::{PollInterest, PollReactorError};
    use msg_ring_runtime::{Event, Runtime, ShardCtx};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_compat_lane_poll_and_runtime_api_work_together() {
        let rt = Runtime::builder().shards(2).build().expect("runtime");
        let handle = rt.handle();
        let lane = match handle.tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let recv = lane
            .spawn_pinned(1, async {
                let next = {
                    let ctx = ShardCtx::current().expect("on shard");
                    ctx.next_event()
                };
                next.await
            })
            .expect("spawn receiver");

        lane.remote(1)
            .expect("remote")
            .send_raw(42, 5)
            .expect("send")
            .await
            .expect("ticket");

        let event = recv.await.expect("join");
        assert_eq!(
            event,
            Event::RingMsg {
                from: u16::MAX,
                tag: 42,
                val: 5
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tokio_compat_lane_poll_register_and_wait() {
        let rt = Runtime::builder().shards(1).build().expect("runtime");
        let lane = match rt.handle().tokio_compat_lane(64) {
            Ok(l) => l,
            Err(PollReactorError::Io(_)) => return,
            Err(err) => panic!("unexpected lane init error: {err:?}"),
        };

        let (mut writer, reader) = UnixStream::pair().expect("pair");
        let token = lane
            .register(reader.as_raw_fd(), PollInterest::Readable)
            .expect("register");
        writer.write_all(b"x").expect("write");

        let event = lane.wait_one().await.expect("wait one");
        assert_eq!(event.token(), token);
        assert!(event.is_readable());
    }
}
