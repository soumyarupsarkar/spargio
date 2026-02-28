use std::fs;
use std::path::Path;

#[test]
fn workspace_lists_companion_subcrates() {
    let cargo_toml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read Cargo.toml");
    assert!(
        cargo_toml.contains("[workspace]"),
        "expected root Cargo.toml to define a workspace"
    );
    assert!(
        cargo_toml.contains("crates/spargio-signal"),
        "expected signal companion crate in workspace members"
    );
    assert!(
        cargo_toml.contains("crates/spargio-protocols"),
        "expected protocol integration crate in workspace members"
    );
    assert!(
        cargo_toml.contains("crates/spargio-process"),
        "expected process companion crate in workspace members"
    );
}
