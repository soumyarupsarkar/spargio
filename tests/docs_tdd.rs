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
