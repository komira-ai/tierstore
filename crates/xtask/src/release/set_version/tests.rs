use super::*;
use crate::testdir::TestDir;

const MANIFEST_TEXT: &str = "[package]\nname = \"tierstore\"\n# a comment\nversion = \"0.3.0\"\nedition = \"2024\"\n\n[dependencies]\nversion = \"9.9.9\"\n";
const LOCK_TEXT: &str = "[[package]]\nname = \"other\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"tierstore\"\nversion = \"0.3.0\"\ndependencies = [\n \"x\",\n]\n\n[[package]]\nname = \"zed\"\nversion = \"2.0.0\"\n";

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    let d = TestDir::under(deps.join("xtask-set-version-unit"), name);
    std::fs::create_dir_all(d.join("crates/tierstore")).expect("dirs");
    d
}

fn read(d: &Path, name: &str) -> String {
    std::fs::read_to_string(d.join(name)).expect("read")
}

/// Asserts that the package's version is the one replaced, and that a `version` key of a later
/// table and every other line stay as they were.
#[test]
fn the_manifest_version_is_the_package_one_and_nothing_else_changes() {
    let got = set_manifest(MANIFEST_TEXT, "1.2.3").expect("set");
    assert_eq!(got, MANIFEST_TEXT.replace("\"0.3.0\"", "\"1.2.3\""));
    assert!(got.contains("[dependencies]\nversion = \"9.9.9\""));
}

/// Asserts that a manifest with no `[package]` table, or none holding a version before the next
/// table or the end of the file, is refused with words naming what is missing.
#[test]
fn a_manifest_without_a_package_version_is_refused() {
    let err = set_manifest("[dependencies]\nx = 1\n", "1.2.3").expect_err("no table");
    assert!(err.to_string().contains("no [package] table"), "{err}");
    let later = "[package]\nname = \"t\"\n[dependencies]\nversion = \"1\"\n";
    let err = set_manifest(later, "1.2.3").expect_err("no line");
    assert!(err.to_string().contains("[package] has no version line"));
    let err = set_manifest("[package]\nname = \"t\"\n", "1.2.3").expect_err("end");
    assert!(err.to_string().contains("[package] has no version line"));
}

/// Asserts that only the version line under `name = "tierstore"` changes in the lock, the
/// entries before and after it staying as they were.
#[test]
fn the_lock_version_is_that_of_the_tierstore_package_alone() {
    let got = set_lock(LOCK_TEXT, "1.2.3").expect("set");
    let expected = LOCK_TEXT.replace(
        "name = \"tierstore\"\nversion = \"0.3.0\"",
        "name = \"tierstore\"\nversion = \"1.2.3\"",
    );
    assert_eq!(got, expected);
    assert!(got.contains("name = \"other\"\nversion = \"1.0.0\""));
    assert!(got.contains("name = \"zed\"\nversion = \"2.0.0\""));
}

/// Asserts that a lock without the package, or whose entry has no version line, is refused
/// with words naming what is missing.
#[test]
fn a_lock_without_the_package_or_its_version_is_refused() {
    let other = "[[package]]\nname = \"other\"\nversion = \"1\"\n";
    let err = set_lock(other, "1.2.3").expect_err("no package");
    assert!(err.to_string().contains("no tierstore package"), "{err}");
    let split = "name = \"tierstore\"\n\nversion = \"1\"\n";
    let err = set_lock(split, "1.2.3").expect_err("no version");
    assert!(err.to_string().contains("tierstore has no version line"));
    let err = set_lock("name = \"tierstore\"\n", "1.2.3").expect_err("end");
    assert!(err.to_string().contains("tierstore has no version line"));
}

/// Asserts that a strict version is written to the manifest and the lock, and the line printed
/// names both files.
#[test]
fn both_files_are_written_for_a_strict_version() {
    let d = scratch("both");
    std::fs::write(d.join(MANIFEST), MANIFEST_TEXT).expect("manifest");
    std::fs::write(d.join(LOCK), LOCK_TEXT).expect("lock");
    let said = set_version(&d, "0.4.7").expect("set");
    assert_eq!(
        said,
        vec!["crates/tierstore/Cargo.toml and Cargo.lock: tierstore is 0.4.7".to_owned()]
    );
    assert!(read(&d, MANIFEST).contains("version = \"0.4.7\""));
    assert!(read(&d, LOCK).contains("name = \"tierstore\"\nversion = \"0.4.7\""));
}

/// Asserts that a version that is not `<major>.<minor>.<patch>` (empty, a `v`, a missing or
/// extra part, a suffix, a leading zero, not digits, a space) is refused and neither file is
/// written.
#[test]
fn a_version_of_another_shape_is_refused_and_writes_nothing() {
    let d = scratch("shape");
    std::fs::write(d.join(MANIFEST), MANIFEST_TEXT).expect("manifest");
    std::fs::write(d.join(LOCK), LOCK_TEXT).expect("lock");
    for bad in [
        "",
        "v0.4.7",
        "0.4",
        "0.4.7.1",
        "0.4.7-rc1",
        "0.04.7",
        "0.4.x",
        " 0.4.7",
    ] {
        let err = set_version(&d, bad).expect_err(bad);
        assert!(err.to_string().contains("<major>.<minor>.<patch>"), "{bad}");
    }
    assert_eq!(read(&d, MANIFEST), MANIFEST_TEXT);
    assert_eq!(read(&d, LOCK), LOCK_TEXT);
}

/// Asserts that a missing file is named, and that a refusal by the lock leaves the manifest as
/// it was.
#[test]
fn a_missing_file_is_named_and_a_refusal_by_one_leaves_the_other_alone() {
    let d = scratch("missing");
    let err = set_version(&d, "0.4.7").expect_err("no manifest");
    assert!(err.to_string().contains("Cargo.toml"), "{err}");
    std::fs::write(d.join(MANIFEST), MANIFEST_TEXT).expect("manifest");
    let err = set_version(&d, "0.4.7").expect_err("no lock");
    assert!(err.to_string().contains("Cargo.lock"), "{err}");
    std::fs::write(d.join(LOCK), "[[package]]\nname = \"other\"\n").expect("lock");
    let err = set_version(&d, "0.4.7").expect_err("no package");
    assert!(err.to_string().contains("no tierstore package"), "{err}");
    assert_eq!(read(&d, MANIFEST), MANIFEST_TEXT);
}
