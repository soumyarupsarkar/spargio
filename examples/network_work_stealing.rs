#[cfg(all(feature = "uring-native", target_os = "linux"))]
mod linux_example {
    use spargio::net::TcpStream;
    use spargio::{BackendKind, Runtime, ShardCtx};
    use std::collections::BTreeSet;
    use std::io::{self, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    const SHARDS: usize = 4;
    const STREAMS: usize = 8;
    const ROUNDS: u32 = 48;
    const FRAME_LEN: usize = 16;

    fn make_frame(stream_id: u32, round: u32) -> [u8; FRAME_LEN] {
        let mut frame = [0u8; FRAME_LEN];
        frame[..4].copy_from_slice(&stream_id.to_le_bytes());
        frame[4..8].copy_from_slice(&round.to_le_bytes());
        frame[8..16]
            .copy_from_slice(&(u64::from(stream_id) << 32 | u64::from(round)).to_le_bytes());
        frame
    }

    fn synthetic_cpu(seed: u64) -> u64 {
        let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
        for i in 0..512u64 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407 ^ i);
            x ^= x >> 33;
        }
        x
    }

    fn spawn_echo_server(listener: TcpListener, expected_clients: usize) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut workers = Vec::with_capacity(expected_clients);
            for _ in 0..expected_clients {
                let (mut socket, _) = listener.accept().expect("accept");
                workers.push(thread::spawn(move || {
                    let mut frame = [0u8; FRAME_LEN];
                    loop {
                        match socket.read_exact(&mut frame) {
                            Ok(()) => socket.write_all(&frame).expect("echo write"),
                            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                            Err(err) if err.kind() == io::ErrorKind::ConnectionReset => break,
                            Err(err) => panic!("echo read failed: {err}"),
                        }
                    }
                }));
            }

            for worker in workers {
                let _ = worker.join();
            }
        })
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let server = spawn_echo_server(listener, STREAMS);

        let (session_shards, execution_shards, digest) = spargio::run_with(
            Runtime::builder()
                .backend(BackendKind::IoUring)
                .shards(SHARDS),
            move |handle| async move {
                let streams = TcpStream::connect_many_round_robin(handle.clone(), addr, STREAMS)
                    .await
                    .expect("connect many");

                let mut join_handles = Vec::with_capacity(streams.len());
                let mut session_shards = BTreeSet::new();

                for (stream_id, stream) in streams.into_iter().enumerate() {
                    let session = stream.session_shard();
                    session_shards.insert(session);
                    let task_stream = stream.clone();
                    let join = stream
                        .spawn_stealable_on_session(&handle, async move {
                            let mut seen_execution_shards = BTreeSet::new();
                            let mut recv = [0u8; FRAME_LEN];
                            let mut checksum = 0u64;

                            for round in 0..ROUNDS {
                                let frame = make_frame(stream_id as u32, round);
                                task_stream.write_all(&frame).await.expect("client write");
                                task_stream.read_exact(&mut recv).await.expect("client read");
                                assert_eq!(recv, frame, "echo mismatch");

                                let shard = ShardCtx::current().expect("current shard").shard_id();
                                seen_execution_shards.insert(shard);
                                checksum = checksum.wrapping_add(synthetic_cpu(
                                    (u64::from(stream_id as u32) << 48)
                                        ^ (u64::from(round) << 16)
                                        ^ u64::from(shard),
                                ));
                            }

                            (session, seen_execution_shards, checksum)
                        })
                        .expect("spawn stream task");
                    join_handles.push(join);
                }

                let mut combined_checksum = 0u64;
                let mut execution_shards = BTreeSet::new();
                for join in join_handles {
                    let (_session, seen, checksum) = join.await.expect("join stream task");
                    execution_shards.extend(seen);
                    combined_checksum = combined_checksum.wrapping_add(checksum);
                }

                (session_shards, execution_shards, combined_checksum)
            },
        )
        .map_err(|err| io::Error::other(format!("runtime startup failed: {err:?}")))?;

        println!("session shards from round-robin connect: {session_shards:?}");
        println!("observed execution shards (stealable tasks): {execution_shards:?}");
        println!("combined checksum: {digest:#x}");

        let _ = server.join();
        Ok(())
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux_example::run()
}

#[cfg(not(all(feature = "uring-native", target_os = "linux")))]
fn main() {
    eprintln!(
        "this example requires Linux + --features uring-native\n\
         run: cargo run --features uring-native --example network_work_stealing"
    );
}
