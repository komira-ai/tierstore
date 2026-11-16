//! The macOS installer pkg verbs of `release` end to end: each is run as a process, and its exit
//! status and words are read. These hold the dispatch of the verbs, the flags clap checks and their
//! statuses (0, 1 for a failure, 2 for a usage error); the logic of each is tested beside it.
//! `pkgbuild`, `productbuild` and `pkgutil` are found on `PATH`; here that is a directory of
//! links to `true` and `false`, which every system has.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// An empty directory for the test `name`, whatever an earlier run left in it.
fn fresh(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-macos-pkg-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// A directory of the three tools, each a link to `program` (found in `/usr/bin` or `/bin`).
fn tools(dir: &Path, program: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let target = ["/usr/bin", "/bin"]
        .map(|d| Path::new(d).join(program))
        .into_iter()
        .find(|p| p.exists())
        .expect("the program");
    for name in ["pkgbuild", "productbuild", "pkgutil"] {
        std::os::unix::fs::symlink(&target, bin.join(name)).expect("link");
    }
    bin
}

fn run(path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .env("PATH", path)
        .args(["release"])
        .args(args)
        .output()
        .expect("run xtask")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn macos_pkg_runs_both_tools_and_names_the_product_it_did_not_get() {
    // Catches: a verb not wired to the builder, a builder that never runs pkgbuild and
    // productbuild, and a failure that does not reach the exit status. The tools here succeed
    // and write nothing, so the product is missing.
    let d = fresh("build");
    let bin = tools(&d, "true");
    std::fs::write(d.join("tierstore"), b"binary").expect("binary");
    let (binary, stage, out) = (d.join("tierstore"), d.join("stage"), d.join("out"));
    let (binary, stage_arg, out) = (
        binary.to_str().unwrap(),
        stage.to_str().unwrap(),
        out.to_str().unwrap(),
    );
    let args = |version| {
        [
            "macos-pkg",
            "--version",
            version,
            "--binary",
            binary,
            "--stage",
            stage_arg,
            "--out",
            out,
        ]
    };
    let o = run(&bin, &args("1.2.3"));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("tierstore-1.2.3-macos-arm64.pkg"),
        "{}",
        stderr(&o)
    );
    assert!(stage.join("root").is_dir());
    let o = run(&bin, &args("1.2"));
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("is not <major>.<minor>.<patch>"));
}

#[test]
fn check_macos_pkg_refuses_a_directory_that_is_not_the_expanded_product() {
    // Catches: a check verb that is not wired, or one that passes an empty directory.
    let d = fresh("check");
    std::fs::write(d.join("tierstore"), b"binary").expect("binary");
    std::fs::create_dir(d.join("expanded")).expect("expanded");
    let o = run(
        Path::new("/usr/bin:/bin"),
        &[
            "check-macos-pkg",
            d.join("expanded").to_str().unwrap(),
            "--version",
            "1.2.3",
            "--binary",
            d.join("tierstore").to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o).trim_end(),
        "xtask: missing Distribution\nmissing tierstore.pkg"
    );
}

#[test]
fn check_pkg_signature_runs_pkgutil_and_refuses_what_it_does_not_report() {
    // Catches: a signature verb that is not wired, one that passes a pkg pkgutil said nothing
    // of, and an identity that is not a Developer ID Installer one reaching pkgutil.
    let d = fresh("signature");
    let bin = tools(&d, "true");
    let identity = "Developer ID Installer: Example Org (TEAM000000)";
    let o = run(
        &bin,
        &["check-pkg-signature", "x.pkg", "--identity", identity],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("pkgutil printed 0 status lines, not one"));
    let o = run(
        &bin,
        &["check-pkg-signature", "x.pkg", "--identity", "Someone"],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("is not a Developer ID Installer identity"));
}
