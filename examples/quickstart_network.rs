#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use spargio::net::TcpStream;
#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use spargio::ShardCtx;

#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
#[spargio::main(backend = "io_uring", shards = 2)]
async fn main(handle: spargio::RuntimeHandle) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4];
        socket.read_exact(&mut buf).expect("read");
        socket.write_all(&buf).expect("write");
    });

    let stream = TcpStream::connect_round_robin(handle.clone(), addr).await?;
    let session_shard = stream.session_shard();
    let task_stream = stream.clone();
    let join = stream
        .spawn_stealable_on_session(&handle, async move {
            task_stream.write_all(b"ping").await?;
            let mut buf = [0u8; 4];
            task_stream.read_exact(&mut buf).await?;
            Ok::<_, std::io::Error>((ShardCtx::current().expect("ctx").shard_id(), buf))
        })
        .expect("spawn");
    let (exec_shard, buf) = join.await.expect("join")?;
    println!(
        "echo={}, session_shard={session_shard}, exec_shard={exec_shard}",
        std::str::from_utf8(&buf).expect("utf8")
    );
    Ok(())
}

#[cfg(not(all(feature = "macros", feature = "uring-native", target_os = "linux")))]
fn main() {
    eprintln!(
        "this example requires Linux + --features macros,uring-native\n\
         run: cargo run --features macros,uring-native --example quickstart_network"
    );
}
