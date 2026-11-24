//! `release macos-assets` end to end: the verb is run as a process, and its exit status and words
//! are read. This holds its dispatch, the flags clap checks, the refusal of a version or commit
//! that would misname the files (exit 1), and that it runs its steps with the Cargo that runs
//! it (`CARGO`, which Cargo sets for the process it runs): one that cannot start ends the run at
//! the first step, after the scratch directory is made. The steps themselves are tested beside
//! the verb, and equal to the hosted macOS job's by the workflow lint.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::Path;
use std::process::{Command, Output};

use tierstore_testkit::dir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(["release", "macos-assets"])
        .args(args)
        .env("CARGO", root.join("no-cargo"))
        .output()
        .expect("run xtask")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_version_or_commit_that_would_misname_the_files_ends_1() {
    // Catches: the verb not dispatched from `release`, or a version that is not X.Y.Z or a
    // commit that is not a full SHA taken (both go into every file name).
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cli-macos-assets-refused");
    let out = run(
        dir.path(),
        &["--version", "1.2", "--commit", COMMIT, "--scratch", "s"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "xtask: --version \"1.2\" is not <major>.<minor>.<patch>\n"
    );
    let out = run(
        dir.path(),
        &[
            "--version",
            "1.2.3",
            "--commit",
            &COMMIT[..12],
            "--scratch",
            "s",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("xtask: not a commit: \"0123456789ab\""),
        "{}",
        stderr(&out)
    );
    assert!(
        !dir.path().join("s").exists(),
        "nothing is made for wrong flags"
    );
}

#[test]
fn a_missing_flag_is_a_usage_error() {
    // Catches: a flag the verb needs made optional, so a run without it builds under a default.
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cli-macos-assets-usage");
    for missing in ["--version", "--commit", "--scratch"] {
        let mut args = vec!["--version", "1.2.3", "--commit", COMMIT, "--scratch", "s"];
        let at = args.iter().position(|a| *a == missing).expect("the flag");
        args.drain(at..at + 2);
        let out = run(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{missing}");
        assert!(
            stderr(&out).contains(missing),
            "{missing}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn the_steps_run_with_the_cargo_that_runs_the_verb_from_the_checkout() {
    // Catches: the steps started with another Cargo than the one running xtask, the scratch
    // directory not made below the checkout (`--root`), and a step that cannot start passed over:
    // the first step, `cargo cyclonedx --version`, fails to start and the run ends 1 naming it.
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cli-macos-assets-cargo");
    let out = run(
        dir.path(),
        &["--version", "1.2.3", "--commit", COMMIT, "--scratch", "t/s"],
    );
    assert_eq!(out.status.code(), Some(1));
    let said = stderr(&out);
    assert!(
        said.starts_with("macos-assets: step 1 of 21: cargo cyclonedx --version\n"),
        "{said}"
    );
    assert!(
        said.contains(&format!(
            "xtask: run {}: ",
            dir.path().join("no-cargo").display()
        )),
        "{said}"
    );
    assert!(dir.path().join("t/s").is_dir());
}
