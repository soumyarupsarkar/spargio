#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::fs;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::os::unix::fs::MetadataExt;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::os::unix::fs::symlink;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    path.push(format!("{prefix}-{}-{ts}", std::process::id()));
    path
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn try_build_runtime() -> Option<spargio::Runtime> {
    match spargio::Runtime::builder().shards(2).build() {
        Ok(rt) => Some(rt),
        Err(spargio::RuntimeError::IoUringInit(_))
        | Err(spargio::RuntimeError::UnsupportedBackend(_)) => None,
        Err(err) => panic!("unexpected runtime init error: {err:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn metadata_lite_exposes_du_relevant_fields() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-meta");
    fs::create_dir_all(&root).expect("mkdir root");
    let file = root.join("file.bin");
    fs::write(&file, b"hello-world").expect("seed file");

    let lite = spargio::fs::metadata_lite(&rt.handle(), &file)
        .await
        .expect("metadata_lite");
    let std_meta = fs::metadata(&file).expect("std metadata");

    assert_eq!(lite.size, std_meta.len());
    assert_eq!(lite.nlink as u64, std_meta.nlink());
    assert_eq!(lite.ino, std_meta.ino());
    assert_eq!(lite.blocks, std_meta.blocks());

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn fs_read_dir_lists_entries_with_types() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-read-dir");
    fs::create_dir_all(&root).expect("mkdir root");
    fs::write(root.join("alpha.txt"), b"x").expect("seed alpha");
    fs::create_dir(root.join("beta")).expect("mkdir beta");
    symlink("alpha.txt", root.join("alpha.link")).expect("symlink");

    let entries = spargio::fs::read_dir(&rt.handle(), &root)
        .await
        .expect("read_dir");

    assert!(entries.iter().any(|e| e.file_name == "alpha.txt"));
    assert!(entries.iter().any(|e| e.file_name == "beta"));
    assert!(entries.iter().any(|e| e.file_name == "alpha.link"));

    let alpha = entries.iter().find(|e| e.file_name == "alpha.txt").unwrap();
    assert_eq!(alpha.entry_type, spargio::fs::DirEntryType::File);

    let beta = entries.iter().find(|e| e.file_name == "beta").unwrap();
    assert_eq!(beta.entry_type, spargio::fs::DirEntryType::Directory);

    let link = entries
        .iter()
        .find(|e| e.file_name == "alpha.link")
        .unwrap();
    assert_eq!(link.entry_type, spargio::fs::DirEntryType::Symlink);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn extension_read_dir_entries_exposes_low_level_dirent_surface() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-ext-read-dir");
    fs::create_dir_all(&root).expect("mkdir root");
    fs::write(root.join("alpha.txt"), b"x").expect("seed alpha");
    fs::create_dir(root.join("beta")).expect("mkdir beta");
    symlink("alpha.txt", root.join("alpha.link")).expect("symlink");

    let entries = spargio::extension::fs::read_dir_entries(rt.handle(), &root)
        .await
        .expect("read_dir_entries");

    assert!(entries.iter().any(|e| e.file_name == "alpha.txt"));
    assert!(entries.iter().any(|e| e.file_name == "beta"));
    assert!(entries.iter().any(|e| e.file_name == "alpha.link"));
    assert!(
        entries.iter().all(|e| !e.file_name.is_empty()),
        "entry names should be populated"
    );

    let alpha = entries.iter().find(|e| e.file_name == "alpha.txt").unwrap();
    assert_eq!(alpha.entry_type, spargio::extension::fs::DirEntryType::File);
    assert!(alpha.inode > 0);

    let beta = entries.iter().find(|e| e.file_name == "beta").unwrap();
    assert_eq!(
        beta.entry_type,
        spargio::extension::fs::DirEntryType::Directory
    );

    let link = entries
        .iter()
        .find(|e| e.file_name == "alpha.link")
        .unwrap();
    assert_eq!(
        link.entry_type,
        spargio::extension::fs::DirEntryType::Symlink
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn du_handles_sparse_files_hardlinks_and_looping_symlinks() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-engine");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("mkdir nested");

    // sparse file: apparent size should exceed allocated size in most filesystems
    let sparse = nested.join("sparse.bin");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&sparse)
            .expect("open sparse");
        f.seek(SeekFrom::Start(8 * 1024 * 1024)).expect("seek");
        f.write_all(&[1u8]).expect("write tail byte");
    }

    // hardlink dedupe test
    let hardlink = nested.join("sparse.hard");
    fs::hard_link(&sparse, &hardlink).expect("hard link");

    // symlink loop safety test
    let loop_link = nested.join("loop");
    symlink(".", &loop_link).expect("loop symlink");

    let allocated = spargio::fs::du(&rt.handle(), &root, spargio::fs::DuOptions::default())
        .await
        .expect("du allocated");

    let apparent = spargio::fs::du(
        &rt.handle(),
        &root,
        spargio::fs::DuOptions::default().size_mode(spargio::fs::DuSizeMode::Apparent),
    )
    .await
    .expect("du apparent");

    assert!(
        apparent.total_bytes >= allocated.total_bytes,
        "apparent bytes should be >= allocated bytes"
    );

    let no_dedupe = spargio::fs::du(
        &rt.handle(),
        &root,
        spargio::fs::DuOptions::default().hardlink_dedupe(false),
    )
    .await
    .expect("du without hardlink dedupe");

    assert!(no_dedupe.total_bytes >= allocated.total_bytes);

    let follow = spargio::fs::DuOptions::default().symlink_mode(spargio::fs::DuSymlinkMode::Follow);
    let _follow_summary = tokio::time::timeout(
        Duration::from_secs(2),
        spargio::fs::du(&rt.handle(), &root, follow),
    )
    .await
    .expect("du follow timeout")
    .expect("du follow");

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn du_error_policy_skip_and_fail_fast_behave_as_configured() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-error-mode");
    fs::create_dir_all(&root).expect("mkdir root");
    symlink("missing-target", root.join("broken")).expect("broken symlink");

    let fail_fast =
        spargio::fs::DuOptions::default().symlink_mode(spargio::fs::DuSymlinkMode::Follow);
    let fail = spargio::fs::du(&rt.handle(), &root, fail_fast).await;
    assert!(
        fail.is_err(),
        "follow mode with broken symlink should fail in fail-fast mode"
    );

    let skip = spargio::fs::du(
        &rt.handle(),
        &root,
        fail_fast.error_mode(spargio::fs::DuErrorMode::Skip),
    )
    .await
    .expect("du skip errors");
    assert!(skip.skipped_errors >= 1);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn du_one_file_system_skips_cross_device_targets() {
    let Some(rt) = try_build_runtime() else {
        return;
    };

    let root = unique_temp_dir("spargio-du-one-fs");
    fs::create_dir_all(&root).expect("mkdir root");
    symlink("/proc", root.join("proc-link")).expect("cross-device symlink");

    let root_dev = fs::symlink_metadata(&root).expect("root metadata").dev();
    let proc_dev = fs::metadata("/proc")
        .map(|m| m.dev())
        .expect("/proc metadata");

    let summary = spargio::fs::du(
        &rt.handle(),
        &root,
        spargio::fs::DuOptions::default()
            .symlink_mode(spargio::fs::DuSymlinkMode::Follow)
            .one_file_system(true)
            .error_mode(spargio::fs::DuErrorMode::Skip),
    )
    .await
    .expect("du one_file_system");

    if proc_dev != root_dev {
        assert!(
            summary.skipped_cross_device >= 1,
            "expected cross-device symlink target skip to be counted"
        );
    }

    let _ = fs::remove_dir_all(root);
}
