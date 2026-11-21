//! A crate whose library is wholly gated to one system (`#![cfg(target_os = "linux")]` at the top
//! of `src/lib.rs`, as the mount protocol's crates are) is empty elsewhere, so every other target
//! of the crate must be gated the same way, or it does not compile there. A pull request runs on
//! Linux x86_64 alone (AGENTS.md B9), so such a target compiles in every check of the pull request
//! and first fails in the beta stage's macOS job, after the merge. This test is the check before
//! the merge, on Linux, of that one way to break the macOS build.
//!
//! The rule, for each crate under `crates/` whose library root has a crate-level `#![cfg(P)]`:
//! - each root of an integration test, a bench and an example (`tests/*.rs`, `tests/*/main.rs`,
//!   the same under `benches/` and `examples/`, and every `path` of a `[[test]]`, `[[bench]]` or
//!   `[[example]]`) has the same `#![cfg(P)]` line at crate level;
//! - each root of a binary (`src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs`, every `path` of a
//!   `[[bin]]`) has a `#[cfg(not(P))]` item at crate level: the `main` of the other systems (a
//!   binary gated whole would have no `main` there).
//!
//! An `auto*` key set to `false` in the manifest turns that kind's discovery off, as it does for
//! Cargo. Only tracked files are read (`git ls-files`).
//!
//! Catches:
//! - a test, bench or example root of a gated library without the gate, or with it indented
//!   (inside a module, not at crate level), named with the line to add;
//! - a binary root of a gated library with no `main` for the other systems;
//! - a walk that sees nothing: the workspace has gated libraries, and the test names one;
//! - a discovery that misses roots or counts files that are not roots: `tests/support/mod.rs`
//!   and `tests/a/b.rs` are not roots, `tests/a/main.rs` and an explicit `path` are, and
//!   `autotests = false` drops the discovered tests and keeps the explicit ones.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

/// What a target root must carry when its crate's library is gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A test, bench or example: the library's `#![cfg(P)]` line.
    Gated,
    /// A binary: a `#[cfg(not(P))]` item, its `main` for the other systems.
    Bin,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The tracked paths of the repository, by `git ls-files`.
fn tracked() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run git ls-files: {e}"));
    assert!(out.status.success(), "git ls-files failed: {}", out.status);
    String::from_utf8(out.stdout)
        .unwrap_or_else(|e| panic!("git ls-files wrote a path that is not UTF-8: {e}"))
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The crate-level `#![cfg(...)]` line of a library root (unindented, so not one of an inline
/// module), if it has one.
fn gate(lib: &str) -> Option<&str> {
    lib.lines()
        .find(|l| l.starts_with("#![cfg(") && l.trim_end().ends_with(")]"))
        .map(str::trim_end)
}

/// The predicate `P` of a gate `#![cfg(P)]`.
fn predicate(gate: &str) -> &str {
    gate.trim_start_matches("#![cfg(")
        .strip_suffix(")]")
        .unwrap_or(gate)
}

/// Whether `auto<kind>` is not turned off in the manifest's `[package]`.
fn auto(manifest: &toml::Table, kind: &str) -> bool {
    manifest
        .get("package")
        .and_then(|p| p.get(format!("auto{kind}")))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

/// The target roots of a crate other than its library, each with what it must carry: the roots
/// Cargo discovers among `files` (paths relative to the crate's directory), unless the manifest
/// turns that discovery off, and the `path` of every target the manifest lists.
fn roots(files: &[&str], manifest: &toml::Table) -> BTreeMap<String, Kind> {
    let mut found = BTreeMap::new();
    for (dir, key, kind) in [
        ("tests", "tests", Kind::Gated),
        ("benches", "benches", Kind::Gated),
        ("examples", "examples", Kind::Gated),
        ("src/bin", "bins", Kind::Bin),
    ] {
        if !auto(manifest, key) {
            continue;
        }
        for f in files {
            let Some(rest) = f.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) else {
                continue;
            };
            let parts: Vec<&str> = rest.split('/').collect();
            let is_root = match parts.as_slice() {
                [one] => one.ends_with(".rs"),
                [_, main] => *main == "main.rs",
                _ => false,
            };
            if is_root {
                found.insert((*f).to_owned(), kind);
            }
        }
    }
    if auto(manifest, "bins") && files.contains(&"src/main.rs") {
        found.insert("src/main.rs".to_owned(), Kind::Bin);
    }
    for (table, kind) in [
        ("test", Kind::Gated),
        ("bench", Kind::Gated),
        ("example", Kind::Gated),
        ("bin", Kind::Bin),
    ] {
        let listed = manifest.get(table).and_then(toml::Value::as_array);
        for target in listed.into_iter().flatten() {
            if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                found.insert(path.trim_start_matches("./").to_owned(), kind);
            }
        }
    }
    found
}

/// What is wrong with the roots of crate `name` whose library carries `gate`, one finding each.
/// `roots` maps each root's path to what it must carry and its text.
fn findings(name: &str, gate: &str, roots: &BTreeMap<String, (Kind, String)>) -> Vec<String> {
    let has = |text: &str, line: &str| text.lines().any(|l| l.trim_end() == line);
    let other = format!("#[cfg(not({}))]", predicate(gate));
    let mut found = Vec::new();
    for (path, (kind, text)) in roots {
        match kind {
            Kind::Gated if !has(text, gate) => found.push(format!(
                "crates/{name}/{path}: the library of {name} is `{gate}`, so this target does not \
                 compile elsewhere: add the line `{gate}` at its crate level"
            )),
            Kind::Bin if !has(text, &other) => found.push(format!(
                "crates/{name}/{path}: the library of {name} is `{gate}`, so this binary needs a \
                 `main` for elsewhere: an item under `{other}`"
            )),
            _ => {}
        }
    }
    found
}

#[test]
fn every_target_of_a_crate_whose_library_is_gated_to_one_system_is_gated_too() {
    let tracked = tracked();
    let tracked: BTreeSet<&str> = tracked.iter().map(String::as_str).collect();
    let read = |rel: &str| {
        std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };
    let mut gated = Vec::new();
    let mut found = Vec::new();
    for manifest_path in tracked.iter().filter(|p| {
        p.strip_prefix("crates/")
            .and_then(|r| r.strip_suffix("/Cargo.toml"))
            .is_some_and(|name| !name.contains('/'))
    }) {
        let dir = manifest_path.trim_end_matches("/Cargo.toml");
        let name = dir.trim_start_matches("crates/");
        let manifest: toml::Table = read(manifest_path)
            .parse()
            .unwrap_or_else(|e| panic!("parse {manifest_path}: {e}"));
        let lib_path = manifest
            .get("lib")
            .and_then(|l| l.get("path"))
            .and_then(toml::Value::as_str)
            .unwrap_or("src/lib.rs");
        let lib_rel = format!("{dir}/{lib_path}");
        if !tracked.contains(lib_rel.as_str()) {
            continue;
        }
        let lib = read(&lib_rel);
        let Some(gate) = gate(&lib) else {
            continue;
        };
        gated.push(name.to_owned());
        let prefix = format!("{dir}/");
        let files: Vec<&str> = tracked
            .iter()
            .filter_map(|p| p.strip_prefix(prefix.as_str()))
            .collect();
        let texts = roots(&files, &manifest)
            .into_iter()
            .map(|(path, kind)| {
                let text = read(&format!("{dir}/{path}"));
                (path, (kind, text))
            })
            .collect();
        found.extend(findings(name, gate, &texts));
    }
    assert!(
        gated.iter().any(|n| n == "tierstore-tree"),
        "the walk found no gated library where tierstore-tree is one: {gated:?}"
    );
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// Asserts each way a root of a gated library is wrong, the roots that are right pass, and which
/// line of a library is its gate: the crate-level one, not an inline module's, a `cfg_attr` or a
/// doc comment.
#[test]
fn an_ungated_test_an_indented_gate_and_a_binary_with_no_other_main_are_named() {
    let linux = "#![cfg(target_os = \"linux\")]";
    let text = |t: &str| t.to_owned();
    let roots: BTreeMap<String, (Kind, String)> = [
        ("tests/gated.rs", Kind::Gated, text("//! Doc.\n\n#![cfg(target_os = \"linux\")]\n")),
        ("tests/bare.rs", Kind::Gated, text("//! Doc.\n\nfn f() {}\n")),
        (
            "tests/nested/main.rs",
            Kind::Gated,
            text("mod m {\n    #![cfg(target_os = \"linux\")]\n}\n"),
        ),
        ("benches/b.rs", Kind::Gated, text("#![cfg(target_os = \"macos\")]\n")),
        ("src/main.rs", Kind::Bin, text("#[cfg(target_os = \"linux\")]\nfn main() {}\n")),
        (
            "src/bin/both.rs",
            Kind::Bin,
            text(
                "#[cfg(target_os = \"linux\")]\nfn main() {}\n#[cfg(not(target_os = \"linux\"))]\nfn main() {}\n",
            ),
        ),
    ]
    .into_iter()
    .map(|(p, k, t)| (p.to_owned(), (k, t)))
    .collect();
    let add = |path: &str| {
        format!(
            "crates/c/{path}: the library of c is `{linux}`, so this target does not compile \
             elsewhere: add the line `{linux}` at its crate level"
        )
    };
    assert_eq!(
        findings("c", linux, &roots),
        vec![
            add("benches/b.rs"),
            format!(
                "crates/c/src/main.rs: the library of c is `{linux}`, so this binary needs a `main` \
                 for elsewhere: an item under `#[cfg(not(target_os = \"linux\"))]`"
            ),
            add("tests/bare.rs"),
            add("tests/nested/main.rs"),
        ]
    );
    assert_eq!(
        gate("//! A library.\n\n#![cfg(target_os = \"linux\")]\n"),
        Some(linux)
    );
    assert_eq!(
        gate("mod m {\n    #![cfg(target_os = \"linux\")]\n}\n"),
        None
    );
    assert_eq!(gate("#![cfg_attr(test, allow(dead_code))]\n"), None);
    assert_eq!(gate("//! #![cfg(target_os = \"linux\")]\n"), None);
}

/// Asserts the roots Cargo would build: one file or a directory's `main.rs` under each target
/// directory, `src/main.rs`, and listed paths; never a module beside them.
#[test]
fn the_discovered_roots_are_cargos_and_an_auto_key_set_false_keeps_only_listed_ones() {
    let files = [
        "Cargo.toml",
        "src/lib.rs",
        "src/main.rs",
        "src/bin/tool.rs",
        "src/bin/big/main.rs",
        "src/bin/big/part.rs",
        "tests/one.rs",
        "tests/dir/main.rs",
        "tests/dir/part.rs",
        "tests/support/mod.rs",
        "tests/a/b/c.rs",
        "tests/notes.md",
        "benches/bench.rs",
        "examples/ex.rs",
        "other/listed.rs",
    ];
    let manifest = |extra: &str| -> toml::Table {
        format!("[package]\nname = \"c\"\n{extra}\n[[test]]\nname = \"listed\"\npath = \"./other/listed.rs\"\n")
            .parse()
            .unwrap_or_else(|e| panic!("parse: {e}"))
    };
    let all = |pairs: &[(&str, Kind)]| -> BTreeMap<String, Kind> {
        pairs.iter().map(|(p, k)| ((*p).to_owned(), *k)).collect()
    };
    assert_eq!(
        roots(&files, &manifest("")),
        all(&[
            ("benches/bench.rs", Kind::Gated),
            ("examples/ex.rs", Kind::Gated),
            ("other/listed.rs", Kind::Gated),
            ("src/bin/big/main.rs", Kind::Bin),
            ("src/bin/tool.rs", Kind::Bin),
            ("src/main.rs", Kind::Bin),
            ("tests/dir/main.rs", Kind::Gated),
            ("tests/one.rs", Kind::Gated),
        ])
    );
    assert_eq!(
        roots(&files, &manifest("autotests = false\nautobins = false")),
        all(&[
            ("benches/bench.rs", Kind::Gated),
            ("examples/ex.rs", Kind::Gated),
            ("other/listed.rs", Kind::Gated),
        ])
    );
}
