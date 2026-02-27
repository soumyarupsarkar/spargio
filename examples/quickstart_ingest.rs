#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use spargio::{fs::File, net::TcpListener, RuntimeHandle};
#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use std::sync::Arc;
#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
#[spargio::main(backend = "io_uring", shards = 4)]
async fn main(handle: RuntimeHandle) -> std::io::Result<()> {
    std::fs::create_dir_all("ingest-out")?;
    let listener = TcpListener::bind(handle.clone(), "127.0.0.1:7001").await?;
    let next_id = Arc::new(AtomicU64::new(0));
    println!("ingest server on {}", listener.local_addr()?);

    loop {
        let (stream, _) = listener.accept_round_robin().await?;
        let task_stream = stream.clone();
        let task_handle = handle.clone();
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let path = format!("ingest-out/conn-{id}.bin");

        stream
            .spawn_stealable_on_session(&handle, async move {
                let file = File::create(task_handle, path).await?;
                let mut offset = 0u64;
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let (n, returned) = task_stream.recv_owned(buf).await?;
                    buf = returned;
                    if n == 0 {
                        break;
                    }
                    file.write_all_at(offset, &buf[..n]).await?;
                    offset = offset.saturating_add(n as u64);
                }
                file.fsync().await
            })
            .expect("spawn connection task");
    }
}

#[cfg(not(all(feature = "macros", feature = "uring-native", target_os = "linux")))]
fn main() {
    eprintln!(
        "this example requires Linux + --features macros,uring-native\n\
         run: cargo run --features macros,uring-native --example quickstart_ingest"
    );
}
