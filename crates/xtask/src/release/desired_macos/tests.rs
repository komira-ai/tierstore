use std::cell::RefCell;

use super::*;
use crate::sums::{Line, render};
use crate::testdir::TestDir;

const REPO: &str = "example/tierstore";
const TAG: &str = "v0.4.0";
const VERSION: &str = "0.4.0";
const FLOOR: &str = "0.1.225";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside
            .parent()
            .expect("deps")
            .join("xtask-desired-macos-unit"),
        name,
    )
}

fn pkg_bytes() -> Vec<u8> {
    b"bytes of the signed pkg".to_vec()
}

/// `gh release download` that writes `pkg` (when there is one) as the pattern it is given.
struct FakeGh {
    pkg: Option<Vec<u8>>,
    ran: RefCell<Vec<Vec<String>>>,
}

impl Gh for FakeGh {
    fn api(&self, _path: &str) -> Result<Option<Value>, Error> {
        panic!("the entry reads nothing through the API");
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        self.ran.borrow_mut().push(args.to_vec());
        if let Some(bytes) = &self.pkg {
            std::fs::write(Path::new(&args[6]).join(&args[8]), bytes).expect("write");
        }
        Ok(String::new())
    }
}

/// A directory holding the `SHA256SUMS` of a release that lists `lines`.
fn with_sums(name: &str, lines: &[Line]) -> TestDir {
    let dir = scratch(name);
    std::fs::write(dir.join(SUMS), render(lines)).expect("sums");
    dir
}

fn listed(bytes: &[u8]) -> Line {
    Line {
        sha256: Sha256::of(bytes),
        name: macos_pkg_name(VERSION),
    }
}

fn gh_with(pkg: Option<Vec<u8>>) -> FakeGh {
    FakeGh {
        pkg,
        ran: RefCell::new(Vec::new()),
    }
}

#[test]
fn the_entry_is_the_named_version_form_with_the_digest_the_size_as_a_string_and_the_manifest() {
    // Asserts: the exact JSON of the macos-pkg entry: the installer, the repository the apt
    // entries name, the package version, the digest, the size as a string (an int64 in
    // hostkeeper's JSON) and the manifest of the package for the given floor.
    assert_eq!(
        entry(VERSION, &"ab".repeat(32), 1234, FLOOR),
        json!({
            "installer": "macos-pkg",
            "repository": "komira-testing",
            "packageVersion": "0.4.0",
            "digest": "ab".repeat(32),
            "bytes": "1234",
            "manifest": manifest_json(FLOOR),
        })
    );
}

#[test]
fn the_pkg_is_downloaded_from_the_release_and_its_digest_and_size_are_read_from_the_file() {
    // Asserts: gh downloads exactly the one pkg of the version from the release into the
    // directory, and the entry carries the SHA-256 and the byte count of what was downloaded.
    let dir = with_sums("entry", &[listed(&pkg_bytes())]);
    let gh = gh_with(Some(pkg_bytes()));
    let found = macos_entry(&gh, REPO, TAG, VERSION, &dir, FLOOR).expect("the entry");
    assert_eq!(
        found,
        entry(
            VERSION,
            &Sha256::of(&pkg_bytes()).to_string(),
            pkg_bytes().len() as u64,
            FLOOR
        )
    );
    let expected: Vec<String> = [
        "release",
        "download",
        TAG,
        "--repo",
        REPO,
        "--dir",
        &dir.display().to_string(),
        "--pattern",
        "tierstore-0.4.0-macos-arm64.pkg",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(gh.ran.borrow().as_slice(), [expected]);
}

#[test]
fn a_release_whose_sums_do_not_list_the_pkg_or_list_other_bytes_is_refused() {
    // Asserts: no listed pkg is refused before anything is downloaded; a downloaded file whose
    // digest is not the sums' is refused with both digests.
    let dir = with_sums(
        "unlisted",
        &[Line {
            sha256: Sha256::of(b"x"),
            name: "other".to_owned(),
        }],
    );
    let gh = gh_with(Some(pkg_bytes()));
    match macos_entry(&gh, REPO, TAG, VERSION, &dir, FLOOR) {
        Err(Error::Refused(lines)) => assert_eq!(
            lines,
            [
                "SHA256SUMS of release v0.4.0 lists no tierstore-0.4.0-macos-arm64.pkg: the release has no macOS installer"
            ]
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(gh.ran.borrow().is_empty());

    let dir = with_sums("changed", &[listed(b"the bytes that were attested")]);
    match macos_entry(&gh, REPO, TAG, VERSION, &dir, FLOOR) {
        Err(Error::Refused(lines)) => assert_eq!(
            lines,
            [format!(
                "tierstore-0.4.0-macos-arm64.pkg of release v0.4.0 has sha256 {}, but SHA256SUMS lists {}",
                Sha256::of(&pkg_bytes()),
                Sha256::of(b"the bytes that were attested")
            )]
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_pkg_that_was_not_downloaded_and_a_missing_sums_file_are_errors() {
    // Asserts: a download that leaves no file is an error naming the path it could not read, and
    // a directory with no SHA256SUMS is an error naming that file.
    let dir = with_sums("missing", &[listed(&pkg_bytes())]);
    let gh = gh_with(None);
    let path = dir.join("tierstore-0.4.0-macos-arm64.pkg");
    match macos_entry(&gh, REPO, TAG, VERSION, &dir, FLOOR) {
        Err(Error::Spawn { what, .. }) => assert_eq!(what, format!("read {}", path.display())),
        other => panic!("expected a read error, got {other:?}"),
    }
    let empty = scratch("no-sums");
    match macos_entry(&gh, REPO, TAG, VERSION, &empty, FLOOR) {
        Err(Error::Spawn { what, .. }) => {
            assert_eq!(what, format!("read {}", empty.join(SUMS).display()));
        }
        other => panic!("expected a read error, got {other:?}"),
    }
}
