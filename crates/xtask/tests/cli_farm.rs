//! `cargo xtask farm-check` end to end, and the flags of `fetch --extract zst` that install the
//! farm lane's buck2: the verb is run as a process with `--buck2` naming the stand-in
//! (src/bin/gh-fake.rs), which answers buck2's `audit` from files of the working directory and
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
/// The audit call.
const AUDIT: &str = "audit config buildrules_re.linux_x86_64_properties --style json";

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

fn farm_check(cwd: &Path, buck2: &str) -> Output {
    xtask(cwd, &["farm-check", "--buck2", buck2])
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
fn a_set_worker_property_set_passes_with_status_0_and_one_audit() {
    // Catches: the key not asked for, asked for by another name, a pass that prints nothing, or
    // a check that runs anything more than the one audit (a build, a daemon stop).
    let d = dir("set", &[("buck2-audit.stdout", SET)]);
    let o = farm_check(&d, FAKE);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "buildrules_re.linux_x86_64_properties is set: buck2 builds on the farm\n"
    );
    assert_eq!(stderr(&o), "");
    assert_eq!(calls(&d), [AUDIT]);
}

#[test]
fn no_worker_property_set_fails_with_status_1() {
    // Catches: a build let start with no farm to run on (buck2 would then fail at analysis, or a
    // later change could fall back to this machine): the key unset, and set to nothing.
    for (name, audit) in [
        ("unset", "{}"),
        ("empty", r#"{"buildrules_re.linux_x86_64_properties":""}"#),
    ] {
        let d = dir(&format!("workers-{name}"), &[("buck2-audit.stdout", audit)]);
        let o = farm_check(&d, FAKE);
        assert_eq!(o.status.code(), Some(1), "{name}");
        assert_eq!(
            stderr(&o),
            "xtask: buildrules_re.linux_x86_64_properties is empty: farm-connect wrote no \
             [komira_re] linux_x86_64_properties, so buck2 has no farm to build on\n",
            "{name}"
        );
        assert_eq!(calls(&d), [AUDIT], "{name}");
    }
}

#[test]
fn a_failed_audit_fails_with_status_1() {
    // Catches: an audit that failed (no .buckconfig, a broken cell) read as a set key.
    let d = dir(
        "audit-fails",
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-audit.stderr", "no .buckconfig\n"),
            ("buck2-audit.status", "3"),
        ],
    );
    let o = farm_check(&d, FAKE);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "xtask: `buck2 audit config` failed (exit status: 3): no .buckconfig\n"
    );
    assert_eq!(calls(&d), [AUDIT]);
}

#[test]
fn a_buck2_that_cannot_start_fails_with_status_1_and_the_flag_is_required() {
    // Catches: a missing buck2 taken for a set key, and a verb that runs without its buck2.
    let d = dir("no-buck2", &[]);
    let o = farm_check(&d, "/nonexistent/buck2");
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buck2\": "),
        "{}",
        stderr(&o)
    );
    let o = xtask(&d, &["farm-check"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("required"), "{}", stderr(&o));
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
            "https://example.invalid/buck2.zst",
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
