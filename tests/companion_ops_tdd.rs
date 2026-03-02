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
    assert!(
        workflow.contains("./scripts/companion_ci_hardening.sh"),
        "expected companion-matrix lane to run companion hardening script"
    );
    assert!(
        workflow.contains("./scripts/quic_interop_matrix.sh"),
        "expected companion-matrix lane to run QUIC interop matrix script"
    );
    assert!(
        workflow.contains("./scripts/quic_soak_fault.sh"),
        "expected CI workflow to wire QUIC soak/fault nightly script"
    );
}

#[test]
fn quic_interop_matrix_script_exists_and_targets_interop_suite() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script_path = root.join("scripts/quic_interop_matrix.sh");
    assert!(
        script_path.is_file(),
        "missing scripts/quic_interop_matrix.sh"
    );
    let script = fs::read_to_string(&script_path).expect("read quic_interop_matrix.sh");
    assert!(
        script.contains("cargo test -p spargio-quic --test interop_tdd"),
        "expected QUIC interop script to execute interop_tdd suite"
    );
}

#[test]
fn quic_soak_fault_script_exists_and_targets_ignored_soak_suite() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script_path = root.join("scripts/quic_soak_fault.sh");
    assert!(script_path.is_file(), "missing scripts/quic_soak_fault.sh");
    let script = fs::read_to_string(&script_path).expect("read quic_soak_fault.sh");
    assert!(
        script.contains("cargo test -p spargio-quic --test soak_tdd -- --ignored"),
        "expected QUIC soak script to execute ignored soak_tdd suite"
    );
}

#[test]
fn companion_hardening_script_exists_and_runs_broader_suites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script_path = root.join("scripts/companion_ci_hardening.sh");
    assert!(
        script_path.is_file(),
        "missing scripts/companion_ci_hardening.sh"
    );
    let script = fs::read_to_string(&script_path).expect("read companion_ci_hardening.sh");
    for command in [
        "cargo test -p spargio-process --tests",
        "cargo test -p spargio-signal --tests",
        "cargo test -p spargio-protocols --tests --features uring-native",
        "cargo test -p spargio-tls --tests",
        "cargo test -p spargio-ws --tests",
        "cargo test -p spargio-quic --test interop_tdd",
    ] {
        assert!(
            script.contains(command),
            "expected hardening script to include command: {command}"
        );
    }
}
