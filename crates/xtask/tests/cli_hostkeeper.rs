//! The hostkeeper verbs of the `xtask` program end to end (`release hostkeeper-index`,
//! `verify-assets hostkeeper-package` and `verify-assets hostkeeper-index`, with
//! `release hostkeeper-package` and `sums assemble` around them): each is run as a process, and
//! its exit status and words are read. These hold the dispatch of the verbs and their statuses
//! (0, 1 for a failure, 2 for a usage error); the logic of each is tested beside it.
//!
//! hostkeeper's unpacker leaves an unpacked version read-only (0555), so every test opens its
//! directory up before it is removed, at its start and at its end.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const VERSION: &str = "0.4.0+0123456789ab";
const ALL: [&str; 3] = ["linux-x86_64", "linux-arm64", "macos-arm64"];
const LINUX: [&str; 1] = ["linux-x86_64"];

/// Gives `path` and every directory below it mode 0700.
fn open_up(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            open_up(&entry.path());
        }
    }
}

/// An empty directory for the test `name`, whatever an earlier run left in it.
fn fresh(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-hostkeeper-{name}"));
    open_up(&dir);
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

/// Writes the stand-in binary in `d` and the package of each of `platforms` into `d/out`.
fn packages(d: &Path, platforms: &[&str]) {
    std::fs::write(
        d.join("tierstore"),
        format!("stand-in binary of {VERSION} and more"),
    )
    .expect("binary");
    for platform in platforms {
        let o = run(
            d,
            &[
                "release",
                "hostkeeper-package",
                "--binary",
                "tierstore",
                "--version",
                VERSION,
                "--min-hostkeeper",
                "0.1.0",
                "--platform",
                platform,
                "--out",
                "out",
            ],
        );
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    }
}

#[test]
fn the_packages_index_sums_and_unpack_chain_of_a_push() {
    // Catches: the flags of the three hostkeeper verbs and `sums assemble` reaching the wrong
    // arguments, so the chain a release runs breaks: the index and its sums part not written,
    // the package version not appended, the sums not listing the packages and the index, or the
    // unpack check failing a good set.
    let d = fresh("chain");
    packages(&d, &ALL);
    let o = run(
        &d,
        &[
            "release",
            "hostkeeper-index",
            "out",
            "--commit",
            COMMIT,
            "--event",
            "push",
            "--output",
            "out.txt",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(d.join("out/index.json").is_file());
    assert!(d.join("out/SHA256SUMS.index").is_file());
    assert_eq!(
        std::fs::read_to_string(d.join("out.txt")).expect("output"),
        format!("package-version={VERSION}\n")
    );

    let o = run(
        &d,
        &[
            "sums",
            "assemble",
            "out",
            "--summary",
            "summary.md",
            "--digest-output",
            "digest.txt",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let sums = std::fs::read_to_string(d.join("out/SHA256SUMS")).expect("sums");
    for platform in ALL {
        assert!(
            sums.contains(&format!("  tierstore-{VERSION}-{platform}.tar.zst\n")),
            "{sums}"
        );
    }
    assert!(sums.contains("  index.json\n"), "{sums}");

    let o = run(
        &d,
        &[
            "verify-assets",
            "hostkeeper-index",
            "out",
            "--commit",
            COMMIT,
            "--scratch",
            d.join("unpacked").to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    for platform in ALL {
        assert!(
            d.join("unpacked")
                .join(platform)
                .join("packages/tierstore")
                .join(VERSION)
                .is_dir(),
            "{platform}"
        );
    }
    open_up(&d);
}

#[test]
fn the_index_of_a_pull_request_needs_the_linux_x86_64_package_and_writes_nothing() {
    // Catches: a pull request writing an index of one platform, a push accepting a set without
    // the macOS package, and a bad --commit not reported as a usage error.
    let d = fresh("pull-request");
    packages(&d, &LINUX);
    let index = |event: &'static str, commit: &'static str| {
        run(
            &d,
            &[
                "release",
                "hostkeeper-index",
                "out",
                "--commit",
                commit,
                "--event",
                event,
            ],
        )
    };
    let o = index("pull_request", COMMIT);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!d.join("out/index.json").exists());
    assert!(!d.join("out/SHA256SUMS.index").exists());

    let o = index("push", COMMIT);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("macos-arm64"), "{}", stderr(&o));
    assert!(!d.join("out/index.json").exists());

    let o = index("push", "0123456789ab");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("--commit"), "{}", stderr(&o));
}

#[test]
fn verify_assets_hostkeeper_package_unpacks_the_package_and_refuses_what_it_cannot() {
    // Catches: the flags of the verb not reaching the check (the package is not unpacked where
    // --scratch says), a malformed --version not a usage error, and a missing package not a
    // failure.
    let d = fresh("package");
    packages(&d, &ALL);
    let scratch = d.join("mac");
    let verify = |version: &'static str, platform: &'static str| {
        run(
            &d,
            &[
                "verify-assets",
                "hostkeeper-package",
                "out",
                "--version",
                version,
                "--platform",
                platform,
                "--scratch",
                scratch.to_str().expect("utf-8 path"),
            ],
        )
    };
    let o = verify(VERSION, "macos-arm64");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let unpacked = scratch.join("packages/tierstore").join(VERSION);
    assert!(unpacked.is_dir(), "{}", stdout(&o));
    assert!(
        stdout(&o).contains(&unpacked.display().to_string()),
        "{}",
        stdout(&o)
    );

    let o = verify("0.4.0", "macos-arm64");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("--version \"0.4.0\" is not"),
        "{}",
        stderr(&o)
    );

    let o = verify("0.5.0+0123456789ab", "macos-arm64");
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    open_up(&d);
}

#[test]
fn verify_assets_hostkeeper_index_without_an_index_is_a_failure() {
    // Catches: a directory of packages with no index.json passing the check.
    let d = fresh("no-index");
    packages(&d, &ALL);
    let o = run(
        &d,
        &[
            "verify-assets",
            "hostkeeper-index",
            "out",
            "--commit",
            COMMIT,
            "--scratch",
            d.join("unpacked").to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("index.json"), "{}", stderr(&o));
    open_up(&d);
}
