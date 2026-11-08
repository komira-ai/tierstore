use super::*;
use crate::testdir::TestDir;

fn all(result: &str) -> Vec<String> {
    STAGES.iter().map(|s| format!("{s}={result}")).collect()
}

/// A run in which everything succeeded, but for `changes` (stage, result).
fn results(changes: &[(&str, &str)]) -> Vec<String> {
    let mut base = all("success");
    for (stage, result) in changes {
        let at = STAGES.iter().position(|s| s == stage).expect("a stage");
        base[at] = format!("{stage}={result}");
    }
    base
}

fn refusal(results: &[String], farm: &str) -> Vec<String> {
    match decide(results, farm, None) {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn a_release_every_stage_of_which_succeeded_is_promoted_and_says_so_in_the_output() {
    // Catches: a promotion whose answer never reaches GITHUB_OUTPUT.
    let dir = TestDir::under(
        std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("xtask-decide-unit"),
        "promoted",
    );
    let out = dir.join("out");
    let said = decide(&results(&[]), "true", Some(&out)).expect("promoted");
    assert_eq!(said, ["promote: the release is promoted from testing"]);
    assert_eq!(
        std::fs::read_to_string(&out).expect("output"),
        "promote=true\n"
    );
}

#[test]
fn a_lab_that_is_not_configured_is_skipped_with_a_notice_and_does_not_hold_the_release() {
    // Catches: a missing hostkeeper setting blocking every release, and a skipped lab that says
    // nothing.
    let skipped = results(&[("farm", "skipped"), ("soak", "skipped")]);
    for farm in ["false", ""] {
        let said = decide(&skipped, farm, None).expect("promoted");
        assert!(
            said[0].starts_with("::notice title=The lab stage was skipped::"),
            "{said:?}"
        );
        assert_eq!(said.len(), 2);
    }
}

#[test]
fn a_lab_that_is_configured_must_have_converged_and_soaked() {
    // Catches: a release promoted although the lab was set up and did not take it (the farm job
    // skipped, or the soak not run).
    let problems = refusal(
        &results(&[("farm", "skipped"), ("soak", "skipped")]),
        "true",
    );
    assert_eq!(
        problems,
        [
            "farm is skipped: the lab is configured, so it must be success",
            "soak is skipped: the lab is configured, so it must be success"
        ]
    );
    let problems = refusal(&results(&[("soak", "failure")]), "true");
    assert_eq!(
        problems,
        ["soak is failure: the lab is configured, so it must be success"]
    );
}

#[test]
fn a_lab_that_ran_without_being_configured_is_refused() {
    // Catches: a lab half on, which the rule cannot tell from a configuration mistake.
    let problems = refusal(&results(&[]), "false");
    assert_eq!(
        problems,
        [
            "farm is success: the lab is not configured, so it must be skipped",
            "soak is success: the lab is not configured, so it must be skipped"
        ]
    );
}

#[test]
fn a_failed_or_cancelled_stage_is_refused_and_every_such_stage_is_named() {
    // Catches: an install check that failed, or a testing publish that did not finish, letting
    // the release go public.
    let problems = refusal(
        &results(&[("check-apt", "failure"), ("testing-upload", "cancelled")]),
        "true",
    );
    assert_eq!(
        problems,
        [
            "testing-upload is cancelled: the release does not go on",
            "check-apt is failure: the release does not go on"
        ]
    );
}

#[test]
fn only_the_stages_that_may_be_skipped_may_be_skipped() {
    // Catches: a testing publish or the lab's gate skipped and read as nothing to do.
    for stage in MAY_SKIP {
        assert!(
            decide(&results(&[(stage, "skipped")]), "true", None).is_ok(),
            "{stage}"
        );
    }
    for stage in ["testing-upload", "testing-apt-write", "farm-check"] {
        let problems = refusal(&results(&[(stage, "skipped")]), "true");
        assert_eq!(
            problems,
            [format!("{stage} is skipped: the release does not go on")]
        );
    }
}

#[test]
fn flags_out_of_their_form_are_usage_errors() {
    // Catches: a typo in a stage name read as a stage that passed, a stage left out, and a result
    // the runner never gives.
    let usage = |results: Vec<String>, farm: &str| match decide(&results, farm, None) {
        Err(Error::Usage(why)) => why,
        other => panic!("not a usage error: {other:?}"),
    };
    assert!(usage(results(&[]), "yes").contains("--farm-enabled"));
    assert!(usage(vec!["check-apt".to_owned()], "true").contains("not NAME=RESULT"));
    assert!(usage(vec!["check-npm=success".to_owned()], "true").contains("no stage"));
    assert!(usage(results(&[("check-apt", "ok")]), "true").contains("not one of"));
    let mut twice = results(&[]);
    twice.push("check-apt=success".to_owned());
    assert_eq!(usage(twice, "true"), "--result check-apt is given twice");
    let mut missing = results(&[]);
    missing.retain(|r| !r.starts_with("soak="));
    assert_eq!(usage(missing, "true"), "--result soak is not given");
}
