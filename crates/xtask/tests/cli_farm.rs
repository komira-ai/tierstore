//! `cargo xtask farm-build` end to end, and the flags of `fetch --extract zst` that install the
//! farm lane's buck2: the verb is run as a process with `--buck2` naming the stand-in
//! (src/bin/gh-fake.rs), which answers each buck2 call from files of the working directory and
//! records the calls it got (`buck2.calls`). Each test reads the verb's exit status and words, and
//! those calls.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");

/// What buck2 prints for a key that is set.
const SET: &str = r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#;
/// A farm build's summary line.
const FARM: &str = "Commands: 474 (cached: 0, remote: 474, local: 0)";
/// The audit call.
const AUDIT: &str = "audit config buildrules_re.linux_x86_64_properties --style json";
/// The calls of a run that got as far as the build.
const BUILT: [&str; 3] = [AUDIT, "build //:ci", "kill"];

/// A fresh directory with `answers` (file name, text) in it.
fn dir(name: &str, answers: &[(&str, &str)]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-farm-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    for (file, text) in answers {
        std::fs::write(dir.join(file), text).expect("an answer");
    }
    dir
}

fn xtask(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

fn farm_build(cwd: &Path, buck2: &str) -> Output {
    xtask(cwd, &["farm-build", "--buck2", buck2, "--target", "//:ci"])
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("buck2.calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_farm_build_prints_its_summary_line_and_stops_buck2_with_status_0() {
    // Catches: the build not run, run before the worker check, its log not passed on, its
    // summary not printed, or buck2's daemon left running.
    let log = format!("[t] Waiting on 3 actions\n[t] {FARM}\nBUILD SUCCEEDED\n");
    let d = dir(
        "ok",
        &[("buck2-audit.stdout", SET), ("buck2-build.stderr", &log)],
    );
    let o = farm_build(&d, FAKE);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FARM}\n"));
    assert_eq!(stderr(&o), log);
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_build_with_actions_on_this_machine_fails_with_status_1() {
    // Catches: a build that ran an action on the runner taken for a farm build.
    let line = "Commands: 5 (cached: 2, remote: 1, local: 2)";
    let d = dir(
        "local",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", &format!("{line}\nBUILD SUCCEEDED\n")),
        ],
    );
    let o = farm_build(&d, FAKE);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        format!(
            "{line}\nBUILD SUCCEEDED\nxtask: `{line}`: actions ran on this machine, not on the farm (local must be 0)\n"
        )
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_build_with_no_summary_line_fails_with_status_1() {
    // Catches: a build whose summary is missing taken for one that ran nothing locally.
    let d = dir(
        "no-summary",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", "BUILD SUCCEEDED\n"),
        ],
    );
    let o = farm_build(&d, FAKE);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).ends_with(
            "xtask: the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)\n"
        ),
        "{}",
        stderr(&o)
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn a_failed_build_fails_with_status_1_shows_its_log_and_still_stops_buck2() {
    // Catches: a failed build read as a pass because its summary says `local: 0`, its log (the
    // failing action's words) hidden, or the daemon left running after a failure.
    let log = format!("Action failed: test of tierstore\n{FARM}\nBUILD FAILED\n");
    let d = dir(
        "failed",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", &log),
            ("buck2-build.status", "3"),
        ],
    );
    let o = farm_build(&d, FAKE);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        format!("{log}xtask: `buck2 build //:ci` failed (exit status: 3)\n")
    );
    assert_eq!(calls(&d), BUILT);
}

#[test]
fn no_worker_property_set_fails_before_the_build_with_status_1() {
    // Catches: a build started with no farm to run on (buck2 would then fail at analysis, or a
    // later change could fall back to this machine): the key unset, and set to nothing.
    for (name, audit) in [
        ("unset", "{}"),
        ("empty", r#"{"buildrules_re.linux_x86_64_properties":""}"#),
    ] {
        let d = dir(&format!("workers-{name}"), &[("buck2-audit.stdout", audit)]);
        let o = farm_build(&d, FAKE);
        assert_eq!(o.status.code(), Some(1), "{name}");
        assert_eq!(
            stderr(&o),
            "xtask: buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no \
             [komira_re] linux_x86_64_properties, so buck2 has no farm to build on\n",
            "{name}"
        );
        assert_eq!(calls(&d), [AUDIT, "kill"], "{name}");
    }
}

#[test]
fn a_failed_audit_fails_before_the_build_with_status_1() {
    // Catches: an audit that failed (no .buckconfig, a broken cell) read as a set key.
    let d = dir(
        "audit-fails",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-audit.stderr", "no .buckconfig\n"),
            ("buck2-audit.status", "3"),
        ],
    );
    let o = farm_build(&d, FAKE);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "xtask: `buck2 audit config` failed (exit status: 3): no .buckconfig\n"
    );
    assert_eq!(calls(&d), [AUDIT, "kill"]);
}

#[test]
fn a_buck2_that_cannot_start_fails_with_status_1_and_the_flags_are_required() {
    // Catches: a missing buck2 taken for a passing build, and a verb that runs without its
    // target or its buck2.
    let d = dir("no-buck2", &[]);
    let o = farm_build(&d, "/nonexistent/buck2");
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buck2\": "),
        "{}",
        stderr(&o)
    );
    for args in [
        &["farm-build", "--buck2", FAKE][..],
        &["farm-build", "--target", "//:ci"][..],
    ] {
        let o = xtask(&d, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(stderr(&o).contains("required"), "{}", stderr(&o));
    }
    assert_eq!(calls(&d), Vec::<String>::new());
}

#[test]
fn fetch_with_zst_and_a_member_or_a_tree_is_a_usage_error_with_status_2() {
    // Catches: `--extract zst` given a member or a tree that it would ignore, reported as a
    // failure (1) instead of a wrong command line (2), or reached only after a download.
    let d = dir("zst-flags", &[]);
    for extra in [&["--member", "buck2"][..], &["--tree"][..]] {
        let mut args = vec![
            "fetch",
            "--url",
            "http://127.0.0.1:9/buck2.zst",
            "--sha256",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "--out",
            "buck2",
            "--extract",
            "zst",
        ];
        args.extend_from_slice(extra);
        let o = xtask(&d, &args);
        assert_eq!(o.status.code(), Some(2), "{extra:?}");
        assert_eq!(
            stderr(&o),
            "xtask: --extract zst is one compressed file: it takes no --member and no --tree\n"
        );
        assert!(!d.join("buck2").exists());
    }
}
