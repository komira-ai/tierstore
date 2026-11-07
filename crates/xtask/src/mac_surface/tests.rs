use super::*;
use crate::testdir::TestDir;

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-mac-surface-unit"), name)
}

/// A path under each kind of entry is on the surface: a directory, a file, the start of a family
/// of files; and the paths beside them are not.
#[test]
fn a_path_is_on_the_surface_when_it_starts_with_an_entry() {
    // Catches: an entry matched as the whole path only (a file of a listed directory, or of a
    // listed family, would run no macOS check), and a match on a part of the path other than its
    // start.
    for path in [
        "crates/tierstore/src/server/update_hook/launchd/mod.rs",
        "crates/tierstore/src/server/update_hook/machine/runner.rs",
        "packaging/hostkeeper/units/tierstore.plist",
        "crates/tierstore/tests/packaging_macos.rs",
        "crates/tierstore/tests/packaging/secrets.rs",
        "crates/tierstore/src/server/cache_handoff_sockets_tests.rs",
        ".github/workflows/macos.yml",
        ".github/workflows/artifacts.yml",
        "Cargo.toml",
        "rust-toolchain.toml",
        "crates/tierstore/Cargo.toml",
        "crates/xtask/src/release/hostkeeper_package.rs",
    ] {
        assert!(on_surface(path.as_bytes()), "{path}");
    }
    for path in [
        "Cargo.lock",
        "README.md",
        "docs/ci-and-release.md",
        ".github/workflows/ci.yml",
        "crates/tierstore/src/server/bucket.rs",
        "crates/tierstore-model/Cargo.toml",
        "crates/xtask/src/advisories.rs",
        "x/packaging/tierstore.toml",
        "",
    ] {
        assert!(!on_surface(path.as_bytes()), "{path}");
    }
}

/// Every entry of the surface is a path of its own kind: relative, with no `./`, no `//`, no
/// glob and no backslash, and listed once.
#[test]
fn every_entry_is_a_plain_relative_prefix_listed_once() {
    // Catches: an entry written as a glob or with a leading `./` or `/`, which a path from `git
    // diff` never starts with, so the files it means would run no macOS check.
    for entry in SURFACE {
        assert!(!entry.is_empty(), "an empty entry takes every path");
        assert!(
            !entry.starts_with('/') && !entry.starts_with("./"),
            "{entry}"
        );
        assert!(
            !entry.contains("//") && !entry.contains('*') && !entry.contains('\\'),
            "{entry}"
        );
        assert_eq!(
            SURFACE.iter().filter(|e| *e == entry).count(),
            1,
            "{entry} is listed twice"
        );
    }
}

/// The NUL-ended list of `git diff -z`: each path is judged, in order; empty entries are skipped;
/// a last path without its NUL is still a path; a path that is not UTF-8 is still judged.
#[test]
fn touched_takes_each_nul_ended_path_and_keeps_the_order() {
    // Catches: a list split at newlines (a path with a newline in it would be two paths), a last
    // path dropped when the NUL after it is missing, an empty entry read as a path (an empty
    // prefix would take it), and a path that is not UTF-8 refused or skipped.
    let list = b"README.md\0packaging/macos/tierstore.toml\0\0Cargo.lock\0crates/tierstore/src/server/update_hook/a\nb.rs\0packaging/\xff.plist";
    let got = touched(list);
    assert_eq!(
        got,
        [
            &b"packaging/macos/tierstore.toml"[..],
            &b"crates/tierstore/src/server/update_hook/a\nb.rs"[..],
            &b"packaging/\xff.plist"[..],
        ]
    );
    assert!(touched(b"").is_empty());
    assert!(touched(b"\0\0").is_empty());
    assert!(touched(b"README.md\0docs/x.md\0").is_empty());
}

/// On the surface: each path is printed and `mac=true` is the last line and the line appended to
/// the output, after what the output already held.
#[test]
fn the_verb_prints_the_paths_and_appends_mac_true() {
    // Catches: an answer written over the output file (GITHUB_OUTPUT holds the other steps'
    // outputs), an answer that is not the last printed line, and a path that is not printed.
    let d = scratch("true");
    let changed = d.join("changed");
    std::fs::write(&changed, b"README.md\0packaging/tierstore.service\0").expect("write");
    let output = d.join("output");
    std::fs::write(&output, "version=0.3.1\n").expect("write");
    let lines = mac_surface(&changed, Some(&output)).expect("verb");
    assert_eq!(
        lines,
        [
            "on the macOS surface: packaging/tierstore.service",
            "mac=true"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&output).expect("read"),
        "version=0.3.1\nmac=true\n"
    );
}

/// Off the surface, and with no changed path at all: `mac=false`, printed and appended; and with
/// no output file, the answer is printed alone.
#[test]
fn the_verb_answers_mac_false_off_the_surface_and_for_an_empty_list() {
    // Catches: an empty list (a pull request that changes no file) read as a failure or as on
    // the surface, and an answer written only when there is an output file.
    let d = scratch("false");
    let changed = d.join("changed");
    for list in [&b"README.md\0docs/ci-and-release.md\0"[..], b""] {
        std::fs::write(&changed, list).expect("write");
        let output = d.join("output");
        let _ = std::fs::remove_file(&output);
        assert_eq!(
            mac_surface(&changed, Some(&output)).expect("verb"),
            ["mac=false"]
        );
        assert_eq!(
            std::fs::read_to_string(&output).expect("read"),
            "mac=false\n"
        );
        assert_eq!(mac_surface(&changed, None).expect("verb"), ["mac=false"]);
    }
}

/// A list that cannot be read, and an output that cannot be appended to, are failures that name
/// the file; neither is read as `mac=false`.
#[test]
fn an_unreadable_list_or_output_is_a_failure_that_names_the_file() {
    // Catches: a missing list (a `git diff` step that wrote nothing) answered as `mac=false`, so
    // the macOS checks would be skipped, and an answer that was not recorded passing as one.
    let d = scratch("errors");
    let missing = d.join("missing");
    let err = mac_surface(&missing, None).expect_err("missing list");
    assert_eq!(
        err.to_string(),
        format!(
            "read {}: No such file or directory (os error 2)",
            missing.display()
        )
    );
    let changed = d.join("changed");
    std::fs::write(&changed, b"packaging/x\0").expect("write");
    let output = d.join("no-such-dir").join("output");
    let err = mac_surface(&changed, Some(&output)).expect_err("unwritable output");
    assert_eq!(
        err.to_string(),
        format!(
            "append to {}: No such file or directory (os error 2)",
            output.display()
        )
    );
}
