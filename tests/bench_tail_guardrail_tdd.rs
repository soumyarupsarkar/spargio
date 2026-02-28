use std::process::Command;

fn run_script(max_p95_ratio: &str) -> std::process::Output {
    Command::new("bash")
        .arg("scripts/bench_tail_guardrail.sh")
        .arg("steady_ping_pong_rtt")
        .arg("tokio_two_worker")
        .arg("spargio_io_uring")
        .env("RUN_BENCH", "0")
        .env("CRITERION_DIR", "tests/fixtures/criterion")
        .env("MAX_P50_RATIO", "1.20")
        .env("MAX_P95_RATIO", max_p95_ratio)
        .env("MAX_P99_RATIO", "1.20")
        .output()
        .expect("run guardrail script")
}

#[test]
fn percentile_guardrail_passes_for_fixture_profile() {
    let output = run_script("1.20");
    assert!(
        output.status.success(),
        "expected success, stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("p50=") && stdout.contains("p95=") && stdout.contains("p99="),
        "expected percentile summary in stdout: {stdout}"
    );
}

#[test]
fn percentile_guardrail_fails_when_threshold_is_too_strict() {
    let output = run_script("1.03");
    assert!(
        !output.status.success(),
        "expected failure, stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
