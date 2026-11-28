//! The `tierstore-blackbox` program, from outside: its flags and exit statuses, and its report of
//! a case whose node does not start (this crate's `fake-s3` given as the node, which refuses
//! `--config`).

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::path::Path;
use std::process::{Command, Output};

use tierstore_testkit::dir::TestDir;

fn driver(scratch: &Path, flags: &[&str]) -> Output {
    Command::new(support::DRIVER)
        .arg("--tierstore")
        .arg(support::FAKE_S3)
        .arg("--fake-s3")
        .arg(support::FAKE_S3)
        .arg("--scratch")
        .arg(scratch)
        .args(flags)
        .output()
        .unwrap()
}

/// Asserts that a case whose node ends before its start line is reported FAIL with the reason,
/// that the driver exits 1, and that the case's directory has the logs of both programs; that a
/// case whose directory is there already is not run over it; that the cases whose nodes set
/// `trees.max_placements` (T2, T3) and `disk-full`, which reads the size of its small disk, fail
/// the same way; and that a small disk that is not there fails `disk-full`.
#[test]
fn a_case_whose_node_does_not_start_fails_with_the_reason() {
    let temp = std::env::temp_dir();
    let (dir, _) = TestDir::for_socket(
        &[temp.as_path(), Path::new(support::SCRATCH)],
        "driver",
        "upstream-errors/provider.sock",
    );
    let ran = driver(dir.path(), &["--case", "t1-cycles"]);
    assert_eq!(ran.status.code(), Some(1));
    let report = String::from_utf8(ran.stdout).unwrap();
    assert_eq!(
        report,
        "FAIL t1-cycles: the rig did not start: tierstore ended before it printed its start line (exit status: 2); see tierstore.err (saw: )\n"
    );
    let logs = dir.path().join("t1-cycles/logs");
    let refused = std::fs::read_to_string(logs.join("tierstore.err")).unwrap();
    assert!(refused.contains("--config"), "{refused}");
    assert!(logs.join("fake-s3.out").is_file());

    let again = driver(dir.path(), &["--case", "t1-cycles"]);
    let report = String::from_utf8(again.stdout).unwrap();
    assert!(
        report.starts_with(&format!(
            "FAIL t1-cycles: the rig did not start: {}: ",
            dir.path().join("t1-cycles").display()
        )),
        "{report}"
    );

    let small = dir.path().join("small");
    std::fs::create_dir(&small).unwrap();
    let full = driver(
        dir.path(),
        &[
            "--case",
            "disk-full",
            "--case",
            "t2-placements",
            "--case",
            "t3-stall",
            "--small-disk",
            small.to_str().unwrap(),
        ],
    );
    let report = String::from_utf8(full.stdout).unwrap();
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 3, "{report}");
    for (line, case) in lines.iter().zip(["t2-placements", "t3-stall", "disk-full"]) {
        assert!(
            line.starts_with(&format!(
                "FAIL {case}: the rig did not start: tierstore ended"
            )),
            "{report}"
        );
    }

    let gone = dir.path().join("gone");
    let no_size = driver(
        dir.path(),
        &[
            "--case",
            "disk-full",
            "--small-disk",
            gone.to_str().unwrap(),
        ],
    );
    let report = String::from_utf8(no_size.stdout).unwrap();
    assert!(
        report.starts_with("FAIL disk-full: the rig did not start: "),
        "{report}"
    );
}

/// Asserts the exit statuses of flags the driver refuses: 2 for a missing path and for
/// `disk-full` without a small disk (naming the flag), and 0 for `--help`.
#[test]
fn flags_the_driver_refuses_exit_2() {
    let missing = Command::new(support::DRIVER).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    let dir = TestDir::under(std::env::temp_dir(), "driver-flags");
    let no_size = driver(dir.path(), &[]);
    assert_eq!(no_size.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(no_size.stderr).unwrap(),
        "tierstore-blackbox: the case disk-full needs --small-disk, a file system of its own that it fills; give one, or name the other cases with --case\n"
    );
    let help = Command::new(support::DRIVER)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--small-disk")
    );
}
