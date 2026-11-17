//! `xtask release red` end to end: the real `xtask` binary runs with the stand-in `gh`
//! (src/bin/gh-fake.rs) as `--gh`, answered from files under the test's scratch directory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-red-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("dir");
    d
}

fn put(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("parents");
    std::fs::write(file, text).expect("write");
}

fn red(d: &Path, run: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(d)
        .args(["release", "red", "--repo", "example/tierstore"])
        .args(["--stage", "beta", "--run", run, "--job", "red"])
        .args(["--commit", COMMIT, "--complete", "true", "--gh", FAKE])
        .output()
        .expect("run xtask")
}

#[test]
fn a_failed_job_opens_the_issue_and_exits_zero() {
    // Asserts: the arguments reach the verb; with one failed job and no open issue it posts
    // the issue with the title and the table, prints where it opened, and exits 0.
    let d = scratch("opens");
    put(
        &d,
        "repos/example/tierstore/actions/runs/42/jobs?per_page=100",
        r#"{"total_count":2,"jobs":[
            {"name":"arm64","conclusion":"failure","html_url":"https://github.com/example/tierstore/actions/runs/42/job/7"},
            {"name":"red","conclusion":null}]}"#,
    );
    put(
        &d,
        "repos/example/tierstore/issues?state=open&per_page=100&page=1",
        "[]",
    );
    put(
        &d,
        "repos/example/tierstore/issues",
        r#"{"number":9,"html_url":"https://github.com/example/tierstore/issues/9"}"#,
    );
    let o = red(&d, "42");
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "RELEASE-RED: tierstore beta: opened https://github.com/example/tierstore/issues/9\n"
    );
    let sent: Value = serde_json::from_str(
        &std::fs::read_to_string(d.join("repos/example/tierstore/issues.body")).expect("body"),
    )
    .expect("json");
    assert_eq!(
        sent,
        json!({
            "title": "RELEASE-RED: tierstore beta",
            "body": format!(
                "RELEASE-RED: tierstore beta failed at {COMMIT}.\n\nRun: https://github.com/example/tierstore/actions/runs/42\n\n| job | conclusion |\n|---|---|\n| [arm64](https://github.com/example/tierstore/actions/runs/42/job/7) | failure |\n"
            ),
        })
    );
}

#[test]
fn a_run_id_that_is_not_digits_is_a_usage_error() {
    // Asserts: exit status 2 and the message on stderr, before any call to GitHub.
    let d = scratch("usage");
    let o = red(&d, "x");
    assert_eq!(o.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("--run \"x\" is not a run id"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
