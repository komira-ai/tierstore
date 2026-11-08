use std::io::Write;
use std::path::PathBuf;

use super::*;
use crate::package::{LICENSES_MODE, LICENSES_NAME, MODE};
use crate::testdir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BINARY_BYTES: &[u8] = b"\x7fELF stand-in for tierstore";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-tap-tarball-unit"), name)
}

/// The gzip stream of `tar` bytes.
fn gzip(tar: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(tar).expect("gzip");
    encoder.finish().expect("gzip")
}

/// A tar of one entry `path` of `kind` holding `bytes`, gzipped.
fn tarball_of(path: &str, kind: tar::EntryType, bytes: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_path(path).expect("a path");
    header.set_entry_type(kind);
    header.set_size(bytes.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append(&header, bytes).expect("append");
    gzip(&builder.into_inner().expect("tar"))
}

/// `dir/tierstore-<COMMIT>-<platform>.tar.gz` holding `tgz`.
fn put_source(dir: &Path, platform: Platform, tgz: &[u8]) -> PathBuf {
    let path = dir.join(format!("tierstore-{COMMIT}-{}.tar.gz", platform.name()));
    std::fs::write(&path, tgz).expect("write the source");
    path
}

#[test]
fn the_tap_names_each_platform_as_the_formula_downloads_it() {
    // Catches: a name the tap's guard refuses (linux-arm64 for linux-aarch64, a commit instead of
    // a version), which would fail the tap's pull request for every release.
    assert_eq!(
        file_name("1.2.3", Platform::MacosArm64),
        "tierstore-1.2.3-macos-arm64.tar.gz"
    );
    assert_eq!(
        file_name("1.2.3", Platform::LinuxArm64),
        "tierstore-1.2.3-linux-aarch64.tar.gz"
    );
    assert_eq!(
        file_name("1.2.3", Platform::LinuxX86_64),
        "tierstore-1.2.3-linux-x86_64.tar.gz"
    );
}

#[test]
fn the_copy_is_the_commit_named_tarball_byte_for_byte_and_is_listed_in_the_platforms_sums() {
    // Catches: a tap tarball whose binary differs from the commit-named tarball's, a licence
    // notice dropped from it, and a sums line for other bytes or in another platform's file.
    let dir = scratch("copy");
    let binary = dir.join("tierstore");
    std::fs::write(&binary, BINARY_BYTES).expect("binary");
    let sbom = dir.join("sbom.json");
    std::fs::write(&sbom, b"{}").expect("sbom");
    let notice = dir.join("notice");
    std::fs::write(&notice, b"THIRD-PARTY\n").expect("notice");
    let out = dir.join("dist");
    for platform in Platform::ALL {
        crate::package::package_with(&binary, &sbom, Some(&notice), platform, COMMIT, &out)
            .expect("package");
        let printed = tap_tarball(&out, COMMIT, "0.4.0", platform).expect("tap tarball");
        let source =
            std::fs::read(out.join(format!("tierstore-{COMMIT}-{}.tar.gz", platform.name())))
                .expect("source");
        let copy = std::fs::read(out.join(file_name("0.4.0", platform))).expect("copy");
        assert_eq!(copy, source);
        assert!(
            crate::package::check_entries(
                &copy,
                &[
                    ("tierstore", MODE, BINARY_BYTES),
                    (LICENSES_NAME, LICENSES_MODE, b"THIRD-PARTY\n"),
                ]
            )
            .is_empty()
        );
        let digest = Sha256::of(&copy);
        let sums = std::fs::read_to_string(out.join(format!("SHA256SUMS.{}", platform.name())))
            .expect("sums");
        assert!(
            sums.ends_with(&format!("{digest}  {}\n", file_name("0.4.0", platform))),
            "{sums}"
        );
        assert_eq!(
            sums.lines().count(),
            3,
            "the tarball, the SBOM and the copy"
        );
        assert_eq!(printed.len(), 1);
        assert!(printed[0].starts_with(&format!("{}: a copy of ", file_name("0.4.0", platform))));
    }
}

#[test]
fn a_commit_or_version_that_is_not_of_its_form_is_a_usage_error() {
    // Catches: a short commit or a version with a commit suffix naming a file the tap refuses.
    let dir = scratch("usage");
    put_source(
        &dir,
        Platform::LinuxArm64,
        &tarball_of("tierstore", tar::EntryType::Regular, b"x"),
    );
    let short = tap_tarball(&dir, "0123456789ab", "0.4.0", Platform::LinuxArm64);
    assert_eq!(
        short,
        Err(Error::Usage(
            "--commit \"0123456789ab\" is not a full commit SHA (40 lowercase hex digits)"
                .to_owned()
        ))
    );
    for bad in ["0.4.0+0123456789ab", "0.4", "v0.4.0", ""] {
        assert_eq!(
            tap_tarball(&dir, COMMIT, bad, Platform::LinuxArm64),
            Err(Error::Usage(format!(
                "--version {bad:?} is not <major>.<minor>.<patch>"
            )))
        );
    }
}

#[test]
fn a_source_that_is_missing_or_does_not_start_with_the_binary_is_refused() {
    // Catches: a copy of a file that is not a tarball of the binary, which the tap would install
    // as a broken formula.
    let dir = scratch("source");
    let platform = Platform::LinuxX86_64;
    let missing = tap_tarball(&dir, COMMIT, "0.4.0", platform).expect_err("no source");
    assert!(matches!(missing, Error::Spawn { .. }), "{missing:?}");
    let cases: [(&str, Vec<u8>, &str); 5] = [
        ("not gzip", b"plain text".to_vec(), "not a gzip stream"),
        ("empty tar", gzip(&[]), "holds no readable first entry"),
        (
            "garbage tar",
            gzip(&[7; 512]),
            "holds no readable first entry",
        ),
        (
            "another name",
            tarball_of("other", tar::EntryType::Regular, b"x"),
            "starts with \"other\", not the regular file \"tierstore\"",
        ),
        (
            "a directory",
            tarball_of("tierstore", tar::EntryType::Directory, b""),
            "starts with \"tierstore\", not the regular file \"tierstore\"",
        ),
    ];
    for (what, tgz, says) in cases {
        let source = put_source(&dir, platform, &tgz);
        let error = tap_tarball(&dir, COMMIT, "0.4.0", platform).expect_err(what);
        let want = format!("{}: {says}", source.display());
        assert!(error.to_string().starts_with(&want), "{what}: {error}");
        assert!(!dir.join(file_name("0.4.0", platform)).exists(), "{what}");
    }
}

#[test]
fn a_copy_or_a_sums_line_that_is_there_already_is_refused() {
    // Catches: a second run overwriting a file whose digest the sums already hold, and a sums
    // line listed twice.
    let dir = scratch("twice");
    let platform = Platform::MacosArm64;
    put_source(
        &dir,
        platform,
        &tarball_of("tierstore", tar::EntryType::Regular, b"x"),
    );
    tap_tarball(&dir, COMMIT, "0.4.0", platform).expect("first");
    let again = tap_tarball(&dir, COMMIT, "0.4.0", platform).expect_err("twice");
    assert!(
        again
            .to_string()
            .contains("already lists tierstore-0.4.0-macos-arm64.tar.gz"),
        "{again}"
    );
    std::fs::remove_file(dir.join("SHA256SUMS.macos-arm64")).expect("remove the sums");
    let exists = tap_tarball(&dir, COMMIT, "0.4.0", platform);
    assert!(matches!(exists, Err(Error::Spawn { .. })), "{exists:?}");
}
