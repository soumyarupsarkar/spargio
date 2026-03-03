use criterion::{criterion_group, criterion_main};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use criterion::{Criterion, black_box};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::executor::block_on;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::{BackendKind, Runtime, RuntimeError};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::fs;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::io::{Seek, SeekFrom, Write};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::os::unix::fs::symlink;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::path::PathBuf;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct DuFixture {
    root: PathBuf,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl DuFixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spargio_du_bench_{}_{}",
            std::process::id(),
            unique
        ));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create fixture root");
        fs::write(root.join("alpha.bin"), vec![0xAB; 256 * 1024]).expect("write alpha");

        let sparse = nested.join("sparse.bin");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&sparse)
            .expect("open sparse");
        file.seek(SeekFrom::Start(16 * 1024 * 1024))
            .expect("seek sparse");
        file.write_all(&[1u8]).expect("write sparse tail");

        fs::hard_link(&sparse, nested.join("sparse.hard")).expect("hardlink sparse");
        symlink("sparse.bin", nested.join("sparse.link")).expect("symlink sparse");

        Self { root }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for DuFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn try_runtime() -> Option<Runtime> {
    match Runtime::builder()
        .backend(BackendKind::IoUring)
        .shards(2)
        .build()
    {
        Ok(rt) => Some(rt),
        Err(RuntimeError::IoUringInit(_)) | Err(RuntimeError::UnsupportedBackend(_)) => None,
        Err(err) => panic!("unexpected runtime init error: {err:?}"),
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn bench_du_api(c: &mut Criterion) {
    let Some(rt) = try_runtime() else {
        return;
    };
    let fixture = DuFixture::new();
    let handle = rt.handle();

    c.bench_function("fs_du_allocated", |b| {
        b.iter(|| {
            let summary = block_on(spargio::fs::du(
                &handle,
                &fixture.root,
                spargio::fs::DuOptions::default(),
            ))
            .expect("du allocated");
            black_box(summary.total_bytes)
        })
    });

    c.bench_function("fs_du_apparent", |b| {
        b.iter(|| {
            let summary = block_on(spargio::fs::du(
                &handle,
                &fixture.root,
                spargio::fs::DuOptions::default().size_mode(spargio::fs::DuSizeMode::Apparent),
            ))
            .expect("du apparent");
            black_box(summary.total_bytes)
        })
    });

    c.bench_function("fs_read_dir_root", |b| {
        b.iter(|| {
            let entries =
                block_on(spargio::fs::read_dir(&handle, &fixture.root)).expect("read_dir");
            black_box(entries.len())
        })
    });
}

#[cfg(not(all(feature = "uring-native", target_os = "linux")))]
fn bench_du_api(_: &mut criterion::Criterion) {}

criterion_group!(du_api_benches, bench_du_api);
criterion_main!(du_api_benches);
