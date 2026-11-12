use std::path::PathBuf;

use super::*;
use crate::release::hostkeeper_package::archive;
use crate::testdir::TestDir;

/// The commit the packages are of.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The version of the packages of [`COMMIT`].
const VERSION: &str = "0.4.0+0123456789ab";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-hostkeeper-index-unit"), name)
}

/// The bytes of the package of `platform` as `version`.
fn package_bytes(platform: Platform, version: &str) -> Vec<u8> {
    let mut binary = b"\x7fELF stand-in: tierstore ".to_vec();
    binary.extend_from_slice(version.as_bytes());
    binary.push(0);
    archive(platform, version, "0.1.0", &binary).expect("archive")
}

/// Writes the package of `platform` as `version` into `dir`.
fn put(dir: &Path, version: &str, platform: Platform) -> PathBuf {
    let path = dir.join(file_name(version, platform));
    std::fs::write(&path, package_bytes(platform, version)).expect("write the package");
    path
}

/// Writes the packages of [`VERSION`] for `platforms` into `dir`.
fn put_all(dir: &Path, platforms: &[Platform]) {
    for platform in platforms {
        put(dir, VERSION, *platform);
    }
}

/// The lines of a refusal.
fn refused<T: std::fmt::Debug>(result: Result<T, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The message of a failure.
fn failed<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(Error::Failed(text)) => text,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn missing_line(platform: &str) -> String {
    format!(
        "no hostkeeper package for {platform}: index.json lists every platform, and hostkeeper refuses a host of a platform it lacks (no_package_for_platform)"
    )
}

fn missing_macos_line() -> String {
    missing_line("macos-arm64")
}

#[test]
fn the_index_lists_each_package_with_the_sha256_and_size_of_its_file() {
    // Catches: an index whose digest or size is not that of the archive a host downloads, or
    // one hostkeeper's own reader refuses.
    let dir = scratch("entries");
    put_all(&dir, &Platform::ALL);
    hostkeeper_index(&dir, COMMIT, "push", None).expect("index");
    let text = std::fs::read_to_string(dir.join("index.json")).expect("read the index");
    let index = ReleaseIndex::parse(
        &text,
        &service().expect("service"),
        &version(VERSION).expect("version"),
    )
    .expect("hostkeeper reads the index");
    assert_eq!(index.packages().len(), 3);
    for platform in Platform::ALL {
        let file = std::fs::read(dir.join(file_name(VERSION, platform))).expect("read");
        let entry = index.entry_for(host_platform(platform)).expect("entry");
        assert_eq!(entry.sha256.as_str(), sha256_hex(&file), "{platform:?}");
        assert_eq!(entry.bytes, file.len() as u64, "{platform:?}");
    }
}

#[test]
fn the_index_is_canonical_and_the_same_for_the_same_packages() {
    // Catches: an index whose bytes differ between runs or runners (order, white space, a final
    // newline), so that its sums line and its attestation would not repeat.
    let first = scratch("canonical-a");
    let second = scratch("canonical-b");
    for dir in [&first, &second] {
        put_all(
            dir,
            &[
                Platform::MacosArm64,
                Platform::LinuxX86_64,
                Platform::LinuxArm64,
            ],
        );
        hostkeeper_index(dir, COMMIT, "push", None).expect("index");
    }
    let one = std::fs::read(first.join("index.json")).expect("read");
    let two = std::fs::read(second.join("index.json")).expect("read");
    assert_eq!(one, two);
    assert_eq!(
        std::fs::read(first.join("SHA256SUMS.index")).expect("read"),
        std::fs::read(second.join("SHA256SUMS.index")).expect("read")
    );
    let text = String::from_utf8(one).expect("utf-8");
    assert!(text.starts_with(r#"{"packages":[{"bytes":"#), "{text}");
    assert!(!text.ends_with('\n'));
    let index = ReleaseIndex::parse(
        &text,
        &service().expect("service"),
        &version(VERSION).expect("version"),
    )
    .expect("parse");
    assert_eq!(index.to_json(), text);
    let order: Vec<_> = index.packages().iter().map(|p| p.platform).collect();
    assert_eq!(
        order,
        [
            HostPlatform::LinuxArm64,
            HostPlatform::LinuxX86_64,
            HostPlatform::MacosArm64
        ]
    );
    let arm = text.find("linux-arm64").expect("arm64");
    let x86 = text.find("linux-x86_64").expect("x86_64");
    let mac = text.find("macos-arm64").expect("macos");
    assert!(arm < x86 && x86 < mac, "{text}");
}

#[test]
fn the_index_part_is_the_sums_line_of_index_json() {
    // Catches: a SHA256SUMS.index whose digest is not that of the index.json beside it, or whose
    // line is not in the sums format.
    let dir = scratch("part");
    put_all(&dir, &Platform::ALL);
    let lines = hostkeeper_index(&dir, COMMIT, "push", None).expect("index");
    let json = std::fs::read(dir.join("index.json")).expect("read");
    let expected = format!("{}  index.json\n", sha256_hex(&json));
    assert_eq!(
        std::fs::read_to_string(dir.join("SHA256SUMS.index")).expect("read"),
        expected
    );
    assert_eq!(
        lines[0],
        format!(
            "index.json of tierstore {VERSION}: 3 packages, {}  index.json",
            sha256_hex(&json)
        )
    );
    assert_eq!(lines[1].as_bytes(), &json[..]);
}

#[test]
fn output_gets_the_package_version_appended_to_what_is_there() {
    // Catches: --output overwriting the file's earlier lines (the other steps' outputs), or the
    // version not being written.
    let dir = scratch("output");
    put_all(&dir, &Platform::ALL);
    let output = dir.join("github-output");
    std::fs::write(&output, "earlier=1\n").expect("write");
    hostkeeper_index(&dir, COMMIT, "push", Some(output.as_path())).expect("index");
    assert_eq!(
        std::fs::read_to_string(&output).expect("read"),
        "earlier=1\npackage-version=0.4.0+0123456789ab\n"
    );
}

#[test]
fn a_push_or_manual_run_without_the_arm64_and_macos_packages_is_refused_and_writes_nothing() {
    // Catches: a release index of one platform, which leaves every Mac on its old version.
    for event in ["push", "workflow_dispatch"] {
        let dir = scratch(&format!("no-macos-{event}"));
        put_all(&dir, &PULL_REQUEST_PLATFORMS);
        let lines = refused(hostkeeper_index(&dir, COMMIT, event, None));
        assert_eq!(
            lines,
            [missing_line("linux-arm64"), missing_macos_line()],
            "{event}"
        );
        assert!(!dir.join("index.json").exists(), "{event}");
        assert!(!dir.join("SHA256SUMS.index").exists(), "{event}");
    }
}

#[test]
fn a_pull_request_with_the_linux_x86_64_package_writes_nothing() {
    // Catches: an index written for a pull request (an index of one platform), or a pull
    // request run refused for lacking the packages of the platforms it does not build.
    let dir = scratch("pr-ok");
    put_all(&dir, &PULL_REQUEST_PLATFORMS);
    let output = dir.join("github-output");
    let lines =
        hostkeeper_index(&dir, COMMIT, "pull_request", Some(output.as_path())).expect("accepted");
    assert_eq!(
        lines,
        [
            "a pull request: only linux-x86_64 builds for one, so no index.json is written; its package of 0.4.0+0123456789ab is there"
        ]
    );
    assert!(!dir.join("index.json").exists());
    assert!(!dir.join("SHA256SUMS.index").exists());
    assert!(!output.exists());
}

#[test]
fn a_pull_request_with_another_platforms_package_or_without_linux_x86_64_is_refused() {
    // Catches: a pull request run that accepts the arm64 or macOS package it does not build, or
    // a run without the linux-x86_64 package.
    let with_others = scratch("pr-others");
    put_all(&with_others, &Platform::ALL);
    assert_eq!(
        refused(hostkeeper_index(&with_others, COMMIT, "pull_request", None)),
        [
            "a hostkeeper package for linux-arm64, which a pull request does not build (only linux-x86_64 builds for one)",
            "a hostkeeper package for macos-arm64, which a pull request does not build (only linux-x86_64 builds for one)"
        ]
    );
    let one = scratch("pr-one");
    put_all(&one, &[Platform::LinuxArm64]);
    assert_eq!(
        refused(hostkeeper_index(&one, COMMIT, "pull_request", None)),
        [
            missing_line("linux-x86_64"),
            "a hostkeeper package for linux-arm64, which a pull request does not build (only linux-x86_64 builds for one)".to_owned()
        ]
    );
}

#[test]
fn packages_refuses_a_file_that_names_no_release_platform() {
    // Catches: a stray tierstore-*.tar.zst of an unknown platform being ignored, and left out of
    // the index without a word.
    let dir = scratch("unknown-platform");
    let name = "tierstore-0.4.0+0123456789ab-windows-x86_64.tar.zst";
    std::fs::write(dir.join(name), b"x").expect("write");
    assert_eq!(
        refused(packages(&dir, COMMIT)),
        [format!(
            "{name}: not tierstore-<version>-<platform>.tar.zst of a release platform"
        )]
    );
}

#[test]
fn packages_refuses_a_version_that_is_not_a_release_version_of_the_commit() {
    // Catches: a package of a plain version, or of another commit's build, entering the index of
    // this commit.
    let dir = scratch("wrong-version");
    let plain = "tierstore-0.4.0-linux-arm64.tar.zst";
    let other = "tierstore-0.4.0+aaaaaaaaaaaa-linux-x86_64.tar.zst";
    std::fs::write(dir.join(plain), b"x").expect("write");
    std::fs::write(dir.join(other), b"x").expect("write");
    let tail = "<major>.<minor>.<patch>+0123456789ab";
    assert_eq!(
        refused(packages(&dir, COMMIT)),
        [
            format!("{other}: \"0.4.0+aaaaaaaaaaaa\" is not a version of 0123456789ab, {tail}"),
            format!("{plain}: \"0.4.0\" is not a version of 0123456789ab, {tail}"),
        ]
    );
}

#[test]
fn packages_refuses_two_versions_in_one_directory() {
    // Catches: an index mixing two versions' packages, which no host could install as one release.
    let dir = scratch("two-versions");
    put(&dir, VERSION, Platform::LinuxArm64);
    put(&dir, "0.5.0+0123456789ab", Platform::LinuxX86_64);
    assert_eq!(
        refused(packages(&dir, COMMIT)),
        [
            "tierstore-0.5.0+0123456789ab-linux-x86_64.tar.zst: of the version 0.5.0+0123456789ab, not 0.4.0+0123456789ab, the version of the other packages"
        ]
    );
}

#[test]
fn packages_ignores_files_that_are_not_packages() {
    // Catches: tarballs, bundles and sums files in the directory being taken for packages.
    let dir = scratch("others");
    put(&dir, VERSION, Platform::LinuxX86_64);
    for other in [
        "tierstore-0123456789ab-linux-x86_64.tar.gz",
        "tierstore-0.4.0+0123456789ab-linux-x86_64.tar.zst.sigstore.json",
        "SHA256SUMS.linux-x86_64",
        "readme.tar.zst",
    ] {
        std::fs::write(dir.join(other), b"x").expect("write");
    }
    assert_eq!(
        packages(&dir, COMMIT).expect("packages"),
        Packages {
            version: VERSION.to_owned(),
            platforms: vec![Platform::LinuxX86_64],
        }
    );
}

#[test]
fn packages_lists_the_platforms_in_the_order_of_their_names() {
    // Catches: platforms in directory order, which differs between file systems.
    let dir = scratch("order");
    put_all(
        &dir,
        &[
            Platform::MacosArm64,
            Platform::LinuxX86_64,
            Platform::LinuxArm64,
        ],
    );
    assert_eq!(
        packages(&dir, COMMIT).expect("packages").platforms,
        [
            Platform::LinuxArm64,
            Platform::LinuxX86_64,
            Platform::MacosArm64
        ]
    );
}

#[test]
fn packages_fails_on_an_empty_directory_and_on_one_that_cannot_be_listed() {
    // Catches: an empty directory giving an index of nothing, and a listing error taken for an
    // empty directory.
    let dir = scratch("empty");
    assert_eq!(
        failed(packages(&dir, COMMIT)),
        format!(
            "{} holds no hostkeeper package (tierstore-<version>-<platform>.tar.zst)",
            dir.display()
        )
    );
    let file = dir.join("a-file");
    std::fs::write(&file, b"x").expect("write");
    match packages(&file, COMMIT) {
        Err(Error::Spawn { what, .. }) => assert_eq!(what, format!("list {}", file.display())),
        other => panic!("expected a listing error, got {other:?}"),
    }
}

#[test]
fn parse_name_reads_the_version_and_platform_of_a_package_name() {
    // Catches: a platform read wrongly from a name, or a name without the prefix, the suffix or
    // the dash before the platform being accepted.
    for platform in Platform::ALL {
        let name = format!("tierstore-0.4.0+0123456789ab-{}.tar.zst", platform.name());
        assert_eq!(parse_name(&name), Some(("0.4.0+0123456789ab", platform)));
    }
    for bad in [
        "0.4.0-linux-arm64.tar.zst",
        "tierstore-0.4.0-linux-arm64.tar.gz",
        "tierstore-0.4.0-linux-arm64",
        "tierstore-0.4.0-windows-x86_64.tar.zst",
        "tierstore-0.4.0linux-arm64.tar.zst",
        "tierstore-.tar.zst",
    ] {
        assert_eq!(parse_name(bad), None, "{bad}");
    }
}

#[test]
fn the_platforms_of_hostkeeper_and_of_this_crate_round_trip() {
    // Catches: a platform mapped to another, which would list a package under the wrong host.
    for platform in Platform::ALL {
        assert_eq!(our_platform(host_platform(platform)), platform);
        assert_eq!(host_platform(platform).as_str(), platform.name());
    }
}

#[test]
fn a_commit_that_is_not_forty_hex_digits_is_a_usage_error() {
    // Catches: a short or malformed --commit accepted, naming packages of the wrong build.
    let dir = scratch("usage");
    put_all(&dir, &Platform::ALL);
    for commit in [
        "0123456789ab",
        "",
        "0123456789ABCDEF0123456789abcdef01234567",
    ] {
        match hostkeeper_index(&dir, commit, "push", None) {
            Err(Error::Usage(text)) => assert_eq!(
                text,
                format!("--commit {commit:?} is not a full commit SHA (40 lowercase hex digits)")
            ),
            other => panic!("expected a usage error, got {other:?}"),
        }
    }
}

#[test]
fn an_index_already_there_is_not_overwritten() {
    // Catches: a second run replacing the index a first run wrote (and its attestation's subject).
    let dir = scratch("again");
    put_all(&dir, &Platform::ALL);
    hostkeeper_index(&dir, COMMIT, "push", None).expect("first");
    let before = std::fs::read(dir.join("index.json")).expect("read");
    match hostkeeper_index(&dir, COMMIT, "push", None) {
        Err(Error::Spawn { what, .. }) => {
            assert_eq!(what, format!("write {}", dir.join("index.json").display()));
        }
        other => panic!("expected a write error, got {other:?}"),
    }
    assert_eq!(std::fs::read(dir.join("index.json")).expect("read"), before);
}

#[test]
fn digest_file_hashes_a_file_larger_than_its_buffer() {
    // Catches: a digest of only the first piece of a large package, or a size short of the file.
    let dir = scratch("digest");
    let bytes: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
    let path = dir.join("big");
    std::fs::write(&path, &bytes).expect("write");
    let (digest, size) = digest_file(&path).expect("digest");
    assert_eq!(digest.as_str(), sha256_hex(&bytes));
    assert_eq!(size, bytes.len() as u64);
    let empty = dir.join("empty");
    std::fs::write(&empty, b"").expect("write");
    let (digest, size) = digest_file(&empty).expect("digest");
    assert_eq!(digest.as_str(), sha256_hex(b""));
    assert_eq!(size, 0);
}

#[test]
fn digest_file_names_the_file_it_cannot_read() {
    // Catches: a missing package reported without its path.
    let dir = scratch("digest-missing");
    let path = dir.join("absent");
    match digest_file(&path) {
        Err(Error::Spawn { what, .. }) => assert_eq!(what, format!("read {}", path.display())),
        other => panic!("expected a read error, got {other:?}"),
    }
}

#[test]
fn a_package_that_vanishes_before_it_is_read_fails_naming_it() {
    // Catches: index_of hiding the read error of a package, or reading one it did not list.
    let dir = scratch("vanished");
    let found = Packages {
        version: VERSION.to_owned(),
        platforms: vec![Platform::LinuxArm64],
    };
    match index_of(&dir, &found) {
        Err(Error::Spawn { what, .. }) => assert_eq!(
            what,
            format!(
                "read {}",
                dir.join(file_name(VERSION, Platform::LinuxArm64)).display()
            )
        ),
        other => panic!("expected a read error, got {other:?}"),
    }
}

#[test]
fn an_index_error_is_reported_with_its_reason_token() {
    // Catches: an index refusal reported without the reason token a host reports for it.
    assert_eq!(
        failed::<()>(Err(index_failed(IndexError::NoPackages))),
        "the release index lists no package (index_invalid)"
    );
}

#[test]
fn hostkeeper_names_are_built_and_a_version_outside_its_grammar_fails() {
    // Catches: a version outside hostkeeper's grammar being let into the index.
    assert_eq!(service().expect("service").as_str(), "tierstore");
    assert_eq!(version(VERSION).expect("version").as_str(), VERSION);
    assert!(failed(version("not a version")).starts_with("\"not a version\" is not a version"));
}
