use std::path::Path;

use super::*;
use crate::package::{Platform, asset_stem, tarball};
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-binary-unit"), name)
}

/// A binary whose `--version` output is its own content.
fn reads(path: &Path) -> Result<Said, Error> {
    Ok(Said {
        success: true,
        stdout: std::fs::read_to_string(path).expect("the binary"),
        stderr: String::new(),
    })
}

fn fails(_: &Path) -> Result<Said, Error> {
    Ok(Said {
        success: false,
        stdout: String::new(),
        stderr: "boom\n".to_owned(),
    })
}

/// `dir` holding the linux-x86_64 tarball of `COMMIT` whose binary prints `says`, and a
/// tarball for the other platform whose binary prints something else (it must never be run).
fn assets(dir: &Path, says: &str) {
    for platform in Platform::ALL {
        let text = if platform == Platform::LinuxX86_64 {
            says
        } else {
            "tierstore 9.9.9+ffffffffffff\n"
        };
        let tgz = tarball("tierstore", text.as_bytes()).expect("tarball");
        let stem = asset_stem("tierstore", COMMIT, platform);
        std::fs::write(dir.join(format!("{stem}.tar.gz")), tgz).expect("write");
    }
}

#[test]
fn a_binary_that_says_the_tag_and_the_commit_is_accepted() {
    // Catches: the check refusing the form `--version` prints, and a tag compared with its `v`.
    check_said("tierstore 0.3.2+0123456789ab\n", "0.3.2", COMMIT).expect("exact");
    check_said("tierstore 0.3.2+0123456789ab", "0.3.2", COMMIT).expect("no newline");
    check_said("tierstore 10.20.30+0123456789ab\n", "10.20.30", COMMIT).expect("numbers");
}

#[test]
fn a_binary_that_says_another_version_or_commit_is_refused_with_both_in_the_words() {
    // Catches: the incident this check exists for (the crate's old version under a new tag),
    // a version that merely starts like the tag, another commit, none, and any other shape.
    for (said, names) in [
        ("tierstore 0.3.0+0123456789ab\n", "0.3.0"),
        ("tierstore 0.3.20+0123456789ab\n", "0.3.20"),
        ("tierstore 0.3.2+e7fd86844000\n", "e7fd86844000"),
        ("tierstore 0.3.2+unknown\n", "unknown"),
        ("tierstore 0.3.2\n", "tierstore 0.3.2"),
        ("tierstore 0.3.2+0123456789ab extra\n", "extra"),
        ("other 0.3.2+0123456789ab\n", "other"),
        ("", "\"\""),
    ] {
        let e = check_said(said, "0.3.2", COMMIT).expect_err(said);
        let words = e.to_string();
        assert!(words.contains(names), "{said:?}: {words}");
        assert!(
            words.contains("tierstore 0.3.2+0123456789ab"),
            "the words must give what is expected: {words}"
        );
    }
}

#[test]
fn the_x86_64_tarball_of_the_commit_is_unpacked_run_and_checked() {
    // Catches: the wrong platform's binary being run, a binary that is not unpacked from the
    // tarball (the dir holds only tarballs), and the good case being refused.
    let dir = scratch("good");
    let work = scratch("good-work");
    assets(&dir, "tierstore 0.3.2+0123456789ab\n");
    let printed = check_binary(&dir, "v0.3.2", COMMIT, &work, reads).expect("check");
    assert!(
        printed
            .iter()
            .any(|l| l.contains("v0.3.2") && l.contains("0123456789ab")),
        "{printed:?}"
    );
}

#[test]
fn a_binary_under_the_wrong_tag_or_commit_is_refused() {
    // Catches: a release published with a binary that reports another version than its tag
    // (the v0.3.1 case), or another commit.
    for (says, tag) in [
        ("tierstore 0.3.0+0123456789ab\n", "v0.3.1"),
        ("tierstore 0.3.1+0123456789ab\n", "v0.3.2"),
        ("tierstore 0.3.2+0123456789ac\n", "v0.3.2"),
    ] {
        let dir = scratch("bad");
        let work = scratch("bad-work");
        assets(&dir, says);
        let e = check_binary(&dir, tag, COMMIT, &work, reads).expect_err(says);
        assert!(matches!(e, Error::Failed(_)), "{e:?}");
    }
}

#[test]
fn a_missing_tarball_a_tarball_without_the_binary_and_a_failing_binary_are_errors() {
    // Catches: a check that passes when it could not look.
    let work = scratch("err-work");
    let empty = scratch("err-empty");
    assert!(check_binary(&empty, "v0.3.2", COMMIT, &work, reads).is_err());
    let dir = scratch("err-member");
    let tgz = tarball("not-tierstore", b"x").expect("tarball");
    let stem = asset_stem("tierstore", COMMIT, Platform::LinuxX86_64);
    std::fs::write(dir.join(format!("{stem}.tar.gz")), tgz).expect("write");
    assert!(check_binary(&dir, "v0.3.2", COMMIT, &work, reads).is_err());
    let dir = scratch("err-fails");
    assets(&dir, "tierstore 0.3.2+0123456789ab\n");
    let e = check_binary(&dir, "v0.3.2", COMMIT, &work, fails).expect_err("fails");
    assert!(e.to_string().contains("boom"), "{e}");
}

#[test]
fn a_bad_tag_or_commit_is_a_usage_error() {
    // Catches: flags that reach the file names unchecked.
    let dir = scratch("usage");
    let work = scratch("usage-work");
    for tag in ["0.3.2", "v0.3", "v0.3.2-rc1", ""] {
        let e = check_binary(&dir, tag, COMMIT, &work, reads).expect_err(tag);
        assert!(matches!(e, Error::Usage(_)), "{tag}: {e:?}");
    }
    let e = check_binary(&dir, "v0.3.2", "0123456789ab", &work, reads).expect_err("short");
    assert!(matches!(e, Error::Usage(_)), "{e:?}");
}

#[test]
fn a_built_binary_is_checked_against_a_version_and_a_commit() {
    // Catches: the build step of the artifacts workflow not seeing a binary that was built
    // without the version it was given.
    let dir = scratch("built");
    let path = dir.join("tierstore");
    std::fs::write(&path, "tierstore 0.3.2+0123456789ab\n").expect("write");
    let line = check_built(&path, "0.3.2", COMMIT, reads).expect("built");
    assert!(
        line.contains("0.3.2") && line.contains("0123456789ab"),
        "{line}"
    );
    std::fs::write(&path, "tierstore 0.3.0+0123456789ab\n").expect("write");
    assert!(check_built(&path, "0.3.2", COMMIT, reads).is_err());
    let e = check_built(&path, "0.3.0", COMMIT, fails).expect_err("fails");
    assert!(e.to_string().contains("boom"), "{e}");
    assert!(matches!(
        check_built(&path, "0.3.2", "nonsense", reads),
        Err(Error::NotACommit(_))
    ));
}
