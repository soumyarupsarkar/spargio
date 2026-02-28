#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = spargio::run_with(spargio::Runtime::builder().shards(1), |handle| async move {
        let path = std::env::temp_dir().join("spargio-extension-statx-example.txt");
        std::fs::write(&path, b"example-data")?;
        let meta = spargio::extension::fs::statx_or_metadata(handle.clone(), &path).await?;
        let _ = std::fs::remove_file(path);
        Ok::<_, std::io::Error>(meta)
    })
    .await??;

    println!("size={} mode={:#o}", out.size, out.mode);
    Ok(())
}

#[cfg(not(all(feature = "uring-native", target_os = "linux")))]
fn main() {
    eprintln!("native_extension_statx example requires --features uring-native on Linux");
}
