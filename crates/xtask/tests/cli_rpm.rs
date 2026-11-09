//! The rpm verbs of `channels` end to end: each is run as a process, and its exit status and words
//! are read. These hold the dispatch of the verbs, the flags clap checks, and their statuses (0, 1
//! for a failure, 2 for a usage error); the logic of each is tested beside it. `aws`, `gpg` and
//! `docker` are the stand-in program (src/bin/gh-fake.rs), which answers none of them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");
const FPR: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

/// An empty directory for the test `name`, whatever an earlier run left in it.
fn fresh(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-rpm-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .current_dir(cwd)
        .args(["channels"])
        .args(args)
        .output()
        .expect("run xtask")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn the_rpm_verbs_are_dispatched_and_report_the_first_thing_that_fails() {
    // Catches: a verb of the rpm channel that is not wired to its implementation, a failure that
    // does not reach the exit status, and a signing failure that leaves the private key behind.
    let d = fresh("verbs");
    let o = run(
        &d,
        &[
            "rpm",
            "repo",
            "read",
            "--bucket",
            "rpm-bucket",
            "--out",
            "current",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("get-object rpm/stable/aarch64/repodata/repomd.xml failed"),
        "{}",
        stderr(&o)
    );
    let o = run(
        &d,
        &[
            "rpm",
            "repo",
            "index",
            "--current",
            "current",
            "--out",
            "next",
            "--rpms",
            "signed",
            "--version",
            "1.2.3",
            "--date",
            "5",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("etags.txt is missing: run `rpm repo read` first"),
        "{}",
        stderr(&o)
    );

    std::fs::create_dir_all(d.join("next/stable/aarch64/repodata")).unwrap();
    std::fs::create_dir_all(d.join("next/stable/x86_64/repodata")).unwrap();
    std::fs::write(d.join("next/stable/aarch64/repodata/repomd.xml"), "x").unwrap();
    std::fs::write(d.join("next/stable/x86_64/repodata/repomd.xml"), "x").unwrap();
    std::fs::write(d.join("key.asc"), "key").unwrap();
    std::fs::write(d.join("pass"), "pass").unwrap();
    let sign = |fingerprint: &str| {
        run(
            &d,
            &[
                "rpm",
                "repo",
                "sign",
                "--dir",
                "next/stable",
                "--work",
                "work",
                "--fingerprint",
                fingerprint,
                "--key-file",
                "key.asc",
                "--passphrase-file",
                "pass",
                "--gpg",
                FAKE,
                "--output",
                "out",
            ],
        )
    };
    let o = sign(FPR);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("gpg could not import the key"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("key.asc").exists() && !d.join("pass").exists() && !d.join("out").exists());
    assert_eq!(sign("nonsense").status.code(), Some(2));

    std::fs::write(
        d.join("SHA256SUMS"),
        format!("{}  tierstore-1.2.3-1.aarch64.rpm\n", "0".repeat(64)),
    )
    .unwrap();
    let o = run(
        &d,
        &[
            "rpm",
            "check-signed",
            "--dir",
            "signed",
            "--version",
            "1.2.3",
            "--sums",
            "SHA256SUMS",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("is not in"), "{}", stderr(&o));
    std::fs::write(d.join("key.asc"), "key").unwrap();
    std::fs::write(d.join("pass"), "pass").unwrap();
    let o = run(
        &d,
        &[
            "rpm",
            "sign",
            "--rpms",
            "dist",
            "--out",
            "signed",
            "--version",
            "1.2.3",
            "--sums",
            "SHA256SUMS",
            "--work",
            "work",
            "--fingerprint",
            FPR,
            "--key-file",
            "key.asc",
            "--passphrase-file",
            "pass",
            "--signed-at",
            "5",
            "--gpg",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("is not in"), "{}", stderr(&o));
    assert_eq!(
        run(
            &d,
            &[
                "rpm",
                "sign",
                "--rpms",
                "dist",
                "--out",
                "signed",
                "--version",
                "1.2.3",
                "--sums",
                "SHA256SUMS",
                "--work",
                "work",
                "--fingerprint",
                FPR,
                "--key-file",
                "key.asc",
                "--passphrase-file",
                "pass",
                "--signed-at",
                "soon",
            ],
        )
        .status
        .code(),
        Some(2)
    );
    let o = run(
        &d,
        &[
            "rpm",
            "promote",
            "--testing",
            "t",
            "--stable",
            "s",
            "--out",
            "o",
            "--version",
            "1.2.3",
            "--sums",
            "SHA256SUMS",
            "--signed-sums",
            "SHA256SUMS.signed",
            "--date",
            "5",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("etags.txt is missing: run `rpm repo read --suite stable` first"),
        "{}",
        stderr(&o)
    );
    let write = |extra: &[&str]| {
        let mut args = vec![
            "rpm",
            "repo",
            "write",
            "--bucket",
            "rpm-bucket",
            "--distribution-id",
            "E1",
            "--hostname",
            "packages.example",
            "--next",
            "next",
            "--aws",
            FAKE,
            "--curl",
            FAKE,
        ];
        args.extend(extra);
        run(&d, &args)
    };
    let o = write(&["--no-pool"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("is missing from the new repodata"),
        "{}",
        stderr(&o)
    );
    assert_eq!(
        write(&["--no-pool", "--version", "1.2.3"]).status.code(),
        Some(2)
    );
    assert_eq!(write(&[]).status.code(), Some(2));
}

#[test]
fn check_rpm_is_dispatched_and_reports_the_first_program_that_fails() {
    // Catches: a verb not wired to its implementation, and a docker that fails taken for a pass.
    let d = fresh("check-rpm");
    let args = |arch: &'static str| {
        vec![
            "check-rpm",
            "--arch",
            arch,
            "--version",
            "1.2.3",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--container",
            "c",
            "--docker",
            FAKE,
        ]
    };
    let o = run(&d, &args("x86_64"));
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("uname -m failed"), "{}", stderr(&o));
    assert_eq!(run(&d, &args("amd64")).status.code(), Some(2));
}
