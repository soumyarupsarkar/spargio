#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
use spargio::{RuntimeHandle, fs::File, net::TcpListener};

#[cfg(all(feature = "macros", feature = "uring-native", target_os = "linux"))]
#[spargio::main(backend = "io_uring", shards = 2)]
async fn main(handle: RuntimeHandle) -> std::io::Result<()> {
    let file = File::open(handle.clone(), "README.md").await?;
    let bytes = file.read_to_end_at(0).await?;
    let listener = TcpListener::bind(handle, "127.0.0.1:7000").await?;
    println!(
        "serving {} bytes on {}",
        bytes.len(),
        listener.local_addr()?
    );
    loop {
        let (stream, _) = listener.accept_round_robin().await?;
        stream.write_all(&bytes).await?;
    }
}

#[cfg(not(all(feature = "macros", feature = "uring-native", target_os = "linux")))]
fn main() {
    eprintln!(
        "this example requires Linux + --features macros,uring-native\n\
         run: cargo run --features macros,uring-native --example quickstart_file_server"
    );
}
