use super::*;
use crate::testdir::TestDir;

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(beside.parent().expect("deps").join("xtask-gate-unit"), name)
}

fn args(settings: &[&str]) -> Vec<String> {
    settings.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn a_job_with_every_setting_is_enabled() {
    // Catches: a gate that keeps a configured channel off.
    let dir = scratch("all");
    let out = dir.join("out");
    let said = gate("tap", &args(&["A=1", "B_2=set"]), &[], Some(&out)).expect("enabled");
    assert_eq!(said, ["tap: all its settings are set"]);
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=true\n"
    );
}

#[test]
fn a_job_with_no_setting_is_a_green_notice_that_writes_nothing() {
    // Catches: a channel that runs, or fails, before its owner has made its settings.
    let dir = scratch("none");
    let out = dir.join("out");
    let said = gate("tap", &args(&["A=", "B="]), &[], Some(&out)).expect("a no-op is green");
    assert_eq!(
        said,
        [
            "::notice title=The tap job is off::None of its settings (A, B) is set, so it writes nothing and ends green."
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn a_job_with_some_settings_fails_and_names_the_missing_ones() {
    // Catches: a typo in one variable name leaving a channel silently off.
    let dir = scratch("some");
    let out = dir.join("out");
    let e = gate("tap", &args(&["A=1", "B=", "C=", "D=x"]), &[], Some(&out)).unwrap_err();
    assert_eq!(
        e,
        Error::Failed(
            "tap: A, D set but B, C not: set all of them or none, so that a channel is never left half on"
                .to_owned()
        )
    );
    // The later steps are skipped even if a caller ignored the failure.
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn the_answer_is_appended_to_the_output_file_and_is_optional() {
    // Catches: an answer that replaces what GITHUB_OUTPUT already holds.
    let dir = scratch("append");
    let out = dir.join("out");
    std::fs::write(&out, "earlier=1\n").expect("seed");
    gate("up", &args(&["A=1"]), &[], Some(&out)).expect("enabled");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "earlier=1\nenabled=true\n"
    );
    assert!(gate("up", &args(&["A=1"]), &[], None).is_ok());
}

#[test]
fn flags_that_are_not_of_their_form_are_usage_errors() {
    // Catches: a setting without a value part, a name that is not a variable name, a name given
    // twice, and a label that could forge a workflow command.
    for (label, settings) in [
        ("tap", args(&["A"])),
        ("tap", args(&["=1"])),
        ("tap", args(&["a-b=1"])),
        ("tap", args(&["A=1", "A="])),
        ("tap", args(&["A=", "A="])),
        ("", args(&["A=1"])),
        ("t\nap", args(&["A=1"])),
        ("t::ap", args(&["A=1"])),
    ] {
        let e = gate(label, &settings, &[], None).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{label:?} {settings:?}: {e}");
    }
}

#[test]
fn a_job_whose_earlier_job_was_off_is_off_and_green_without_judging_its_own_settings() {
    // Catches: the apt index failing "some set" because the read job it follows was off.
    let dir = scratch("require-off");
    let out = dir.join("out");
    let said = gate(
        "apt index",
        &args(&["KEY=set", "FPR="]),
        &args(&["READ_ENABLED=", "SIGNED=true", "OTHER=false"]),
        Some(&out),
    )
    .expect("off is green");
    assert_eq!(
        said,
        [
            "::notice title=The apt index job is off::READ_ENABLED, OTHER is not true, so it writes nothing and ends green."
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "enabled=false\n"
    );
}

#[test]
fn a_job_whose_earlier_jobs_said_true_is_judged_by_its_own_settings() {
    // Catches: a requirement that switches the settings check off.
    let dir = scratch("require-on");
    let out = dir.join("out");
    let ok = gate(
        "apt index",
        &args(&["KEY=set"]),
        &args(&["READ_ENABLED=true"]),
        Some(&out),
    );
    assert_eq!(
        ok.expect("enabled"),
        ["apt index: all its settings are set"]
    );
    let half = gate(
        "apt index",
        &args(&["KEY=set", "FPR="]),
        &args(&["READ_ENABLED=true"]),
        None,
    );
    assert!(half.unwrap_err().to_string().contains("FPR not"));
}

#[test]
fn a_requirement_that_is_not_name_value_is_a_usage_error() {
    // Catches: a typo in the workflow reading as "not true" and silently turning a channel off.
    let e = gate("apt index", &args(&["A=1"]), &args(&["READ_ENABLED"]), None).unwrap_err();
    assert_eq!(
        e,
        Error::Usage("--require \"READ_ENABLED\" is not NAME=VALUE".to_owned())
    );
}
