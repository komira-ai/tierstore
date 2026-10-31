//! The binary end to end: a workspace directory with crates, an lcov tracefile, and the
//! exit status and output it gives.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A workspace under the target directory with the crates `names`, removed first if a
/// run left it.
fn workspace(name: &str, names: &[&str]) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("coverage-gate-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    for krate in names {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(dir.join("src")).expect("crate dir");
        std::fs::write(dir.join("Cargo.toml"), b"").expect("manifest");
    }
    root
}

/// A tracefile with one source per `(path, lf, lh, brf, brh)`.
fn lcov(root: &Path, files: &[(&str, u64, u64, u64, u64)]) -> PathBuf {
    let mut text = String::new();
    for (path, lf, lh, brf, brh) in files {
        text.push_str(&format!(
            "SF:{}\nLF:{lf}\nLH:{lh}\nBRF:{brf}\nBRH:{brh}\nend_of_record\n",
            root.join(path).display()
        ));
    }
    let file = root.join("lcov.info");
    std::fs::write(&file, text).expect("lcov");
    file
}

fn gate(root: &Path, lcov: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coverage-gate"))
        .arg("--lcov")
        .arg(lcov)
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("run coverage-gate")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn crates_at_99_percent_pass_and_the_table_is_printed() {
    // Catches: a gate that fails a good workspace, or prints nothing.
    let root = workspace("pass", &["a", "b"]);
    let tracefile = lcov(
        &root,
        &[
            ("crates/a/src/lib.rs", 100, 99, 10, 10),
            ("crates/b/src/lib.rs", 4, 4, 0, 0),
        ],
    );
    let o = gate(&root, &tracefile, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("coverage gate: PASS"), "{}", out(&o));
}

#[test]
fn a_crate_under_99_percent_fails_with_status_1() {
    // Catches: the threshold not enforced, or enforced with another exit status.
    let root = workspace("fail", &["a"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 100, 98, 0, 0)]);
    let o = gate(&root, &tracefile, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("BELOW THRESHOLD (lines)"), "{}", out(&o));
}

#[test]
fn an_excluded_crate_is_not_judged_and_a_missing_one_fails() {
    // Catches: `--exclude` ignored, and an exclusion that outlives its crate.
    let root = workspace("exclude", &["a", "tool"]);
    let tracefile = lcov(
        &root,
        &[
            ("crates/a/src/lib.rs", 10, 10, 0, 0),
            ("crates/tool/src/lib.rs", 10, 1, 0, 0),
        ],
    );
    let o = gate(&root, &tracefile, &["--exclude", "tool"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("excluded"), "{}", out(&o));
    let o = gate(
        &root,
        &tracefile,
        &["--exclude", "tool", "--exclude", "gone"],
    );
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(
        out(&o).contains("EXCLUDED BUT NOT IN WORKSPACE"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_summary_file_gets_the_table_appended() {
    // Catches: the job summary overwritten or not written.
    let root = workspace("summary", &["a"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 10, 10, 0, 0)]);
    let summary = root.join("summary.md");
    std::fs::write(&summary, "# earlier\n").expect("summary");
    let o = gate(
        &root,
        &tracefile,
        &["--summary-file", summary.to_str().expect("utf8")],
    );
    assert_eq!(o.status.code(), Some(0));
    let text = std::fs::read_to_string(&summary).expect("read");
    assert!(text.starts_with("# earlier\n| crate |"), "{text}");
}

#[test]
fn unreadable_input_is_status_2() {
    // Catches: a missing or damaged tracefile read as a pass or as a gate failure.
    let root = workspace("input", &["a"]);
    let o = gate(&root, &root.join("absent.info"), &[]);
    assert_eq!(o.status.code(), Some(2));
    let empty = root.join("empty.info");
    std::fs::write(&empty, "").expect("empty");
    let o = gate(&root, &empty, &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no records"));
    let stray = lcov(&root, &[("elsewhere/src/lib.rs", 1, 1, 0, 0)]);
    let o = gate(&root, &stray, &[]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn a_crate_with_no_instrumented_line_and_one_under_on_branches_are_named() {
    // Catches: a crate that never ran passing for lack of data, and a branch shortfall
    // reported under the wrong metric.
    let root = workspace("nodata", &["a", "b"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 10, 10, 100, 90)]);
    let o = gate(&root, &tracefile, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("NOTHING MEASURED"), "{}", out(&o));
    assert!(
        out(&o).contains("BELOW THRESHOLD (branches)"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_relative_root_is_resolved_from_the_working_directory() {
    // Catches: a relative `--root` compared as written with the tracefile's absolute paths.
    let root = workspace("relative", &["a"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 10, 10, 0, 0)]);
    let o = Command::new(env!("CARGO_BIN_EXE_coverage-gate"))
        .current_dir(&root)
        .args(["--lcov"])
        .arg(&tracefile)
        .args(["--root", "."])
        .output()
        .expect("run coverage-gate");
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("| a | 10/10 |"), "{}", out(&o));
    assert!(out(&o).contains("coverage gate: PASS"), "{}", out(&o));
}

#[test]
fn a_root_that_is_not_a_workspace_is_status_2() {
    // Catches: a workspace that cannot be listed read as an empty one, which passes.
    let root = workspace("no-crates", &["a"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 10, 10, 0, 0)]);
    let o = gate(&root.join("absent"), &tracefile, &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("list crates under"));
}

#[test]
fn a_summary_file_that_cannot_be_written_is_status_2() {
    // Catches: a failed summary write ignored, so the job summary silently goes missing.
    let root = workspace("summary-dir", &["a"]);
    let tracefile = lcov(&root, &[("crates/a/src/lib.rs", 10, 10, 0, 0)]);
    let o = gate(
        &root,
        &tracefile,
        &["--summary-file", root.to_str().expect("utf8")],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("append to"));
}
