use std::fs;
use std::path::Path;

#[test]
fn companion_ci_smoke_script_exists_and_targets_companion_crates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script_path = root.join("scripts/companion_ci_smoke.sh");
    assert!(
        script_path.is_file(),
        "missing scripts/companion_ci_smoke.sh"
    );
    let script = fs::read_to_string(&script_path).expect("read companion_ci_smoke.sh");
    for package in [
        "spargio-protocols",
        "spargio-tls",
        "spargio-ws",
        "spargio-quic",
        "spargio-process",
        "spargio-signal",
    ] {
        assert!(
            script.contains(package),
            "expected companion smoke script to include package {package}"
        );
    }
}

#[test]
fn ci_workflow_has_companion_matrix_lane() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow_path = root.join(".github/workflows/ci.yml");
    assert!(workflow_path.is_file(), "missing CI workflow");
    let workflow = fs::read_to_string(&workflow_path).expect("read CI workflow");
    assert!(
        workflow.contains("companion-matrix"),
        "expected CI workflow to define companion-matrix job"
    );
    assert!(
        workflow.contains("./scripts/companion_ci_smoke.sh"),
        "expected companion-matrix lane to run companion smoke script"
    );
}
