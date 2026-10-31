use super::*;
use std::path::Path;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn a_commit_is_twelve_to_forty_lowercase_hex_digits() {
    // Catches: a short, long, upper-case or non-hex value accepted as a commit.
    assert!(is_commit("0123456789ab"));
    assert!(is_commit(COMMIT));
    for bad in [
        "",
        "0123456789a",
        "0123456789AB",
        "0123456789xy",
        &"a".repeat(41),
    ] {
        assert!(!is_commit(bad), "{bad}");
    }
    assert!(full_commit("--commit", COMMIT).is_ok());
    assert!(matches!(
        full_commit("--commit", "0123456789ab"),
        Err(Error::Usage(_))
    ));
}

#[test]
fn the_version_text_must_end_with_the_commit() {
    // Catches: a check that looks for the commit anywhere (the version number may hold the
    // same digits) or accepts `unknown`.
    assert!(names_commit("tierstore 0.2.0+0123456789ab\n", COMMIT));
    assert!(!names_commit("tierstore 0.2.0+unknown\n", COMMIT));
    assert!(!names_commit("tierstore 0.2.0+0123456789ac\n", COMMIT));
    assert!(!names_commit("tierstore 0123456789ab\n", COMMIT));
    assert!(!names_commit(
        "tierstore 0.2.0+0123456789ab extra\n",
        COMMIT
    ));
}

#[test]
fn the_other_commit_differs_in_the_last_digit_only() {
    // Catches: a control commit that equals the real one, or that moves a digit inside the
    // twelve the version text carries only by accident (the last of 40 is outside them).
    let other = other_commit(COMMIT);
    assert_ne!(other, COMMIT);
    assert_eq!(&other[..39], &COMMIT[..39]);
    assert!(other_commit("0000000000000000000000000000000000000000").ends_with('1'));
}

#[test]
fn the_short_control_differs_inside_the_twelve_digits_the_version_carries() {
    // Catches: a control that changes a digit `--version` does not print, so it can never fail.
    let other = other_short_commit(COMMIT);
    assert_eq!(other.len(), COMMIT.len());
    assert_eq!(&other[..11], &COMMIT[..11]);
    assert_eq!(&other[12..], &COMMIT[12..]);
    assert_ne!(&other[11..12], &COMMIT[11..12]);
    assert!(other_short_commit("000000000000").ends_with('1'));
    assert!(names_commit("tierstore 0.2.0+0123456789ab", COMMIT));
    assert!(!names_commit("tierstore 0.2.0+0123456789ab", &other));
}

fn says(text: &'static str) -> fn(&Path) -> Result<Said, Error> {
    // A fn pointer cannot capture, so each text the tests need has its own.
    match text {
        "ok" => |_| Ok(said(true, "tierstore 0.2.0+0123456789ab\n", "")),
        "other" => |_| Ok(said(true, "tierstore 0.2.0+ffffffffffff\n", "")),
        "unknown" => |_| Ok(said(true, "tierstore 0.2.0+unknown\n", "")),
        "fails" => |_| Ok(said(false, "", "boom\n")),
        _ => |_| {
            Err(Error::Spawn {
                what: "x".into(),
                why: "missing".into(),
            })
        },
    }
}

fn said(success: bool, stdout: &str, stderr: &str) -> Said {
    Said {
        success,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

#[test]
fn a_binary_that_names_the_commit_passes() {
    // Catches: the check refusing the form `--version` really prints.
    let line = check_with(Path::new("tierstore"), COMMIT, says("ok")).expect("check");
    assert_eq!(
        line,
        "tierstore: tierstore 0.2.0+0123456789ab names commit 0123456789ab"
    );
}

#[test]
fn a_binary_that_names_another_commit_or_none_is_refused() {
    // Catches: a binary built from another commit, or without a git checkout, released.
    for which in ["other", "unknown"] {
        let result = check_with(Path::new("tierstore"), COMMIT, says(which));
        assert!(matches!(result, Err(Error::WrongCommit { .. })), "{which}");
    }
}

#[test]
fn a_binary_that_fails_or_cannot_run_is_an_error_and_a_bad_commit_is_refused() {
    // Catches: a failing `--version` read as a pass, or a missing binary skipped.
    let bin = Path::new("tierstore");
    let failed = check_with(bin, COMMIT, says("fails")).expect_err("fails");
    assert_eq!(failed.to_string(), "tierstore --version failed: boom");
    assert!(matches!(
        check_with(bin, COMMIT, says("missing")),
        Err(Error::Spawn { .. })
    ));
    assert!(matches!(
        check_with(bin, "not a commit", says("ok")),
        Err(Error::NotACommit(_))
    ));
}

#[test]
fn the_real_runner_reports_a_program_that_is_not_there() {
    // Catches: `run_version` turning a missing program into a success.
    let result = run_version(Path::new("/nonexistent/tierstore"));
    assert!(matches!(result, Err(Error::Spawn { .. })));
}

#[test]
fn a_check_that_accepts_any_text_fails_its_control() {
    // Catches: a check that cannot fail. A reader that accepts every text passes the real
    // commit, and the control (a commit the binary was not built from) must then refuse it.
    let result = check_naming(Path::new("tierstore"), COMMIT, says("ok"), |_, _| true);
    assert!(
        matches!(result, Err(Error::ControlPassed { .. })),
        "{result:?}"
    );
}

#[test]
fn a_full_length_text_that_is_not_hex_is_not_a_full_commit() {
    // Catches: the length alone taken for a commit.
    assert!(matches!(
        full_commit("--commit", &"z".repeat(40)),
        Err(Error::Usage(_))
    ));
}

#[test]
fn the_check_verb_runs_the_binary() {
    // Catches: `check` not reaching the real runner. `false` exits 1 whatever it is given, so
    // the verb reports a failed `--version`.
    let result = check(Path::new("false"), COMMIT);
    assert!(matches!(result, Err(Error::Failed(_))), "{result:?}");
}
