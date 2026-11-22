//! The names of the put ingest: which strings are one component of a key, the directory of a job
//! and its key, where a path under the root is, and what a file's name says.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use super::names::{JobDir, Name, Place, is_component, name_of, place};
use crate::TreeDigest;

/// 64 lower-case hex digits and the digest they spell.
fn digest() -> (String, TreeDigest) {
    let sha = TreeDigest::of(b"names");
    (sha.to_string(), sha)
}

/// The directory of `build` and `nonce`, which are components.
fn job(build: &str, nonce: &str) -> JobDir {
    JobDir::new(build, nonce).unwrap()
}

/// Asserts that an ordinary name, a name of one byte, and names with dots and unicode letters
/// are components.
#[test]
fn ordinary_names_are_components() {
    for name in [
        "pack",
        "a",
        ".hidden",
        "a.b",
        "...",
        "pack.part",
        "caf\u{e9}",
        "\u{65e5}\u{672c}",
    ] {
        assert!(is_component(name), "{name:?}");
    }
}

/// Asserts that the empty name, `.` and `..` are not components.
#[test]
fn empty_and_dot_names_are_not_components() {
    for name in ["", ".", ".."] {
        assert!(!is_component(name), "{name:?}");
    }
}

/// Asserts that a name with a slash, a NUL, white space (ASCII or unicode) or a control character
/// is not a component.
#[test]
fn separators_white_space_and_control_characters_are_not_components() {
    for name in [
        "a/b",
        "/",
        "a\0b",
        "a b",
        " ",
        "a\tb",
        "a\nb",
        "a\u{a0}b",
        "a\u{2003}b",
        "a\u{7f}b",
        "a\u{1b}b",
        "a\u{85}b",
    ] {
        assert!(!is_component(name), "{name:?}");
    }
}

/// Asserts that a name of 255 bytes is a component and one of 256 bytes is not, and that the limit
/// counts bytes, not characters.
#[test]
fn a_component_has_at_most_255_bytes() {
    assert!(is_component(&"a".repeat(255)));
    assert!(!is_component(&"a".repeat(256)));
    assert!(is_component(&"\u{e9}".repeat(127)));
    assert!(!is_component(&"\u{e9}".repeat(128)));
}

/// Asserts that a job directory is made of a build and a nonce that are both components, and that
/// either one that is not refuses it.
#[test]
fn a_job_dir_refuses_a_build_or_nonce_that_is_not_a_component() {
    assert!(JobDir::new("b", "n").is_some());
    assert!(JobDir::new("", "n").is_none());
    assert!(JobDir::new("b", "").is_none());
    assert!(JobDir::new("a/b", "n").is_none());
    assert!(JobDir::new("b", "..").is_none());
    assert!(JobDir::new("b", "n n").is_none());
}

/// Asserts that a job directory names `cas/b/<build>` as its build directory,
/// `cas/b/<build>/<nonce>` as its relative path, and prints as that path with slashes.
#[test]
fn a_job_dir_has_its_build_dir_relative_path_and_display() {
    let dir = job("b1", "n1");
    assert_eq!(dir.build_dir(), PathBuf::from("cas/b/b1"));
    assert_eq!(dir.rel(), PathBuf::from("cas/b/b1/n1"));
    assert_eq!(dir.to_string(), "cas/b/b1/n1");
}

/// Asserts that the key of a file is `cas/b/<build>/<nonce>/<name>`, and that the longest one
/// three components make (each 255 bytes) is 773 bytes, within the 1024 of an S3 key.
#[test]
fn a_key_is_the_files_path_and_the_longest_fits_a_key() {
    assert_eq!(job("b1", "n1").key("pack"), "cas/b/b1/n1/pack");
    let long = "x".repeat(255);
    let key = job(&long, &long).key(&long);
    assert_eq!(key.len(), 773);
    assert!(key.len() <= 1024);
}

/// Asserts that a path with fewer than four components is above the jobs' directories and
/// carries its number of components.
#[test]
fn paths_down_to_a_build_dir_are_above_the_jobs() {
    assert_eq!(place(Path::new("")), Place::Above(0));
    assert_eq!(place(Path::new("cas")), Place::Above(1));
    assert_eq!(place(Path::new("cas/b")), Place::Above(2));
    assert_eq!(place(Path::new("cas/b/b1")), Place::Above(3));
}

/// Asserts that a path of four components is a job's directory and one of five is a file in it.
#[test]
fn a_job_dir_and_its_file_are_placed() {
    assert_eq!(place(Path::new("cas/b/b1/n1")), Place::Job(job("b1", "n1")));
    assert_eq!(
        place(Path::new("cas/b/b1/n1/pack")),
        Place::File(job("b1", "n1"), "pack".to_owned())
    );
}

/// Asserts that a path whose first components are not `cas` and `b` is elsewhere, at every depth.
#[test]
fn a_path_with_the_wrong_prefix_is_elsewhere() {
    for path in [
        "x",
        "cas/x",
        "x/b",
        "x/b/b1",
        "cas/x/b1/n1",
        "x/b/b1/n1/pack",
        "b/cas",
    ] {
        assert_eq!(place(Path::new(path)), Place::Elsewhere, "{path}");
    }
}

/// Asserts that a path deeper than a job's files is elsewhere.
#[test]
fn a_path_deeper_than_a_file_is_elsewhere() {
    assert_eq!(place(Path::new("cas/b/b1/n1/pack/more")), Place::Elsewhere);
    assert_eq!(place(Path::new("cas/b/b1/n1/a/b/c")), Place::Elsewhere);
}

/// Asserts that `..`, an absolute path and a component that is not a key component make the path
/// elsewhere, wherever they are.
#[test]
fn a_path_with_a_parent_root_or_bad_component_is_elsewhere() {
    for path in [
        "..",
        "../cas",
        "cas/../b",
        "cas/b/b1/../n1",
        "cas/b/b1/n1/..",
        "/",
        "/cas/b/b1/n1",
        "cas/b/b 1/n1",
        "cas/b/b1/n\t1",
        "cas/b/b1/n1/a\nb",
    ] {
        assert_eq!(place(Path::new(path)), Place::Elsewhere, "{path:?}");
    }
    let long = format!("cas/b/b1/n1/{}", "a".repeat(256));
    assert_eq!(place(Path::new(&long)), Place::Elsewhere);
}

/// Asserts that a component that is not valid UTF-8 makes the path elsewhere, at any depth.
#[test]
fn a_component_that_is_not_utf8_is_elsewhere() {
    let bad = OsStr::from_bytes(b"p\xffck");
    for depth in 0..5 {
        let mut path = PathBuf::new();
        for part in ["cas", "b", "b1", "n1"].iter().take(depth) {
            path.push(part);
        }
        path.push(bad);
        assert_eq!(place(&path), Place::Elsewhere, "{path:?}");
    }
}

/// Asserts that a name without `.part` is committed, whatever else it has.
#[test]
fn a_name_without_part_is_committed() {
    let (hex, _) = digest();
    for name in ["pack", "pack.partial", "part", ".part.x", "a.sha256-x"] {
        assert_eq!(name_of(name), Name::Committed, "{name}");
    }
    assert_eq!(name_of(&format!("pack.sha256-{hex}")), Name::Committed);
}

/// Asserts that `<name>.part` declares nothing, and neither does a bare `.part`.
#[test]
fn a_part_name_without_a_digest_is_part() {
    assert_eq!(name_of("pack.part"), Name::Part);
    assert_eq!(name_of(".part"), Name::Part);
    assert_eq!(name_of("pack.sha256-.part"), Name::Part);
}

/// Asserts that `<name>.sha256-<64 lower-case hex>.part` declares the digest for `<name>`.
#[test]
fn a_part_name_with_a_digest_declares_it() {
    let (hex, sha256) = digest();
    assert_eq!(
        name_of(&format!("pack.sha256-{hex}.part")),
        Name::Declares {
            name: "pack".to_owned(),
            sha256
        }
    );
}

/// Asserts that upper-case hex, 63 hex digits, 65 hex digits and non-hex characters in the digest
/// do not declare anything.
#[test]
fn a_digest_that_is_not_64_lower_case_hex_digits_declares_nothing() {
    let (hex, _) = digest();
    let upper = hex.to_uppercase();
    assert_ne!(upper, hex);
    for bad in [
        upper,
        hex[1..].to_owned(),
        format!("{hex}0"),
        "g".repeat(64),
    ] {
        assert_eq!(
            name_of(&format!("pack.sha256-{bad}.part")),
            Name::Part,
            "{bad}"
        );
    }
}

/// Asserts that a declaration whose base is empty or is not a component is only a part name.
#[test]
fn a_declaration_for_a_base_that_is_not_a_component_is_part() {
    let (hex, _) = digest();
    for base in ["", "a b", "a\tb", ".", ".."] {
        let name = format!("{base}.sha256-{hex}.part");
        assert_eq!(name_of(&name), Name::Part, "{name:?}");
    }
}

/// Asserts that a declaration whose base itself ends in `.part` is only a part name.
#[test]
fn a_declaration_for_a_part_name_is_part() {
    let (hex, _) = digest();
    assert_eq!(name_of(&format!("pack.part.sha256-{hex}.part")), Name::Part);
}

/// Asserts that a name with several `.sha256-` marks takes the last one for the digest and keeps
/// the earlier ones in the base.
#[test]
fn the_last_digest_mark_of_a_name_counts() {
    let (hex, sha256) = digest();
    let other = "0".repeat(64);
    assert_eq!(
        name_of(&format!("a.sha256-{other}.sha256-{hex}.part")),
        Name::Declares {
            name: format!("a.sha256-{other}"),
            sha256
        }
    );
    assert_eq!(
        name_of(&format!("a.sha256-{hex}.sha256-nothex.part")),
        Name::Part
    );
}
