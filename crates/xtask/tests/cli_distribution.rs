//! The distribution verbs of the `xtask` program end to end (`release tap-tarball` and `release
//! deb`, with `package` before them and `sums assemble` and `verify-assets sums` after them, as a
//! build job and the sums job of `artifacts.yml` run them): each is run as a process, and its exit
//! status and words are read. These hold the dispatch of the verbs and their statuses (0, 1 for a
//! failure, 2 for a usage error); the logic of each is tested beside it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// An empty directory for the test `name`, whatever an earlier run left in it.
fn fresh(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-distribution-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The stand-in binary, SBOM and notice in `d`.
fn inputs(d: &Path) {
    std::fs::write(d.join("tierstore"), b"stand-in binary").expect("binary");
    std::fs::write(d.join("sbom.json"), b"{}").expect("sbom");
    std::fs::write(d.join("notice.txt"), b"licences\n").expect("notice");
}

/// `package` for `platform` into `d/dist`.
fn package(d: &Path, platform: &str) {
    let o = run(
        d,
        &[
            "package",
            "--binary",
            "tierstore",
            "--sbom",
            "sbom.json",
            "--licenses",
            "notice.txt",
            "--platform",
            platform,
            "--commit",
            COMMIT,
            "--out",
            "dist",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
}

#[test]
fn the_build_jobs_chain_of_a_linux_platform_gives_sums_that_verify() {
    // Catches: a verb that is not wired to the command line, or whose files the sums job of
    // artifacts.yml would refuse (a line missing from SHA256SUMS.<platform>, a file it does not
    // list).
    let d = fresh("chain");
    inputs(&d);
    for platform in ["linux-x86_64", "linux-arm64"] {
        package(&d, platform);
        let o = run(
            &d,
            &[
                "release",
                "tap-tarball",
                "dist",
                "--commit",
                COMMIT,
                "--version",
                "0.4.0",
                "--platform",
                platform,
            ],
        );
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(stdout(&o).contains("a copy of"), "{}", stdout(&o));
        let o = run(
            &d,
            &[
                "release",
                "deb",
                "--binary",
                "tierstore",
                "--version",
                "0.4.0",
                "--platform",
                platform,
                "--licenses",
                "notice.txt",
                "--out",
                "dist",
            ],
        );
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(stdout(&o).starts_with("tierstore_0.4.0_"), "{}", stdout(&o));
        let o = run(
            &d,
            &[
                "release",
                "rpm",
                "--binary",
                "tierstore",
                "--version",
                "0.4.0",
                "--platform",
                platform,
                "--licenses",
                "notice.txt",
                "--out",
                "dist",
            ],
        );
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(
            stdout(&o).starts_with("tierstore-0.4.0-1."),
            "{}",
            stdout(&o)
        );
    }
    let o = run(
        &d,
        &["release", "rpm-info", "dist/tierstore-0.4.0-1.aarch64.rpm"],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("Architecture: aarch64"),
        "{}",
        stdout(&o)
    );
    assert!(
        stdout(&o).contains("-0640 root:root /etc/tierstore/tierstore.toml %config(noreplace)"),
        "{}",
        stdout(&o)
    );
    let o = run(&d, &["sums", "assemble", "dist"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let sums = stdout(&o);
    for name in [
        "tierstore-0.4.0-1.aarch64.rpm",
        "tierstore-0.4.0-1.x86_64.rpm",
        "tierstore-0.4.0-linux-aarch64.tar.gz",
        "tierstore-0.4.0-linux-x86_64.tar.gz",
        "tierstore_0.4.0_amd64.deb",
        "tierstore_0.4.0_arm64.deb",
    ] {
        assert!(sums.contains(&format!("  {name}")), "{name}: {sums}");
    }
    let o = run(&d, &["verify-assets", "sums", "dist"]);
    assert!(o.status.success(), "{}", stderr(&o));
}

#[test]
fn the_macos_platform_has_a_tap_tarball_and_no_debian_or_rpm_package() {
    // Catches: a Debian or an RPM package built for macOS, and a macOS tap tarball that is not made.
    let d = fresh("macos");
    inputs(&d);
    package(&d, "macos-arm64");
    let o = run(
        &d,
        &[
            "release",
            "tap-tarball",
            "dist",
            "--commit",
            COMMIT,
            "--version",
            "0.4.0",
            "--platform",
            "macos-arm64",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(d.join("dist/tierstore-0.4.0-macos-arm64.tar.gz").is_file());
    let o = run(
        &d,
        &[
            "release",
            "deb",
            "--binary",
            "tierstore",
            "--version",
            "0.4.0",
            "--platform",
            "macos-arm64",
            "--licenses",
            "notice.txt",
            "--out",
            "dist",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("only the Linux platforms have a Debian package"),
        "{}",
        stderr(&o)
    );
    let o = run(
        &d,
        &[
            "release",
            "rpm",
            "--binary",
            "tierstore",
            "--version",
            "0.4.0",
            "--platform",
            "macos-arm64",
            "--licenses",
            "notice.txt",
            "--out",
            "dist",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("only the Linux platforms have an RPM package"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_version_with_a_commit_is_a_usage_error_and_a_missing_tarball_a_failure() {
    // Catches: the statuses of the two verbs: 2 for flags that do not fit, 1 for a file that is
    // not there.
    let d = fresh("statuses");
    inputs(&d);
    let o = run(
        &d,
        &[
            "release",
            "tap-tarball",
            "dist",
            "--commit",
            COMMIT,
            "--version",
            "0.4.0+0123456789ab",
            "--platform",
            "linux-x86_64",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let o = run(
        &d,
        &[
            "release",
            "tap-tarball",
            "dist",
            "--commit",
            COMMIT,
            "--version",
            "0.4.0",
            "--platform",
            "linux-x86_64",
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let o = run(
        &d,
        &[
            "release",
            "deb",
            "--binary",
            "tierstore",
            "--version",
            "0.4",
            "--platform",
            "linux-x86_64",
            "--licenses",
            "notice.txt",
            "--out",
            "dist",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
}

/// The 64-byte header of a 64-bit little-endian ELF file with no section: a binary that needs
/// no glibc.
fn bare_elf() -> Vec<u8> {
    let mut header = b"\x7fELF\x02\x01\x01".to_vec();
    header.resize(64, 0);
    header
}

#[test]
fn check_glibc_reads_the_binary_against_the_limit_and_refuses_what_it_cannot_read() {
    // Catches: the verb not reaching the check (the flag `--max` lost, the binary argument
    // taken for something else), a file that is not an ELF binary passing, and a limit out of
    // its form being a failure (1) and not a usage error (2).
    let d = fresh("check-glibc");
    std::fs::write(d.join("static"), bare_elf()).expect("binary");
    std::fs::write(d.join("script"), b"#!/bin/sh\n").expect("script");
    let o = run(&d, &["release", "check-glibc", "static", "--max", "2.28"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o).trim_end(),
        "release check-glibc: static needs no glibc symbol version; the limit is GLIBC_2.28"
    );
    let o = run(&d, &["release", "check-glibc", "script", "--max", "2.28"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stderr(&o).trim_end(), "xtask: script: not an ELF file");
    let o = run(&d, &["release", "check-glibc", "static", "--max", "2"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("--max \"2\" is not <major>.<minor>"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn fetch_tree_without_a_tar_format_is_a_usage_error() {
    // Catches: `--tree` reaching the download with nothing to unpack it with, instead of
    // being refused before any connection.
    let d = fresh("fetch-tree");
    let o = run(
        &d,
        &[
            "fetch",
            "--url",
            "https://example.invalid/zig.tar.xz",
            "--sha256",
            &"0".repeat(64),
            "--out",
            "zig",
            "--tree",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("--tree is for a tar: it needs --extract tar.gz or tar.xz"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("zig").exists());
}
