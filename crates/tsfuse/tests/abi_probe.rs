//! Proves the layout probe itself: `abi/probe.c`, compiled with the system C compiler (`cc`, as
//! its header says) and run on the pinned header, prints exactly `abi/layout.txt`, the output the
//! layout test compares the Rust structs with. Without this, a probe that printed something else
//! would leave a stale `layout.txt` unchallenged.
//!
//! Catches:
//! - a probe whose output differs from the checked-in `layout.txt` in any byte (a changed field
//!   list, format, checksum or version);
//! - a probe that no longer compiles warning-free under `-std=c11 -Wall -Wextra -Werror`;
//! - a probe run without a header path, or on a path it cannot read, that prints a layout or
//!   exits 0 instead of failing with its usage (exit 2) or the read error (exit 1).
//!
//! This test needs a C compiler named `cc` on `PATH` (CI's runners and the farm's build image
//! have gcc); without one it fails, naming the missing compiler.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tierstore_testkit::dir::TestDir;

fn abi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("abi")
}

/// `probe.c` compiled into `dir`.
fn build_probe(dir: &Path) -> PathBuf {
    let exe = dir.join("tsfuse-abi-probe");
    let out = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&exe)
        .arg(abi_dir().join("probe.c"))
        .output()
        .unwrap_or_else(|e| panic!("run the C compiler `cc` (this test needs one): {e}"));
    assert!(
        out.status.success(),
        "cc failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

fn run(exe: &Path, args: &[&Path]) -> Output {
    Command::new(exe).args(args).output().unwrap()
}

#[test]
fn the_probe_prints_exactly_the_checked_in_layout() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tsfuse-abi-probe");
    let exe = build_probe(dir.path());

    let out = run(&exe, &[&abi_dir().join("fuse.h")]);
    assert!(out.status.success(), "probe failed: {}", out.status);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let layout = std::fs::read_to_string(abi_dir().join("layout.txt")).unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), layout);

    // Without a header path: its usage on stderr, nothing on stdout, exit 2.
    let out = run(&exe, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let usage = String::from_utf8(out.stderr).unwrap();
    assert!(
        usage.starts_with("usage: ") && usage.ends_with(" <path of fuse.h>\n"),
        "{usage:?}"
    );

    // An unreadable header: the error on stderr, no layout, exit 1.
    let missing = dir.path().join("no-such-fuse.h");
    let out = run(&exe, &[&missing]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        err,
        format!("{}: No such file or directory\n", missing.display())
    );

    // Another header file: a different checksum line, so the layout test refuses it.
    let other = dir.path().join("other-fuse.h");
    std::fs::write(&other, "not the pinned header\n").unwrap();
    let out = run(&exe, &[&other]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_ne!(text.lines().next(), layout.lines().next());
    assert_eq!(
        text.lines().skip(1).collect::<Vec<_>>(),
        layout.lines().skip(1).collect::<Vec<_>>()
    );
}
