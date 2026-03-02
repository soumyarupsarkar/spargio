use std::path::Path;
use std::process::Command;

fn run_script(max_p95: &str, max_p99: &str, min_tp_ratio: &str) -> std::process::Output {
    Command::new("bash")
        .arg("scripts/quic_perf_gate.sh")
        .env(
            "QUIC_PERF_FIXTURE",
            Path::new("tests/fixtures/quic_perf/native_vs_bridge.json"),
        )
        .env("MAX_P95_REGRESSION_PCT", max_p95)
        .env("MAX_P99_REGRESSION_PCT", max_p99)
        .env("MIN_THROUGHPUT_RATIO", min_tp_ratio)
        .output()
        .expect("run quic perf gate script")
}

#[test]
fn quic_perf_gate_passes_for_fixture_profile() {
    let output = run_script("15.0", "20.0", "0.90");
    assert!(
        output.status.success(),
        "expected success, stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("quic perf gate: pass=true"),
        "expected pass=true summary in stdout: {stdout}"
    );
}

#[test]
fn quic_perf_gate_fails_for_strict_thresholds() {
    let output = run_script("5.0", "8.0", "0.98");
    assert!(
        !output.status.success(),
        "expected failure, stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("quic perf gate failed"),
        "expected failure details in stderr: {stderr}"
    );
}
