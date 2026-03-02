use std::fs;
use std::path::{Path, PathBuf};

fn book_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("book")
}

#[test]
fn mdbook_scaffold_exists_with_summary() {
    let root = book_root();
    assert!(root.join("book.toml").is_file(), "missing book/book.toml");
    assert!(
        root.join("src/SUMMARY.md").is_file(),
        "missing book/src/SUMMARY.md"
    );
}

#[test]
fn mdbook_summary_links_resolve_to_existing_files() {
    let root = book_root();
    let src_dir = root.join("src");
    let summary = fs::read_to_string(src_dir.join("SUMMARY.md")).expect("read SUMMARY.md");
    for line in summary.lines() {
        let Some(start) = line.find("](") else {
            continue;
        };
        let tail = &line[start + 2..];
        let Some(end) = tail.find(')') else {
            continue;
        };
        let rel = &tail[..end];
        if rel.starts_with("http://") || rel.starts_with("https://") || rel.starts_with('#') {
            continue;
        }
        let candidate = src_dir.join(rel);
        assert!(
            candidate.is_file(),
            "missing chapter referenced by SUMMARY: {rel}"
        );
    }
}

#[test]
fn readme_tracks_quic_rollout_done_and_not_done_status() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).expect("read README.md");
    assert!(
        readme.contains("QuicBackend::Native") && readme.contains("QuicBackend::Bridge"),
        "expected README done section to describe QUIC backend selector"
    );
    assert!(
        readme.contains("quinn-proto")
            && readme.contains("NativeProtoDriver")
            && readme.contains("rollout_stage"),
        "expected README to describe driver-backed native QUIC path plus remaining rollout-hardening work"
    );
    for script in [
        "./scripts/quic_interop_matrix.sh",
        "./scripts/quic_perf_gate.sh",
        "./scripts/quic_soak_fault.sh",
    ] {
        assert!(
            readme.contains(script),
            "expected README helper list to include {script}"
        );
    }
}

#[test]
fn implementation_log_contains_r1_to_r9_breakdown_sections() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let log =
        fs::read_to_string(root.join("IMPLEMENTATION_LOG.md")).expect("read IMPLEMENTATION_LOG.md");
    for heading in [
        "### R1:", "### R2:", "### R3:", "### R4:", "### R5:", "### R6:", "### R7:", "### R8:",
        "### R9:",
    ] {
        assert!(
            log.contains(heading),
            "expected implementation log to include milestone heading {heading}"
        );
    }
}
