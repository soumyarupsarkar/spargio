use std::fs;
use std::path::Path;

#[test]
fn quic_perf_gate_script_exists_and_is_wired_in_ci() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script_path = root.join("scripts/quic_perf_gate.sh");
    assert!(script_path.is_file(), "missing scripts/quic_perf_gate.sh");
    let script = fs::read_to_string(&script_path).expect("read quic_perf_gate.sh");
    assert!(
        script.contains("p95_regression_pct") && script.contains("throughput_ratio"),
        "expected script to emit p95/p99 and throughput verdict details"
    );

    let workflow_path = root.join(".github/workflows/ci.yml");
    let workflow = fs::read_to_string(&workflow_path).expect("read CI workflow");
    assert!(
        workflow.contains("./scripts/quic_perf_gate.sh"),
        "expected CI workflow to run QUIC perf gate script"
    );
}
