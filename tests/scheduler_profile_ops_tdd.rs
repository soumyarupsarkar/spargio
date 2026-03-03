use std::fs;
use std::path::Path;

#[test]
fn scheduler_profile_scripts_exist_and_are_wired_in_ci() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let profile_script_path = root.join("scripts/bench_scheduler_profile.sh");
    let guardrail_script_path = root.join("scripts/scheduler_profile_guardrail.sh");
    let fixture_path =
        root.join("tests/fixtures/scheduler_profile/fanout_fanin_skewed_spargio_io_uring.json");
    assert!(
        profile_script_path.is_file(),
        "missing scripts/bench_scheduler_profile.sh"
    );
    assert!(
        guardrail_script_path.is_file(),
        "missing scripts/scheduler_profile_guardrail.sh"
    );
    assert!(
        fixture_path.is_file(),
        "missing scheduler profiler baseline fixture"
    );

    let profile_script =
        fs::read_to_string(&profile_script_path).expect("read bench_scheduler_profile.sh");
    assert!(
        profile_script.contains("callgrind") && profile_script.contains("cachegrind"),
        "expected scheduler profiler script to run callgrind/cachegrind"
    );

    let guardrail_script =
        fs::read_to_string(&guardrail_script_path).expect("read scheduler_profile_guardrail.sh");
    assert!(
        guardrail_script.contains("MAX_CALLGRIND_IR_RATIO")
            && guardrail_script.contains("MAX_CACHEGRIND_D1MR_RATIO"),
        "expected scheduler guardrail script to compare profiler ratios"
    );

    let workflow_path = root.join(".github/workflows/ci.yml");
    let workflow = fs::read_to_string(&workflow_path).expect("read CI workflow");
    assert!(
        workflow.contains("./scripts/bench_scheduler_profile.sh"),
        "expected CI workflow to run scheduler profiling lane"
    );
}
