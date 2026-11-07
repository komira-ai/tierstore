//! `cargo xtask mac-surface` end to end: the flags reach the verb, the answer is printed and
//! appended to `--output`, a list it cannot read exits 1, and a missing `--changed` exits 2. The
//! logic of the verb is tested beside it, in the library (`src/mac_surface/tests.rs`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::Output;

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-mac-surface-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

#[test]
fn mac_surface_prints_the_paths_and_appends_the_answer_to_the_output() {
    // Catches: `--changed` or `--output` not reaching the verb, and an answer printed but not
    // recorded for the jobs that read it.
    let d = dir("true");
    std::fs::write(
        d.join("changed"),
        b"docs/x.md\0crates/tierstore/src/server/update_hook/launchd/mod.rs\0",
    )
    .expect("write");
    let o = run(
        &d,
        &[
            "mac-surface",
            "--changed",
            "changed",
            "--output",
            "github-output",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "on the macOS surface: crates/tierstore/src/server/update_hook/launchd/mod.rs\nmac=true\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("out"),
        "mac=true\n"
    );
}

#[test]
fn mac_surface_exits_1_for_a_list_it_cannot_read_and_2_without_one() {
    // Catches: a missing list (the `git diff` step wrote nothing) passing as `mac=false`, and a
    // call without `--changed` read as an empty list.
    let d = dir("errors");
    let o = run(&d, &["mac-surface", "--changed", "missing"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert_eq!(
        String::from_utf8_lossy(&o.stderr),
        "xtask: read missing: No such file or directory (os error 2)\n"
    );
    assert!(o.stdout.is_empty(), "{o:?}");
    let o = run(&d, &["mac-surface"]);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("--changed <CHANGED>"),
        "{o:?}"
    );
}
