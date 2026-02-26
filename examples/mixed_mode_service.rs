use futures::executor::block_on;
use spargio::boundary;
use spargio::{Runtime, TaskPlacement};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
struct Request {
    id: u64,
    fanout: usize,
}

#[derive(Clone, Copy, Debug)]
struct Response {
    id: u64,
    checksum: u64,
}

fn synthetic_work(seed: u64, iterations: u32) -> u64 {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    for i in 0..iterations {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407 ^ u64::from(i));
        x ^= x >> 29;
    }
    x
}

fn main() {
    let runtime = Runtime::builder()
        .shards(4)
        .build()
        .expect("spargio runtime");
    let handle = runtime.handle();
    let shard_count = handle.shard_count();

    let (client, server) = boundary::channel::<Request, Response>(256);

    let service_handle = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            loop {
                let req = match server.recv_timeout(Duration::from_millis(200)) {
                    Ok(req) => req,
                    Err(boundary::BoundaryError::Timeout) => continue,
                    Err(boundary::BoundaryError::Closed) => break,
                    Err(err) => {
                        eprintln!("boundary receive error: {err:?}");
                        break;
                    }
                };

                let request = *req.request();
                let preferred = (request.id as usize % shard_count.max(1)) as u16;
                let fanout = request.fanout;
                let branch_handle = handle.clone();
                let join = handle
                    .spawn_with_placement(
                        TaskPlacement::StealablePreferred(preferred),
                        async move {
                            let mut branches = Vec::with_capacity(fanout);
                            for branch in 0..fanout {
                                let seed = (request.id << 32) ^ branch as u64;
                                branches.push(
                                    branch_handle
                                        .spawn_stealable(async move { synthetic_work(seed, 8_000) })
                                        .expect("spawn branch"),
                                );
                            }

                            let mut checksum = 0u64;
                            for branch in branches {
                                checksum =
                                    checksum.wrapping_add(branch.await.expect("join branch"));
                            }
                            checksum
                        },
                    )
                    .expect("spawn request");

                let checksum = match block_on(join) {
                    Ok(v) => v,
                    Err(_) => {
                        let _ = req.respond(Response {
                            id: request.id,
                            checksum: 0,
                        });
                        continue;
                    }
                };

                let _ = req.respond(Response {
                    id: request.id,
                    checksum,
                });
            }
        })
    };

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let client_for_tokio = client.clone();
    tokio_rt.block_on(async move {
        let mut tasks = Vec::new();
        for id in 0..16u64 {
            let client = client_for_tokio.clone();
            tasks.push(tokio::spawn(async move {
                let request = Request { id, fanout: 8 };
                let ticket = client
                    .call_with_timeout(request, Duration::from_secs(2))
                    .expect("enqueue request");
                ticket.await
            }));
        }

        for task in tasks {
            match task.await {
                Ok(Ok(Response { id, checksum })) => {
                    println!("request {id} checksum {checksum}");
                }
                Ok(Err(err)) => {
                    eprintln!("request failed: {err:?}");
                }
                Err(join_err) => {
                    eprintln!("tokio task join error: {join_err}");
                }
            }
        }
    });

    drop(client);
    let _ = service_handle.join();
}
