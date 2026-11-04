use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::testdir::TestDir;

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-licenses-unit"), name)
}

/// The directory `dir/<name>` holding `files`, and the path of the `Cargo.toml` in it.
fn krate(dir: &Path, name: &str, files: &[(&str, &str)]) -> String {
    let d = dir.join(name);
    std::fs::create_dir_all(&d).expect("crate dir");
    for (file, text) in files {
        std::fs::write(d.join(file), text).expect("file");
    }
    let manifest = d.join("Cargo.toml");
    std::fs::write(&manifest, "[package]\n").expect("manifest");
    manifest.display().to_string()
}

/// A package of `cargo metadata`: from a registry, or of the workspace.
fn pkg(name: &str, version: &str, workspace: bool, manifest: &str) -> Value {
    json!({
        "id": format!("{name} {version}"),
        "name": name,
        "version": version,
        "source": if workspace { Value::Null } else { json!("registry+crates.io") },
        "manifest_path": manifest,
        "license": "MIT",
        "license_file": null,
        "repository": null,
    })
}

/// A dependency edge of a resolve node, of the kinds given (`null` is a normal dependency).
fn dep(id: &str, kinds: &[Value]) -> Value {
    let kinds: Vec<Value> = kinds
        .iter()
        .map(|k| json!({"kind": k, "target": null}))
        .collect();
    json!({"pkg": id, "dep_kinds": kinds})
}

fn node(id: &str, deps: Vec<Value>) -> Value {
    json!({"id": id, "deps": deps})
}

fn metadata(packages: Vec<Value>, nodes: Vec<Value>) -> Value {
    json!({"packages": packages, "resolve": {"nodes": nodes, "root": null}})
}

const NORMAL: Value = Value::Null;

#[test]
fn the_notice_of_one_crate_is_exactly_its_licence_expression_source_and_text() {
    // Asserts the whole text for one crate: the header, the separator, the section with its
    // licence expression, repository and the licence file with its trailing blank lines cut,
    // and that a file that is no licence (README) is not in it.
    let dir = scratch("exact");
    let manifest = krate(
        &dir,
        "x",
        &[("LICENSE", "MIT text\n\n"), ("README.md", "readme")],
    );
    let mut x = pkg("x", "1.0.0", false, &manifest);
    x["repository"] = json!("https://example.com/x");
    let meta = metadata(
        vec![pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"), x],
        vec![
            node("app 0.1.0", vec![dep("x 1.0.0", &[NORMAL])]),
            node("x 1.0.0", vec![]),
        ],
    );
    let bar = "=".repeat(64);
    assert_eq!(
        notice(&meta, "app").expect("notice"),
        format!(
            "Third-party licences of app\n\n\
             The crates below are linked into the binary. Each is used under the licence it declares;\n\
             the licence texts it ships follow it.\n\
             \n{bar}\n\n\
             x 1.0.0\nlicence: MIT\nsource: https://example.com/x\n\
             \n--- LICENSE ---\nMIT text\n"
        )
    );
}

#[test]
fn only_the_crates_the_binary_links_are_listed_once_each_in_name_and_version_order() {
    // Asserts which crates are in the notice: normal dependencies, also through a workspace
    // crate and through an edge that is a build and a normal one, a cycle ending; and not
    // the workspace crates, a dev dependency, a build dependency or a crate nothing reaches.
    // Two versions of one crate are two sections, in version order.
    let dir = scratch("reach");
    let m = |name: &str, files: &[(&str, &str)]| krate(&dir, name, files);
    let packages = vec![
        pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
        pkg("ws", "0.1.0", true, "/nowhere/Cargo.toml"),
        pkg(
            "a",
            "1.0.0",
            false,
            &m(
                "a",
                &[
                    ("LICENSE-MIT", "MIT of a"),
                    ("LICENSE-APACHE", "Apache of a"),
                    ("build.rs", "x"),
                ],
            ),
        ),
        pkg(
            "b",
            "2.0.0",
            false,
            &m("b", &[("license.txt", "lower case licence of b")]),
        ),
        pkg(
            "b",
            "1.0.0",
            false,
            &m(
                "b1",
                &[("COPYING", "copying of b1"), ("NOTICE", "notice of b1")],
            ),
        ),
        pkg("dev", "1.0.0", false, "/nowhere/Cargo.toml"),
        pkg("build", "1.0.0", false, "/nowhere/Cargo.toml"),
        pkg("other", "1.0.0", false, "/nowhere/Cargo.toml"),
    ];
    let nodes = vec![
        node(
            "app 0.1.0",
            vec![
                dep("a 1.0.0", &[NORMAL]),
                dep("dev 1.0.0", &[json!("dev")]),
                dep("build 1.0.0", &[json!("build")]),
                dep("ws 0.1.0", &[NORMAL]),
            ],
        ),
        node(
            "ws 0.1.0",
            vec![dep("b 1.0.0", &[NORMAL]), dep("a 1.0.0", &[NORMAL])],
        ),
        node("a 1.0.0", vec![dep("b 2.0.0", &[json!("build"), NORMAL])]),
        node("b 2.0.0", vec![]),
        node("b 1.0.0", vec![]),
    ];
    let text = notice(&metadata(packages, nodes), "app").expect("notice");
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} not in {text}"))
    };
    assert!(at("\na 1.0.0\n") < at("\nb 1.0.0\n"));
    assert!(at("\nb 1.0.0\n") < at("\nb 2.0.0\n"));
    assert!(at("--- LICENSE-APACHE ---\nApache of a\n") < at("--- LICENSE-MIT ---\nMIT of a\n"));
    assert!(text.contains("--- license.txt ---\nlower case licence of b\n"));
    assert!(text.contains("--- COPYING ---\ncopying of b1\n"));
    assert!(text.contains("--- NOTICE ---\nnotice of b1\n"));
    assert!(!text.contains("build.rs"), "{text}");
    for absent in [
        "ws 0.1.0",
        "app 0.1.0",
        "dev 1.0.0",
        "build 1.0.0",
        "other 1.0.0",
    ] {
        assert!(
            !text.contains(&format!("\n{absent}\n")),
            "{absent} is listed: {text}"
        );
    }
    assert_eq!(text.matches("\nlicence: MIT\n").count(), 3);
    assert!(!text.contains("source:"), "no repository was given: {text}");
}

#[test]
fn a_crate_that_declares_no_licence_is_refused_and_every_such_crate_is_named() {
    // Catches: a binary released with a crate nothing says the licence of: each such crate is
    // named, in one error, whether or not it ships a licence file. A crate that gives only
    // `license_file` has declared its licence.
    let dir = scratch("refused");
    let mut with_file = pkg(
        "withfile",
        "2.0.0",
        false,
        &krate(&dir, "withfile", &[("LICENSE", "t")]),
    );
    with_file["license"] = Value::Null;
    let mut without = pkg(
        "without",
        "1.0.0",
        false,
        &krate(&dir, "without", &[("README", "x")]),
    );
    without["license"] = Value::Null;
    let by_file = {
        let mut p = pkg(
            "byfile",
            "1.0.0",
            false,
            &krate(&dir, "byfile", &[("COPYING", "t")]),
        );
        p["license"] = Value::Null;
        p["license_file"] = json!("COPYING");
        p
    };
    let ids = ["withfile 2.0.0", "without 1.0.0", "byfile 1.0.0"];
    let meta = metadata(
        vec![
            pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
            with_file,
            without,
            by_file,
        ],
        vec![
            node("app 0.1.0", ids.map(|id| dep(id, &[NORMAL])).to_vec()),
            node("withfile 2.0.0", vec![]),
            node("without 1.0.0", vec![]),
            node("byfile 1.0.0", vec![]),
        ],
    );
    assert_eq!(
        notice(&meta, "app"),
        Err(Error::Refused(vec![
            "withfile 2.0.0: declares no licence".to_owned(),
            "without 1.0.0: declares no licence".to_owned(),
        ]))
    );
}

#[test]
fn a_crate_that_ships_no_licence_file_is_listed_with_its_declared_licence_and_says_so() {
    // Asserts the section of a crate with no licence file (its declared licence and a line
    // saying it ships none) and that a directory called LICENSE is not taken for a file.
    let dir = scratch("no-file");
    let bare = krate(&dir, "bare", &[("README", "x")]);
    let dir_named = krate(&dir, "dirnamed", &[]);
    std::fs::create_dir(dir.join("dirnamed/LICENSE")).expect("directory");
    let meta = metadata(
        vec![
            pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
            pkg("bare", "1.0.0", false, &bare),
            pkg("dirnamed", "1.0.0", false, &dir_named),
        ],
        vec![
            node(
                "app 0.1.0",
                vec![
                    dep("bare 1.0.0", &[NORMAL]),
                    dep("dirnamed 1.0.0", &[NORMAL]),
                ],
            ),
            node("bare 1.0.0", vec![]),
            node("dirnamed 1.0.0", vec![]),
        ],
    );
    let none = "This crate ships no licence file; its licence is the one declared above.\n";
    let text = notice(&meta, "app").expect("notice");
    assert!(
        text.contains(&format!("\nbare 1.0.0\nlicence: MIT\n\n{none}")),
        "{text}"
    );
    assert!(
        text.ends_with(&format!("\ndirnamed 1.0.0\nlicence: MIT\n\n{none}")),
        "{text}"
    );
    assert!(!text.contains("---"), "{text}");
}

#[test]
fn a_procedural_macro_and_what_only_it_needs_are_not_listed() {
    // Catches: a compiler-side crate (asn1-rs-impl in this repository's tree) named as linked,
    // with its own dependencies; a crate the macro and a linked crate both need stays listed,
    // and a crate with other kinds of targets is not taken for a macro.
    let dir = scratch("macro");
    let m = |name: &str| krate(&dir, name, &[("LICENSE", name)]);
    let mut mac = pkg("mac", "1.0.0", false, &m("mac"));
    mac["targets"] = json!([{"kind": ["proc-macro"]}]);
    let mut lib = pkg("lib", "1.0.0", false, &m("lib"));
    lib["targets"] = json!([{"kind": ["lib"]}, {"kind": ["custom-build"]}, {}]);
    let meta = metadata(
        vec![
            pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
            mac,
            lib,
            pkg("syn", "1.0.0", false, &m("syn")),
            pkg("shared", "1.0.0", false, &m("shared")),
        ],
        vec![
            node(
                "app 0.1.0",
                vec![dep("mac 1.0.0", &[NORMAL]), dep("lib 1.0.0", &[NORMAL])],
            ),
            node(
                "mac 1.0.0",
                vec![dep("syn 1.0.0", &[NORMAL]), dep("shared 1.0.0", &[NORMAL])],
            ),
            node("lib 1.0.0", vec![dep("shared 1.0.0", &[NORMAL])]),
            node("shared 1.0.0", vec![]),
            node("syn 1.0.0", vec![]),
        ],
    );
    let text = notice(&meta, "app").expect("notice");
    for (name, listed) in [
        ("lib", true),
        ("shared", true),
        ("mac", false),
        ("syn", false),
    ] {
        assert_eq!(
            text.contains(&format!("\n{name} 1.0.0\n")),
            listed,
            "{name}: {text}"
        );
    }
}

#[test]
fn malformed_metadata_is_refused_with_what_is_missing() {
    // Catches: metadata that is not the output of `cargo metadata` (no resolve because of
    // --no-deps, an edge without kinds, a package listed in no node) read as an empty tree
    // and released with an empty notice.
    let dir = scratch("malformed");
    let ok = krate(&dir, "x", &[("LICENSE", "t")]);
    let app = || pkg("app", "0.1.0", true, "/nowhere/Cargo.toml");
    let x = || pkg("x", "1.0.0", false, &ok);
    let edge = || node("app 0.1.0", vec![dep("x 1.0.0", &[NORMAL])]);
    let x_node = || node("x 1.0.0", vec![]);
    let missing = |what: &str, key: &str| {
        Error::Failed(format!(
            "the metadata: {what} has no `{key}` of the expected type"
        ))
    };
    let rows: Vec<(&str, &str, Value, Error)> = vec![
        (
            "not an object",
            "app",
            json!([]),
            missing("the metadata", "packages"),
        ),
        (
            "no resolve (--no-deps)",
            "app",
            json!({"packages": [app()]}),
            missing("the metadata", "resolve"),
        ),
        (
            "resolve without nodes",
            "app",
            json!({"packages": [app()], "resolve": null}),
            missing("resolve", "nodes"),
        ),
        (
            "a package without an id",
            "app",
            metadata(vec![json!({"name": "app"})], vec![]),
            missing("a package", "id"),
        ),
        (
            "a node without an id",
            "app",
            metadata(vec![app()], vec![json!({"deps": []})]),
            missing("a resolve node", "id"),
        ),
        (
            "no workspace package of that name",
            "missing",
            metadata(vec![app(), x()], vec![edge(), x_node()]),
            Error::Failed("the metadata has no workspace package named missing".to_owned()),
        ),
        (
            "no node for the root",
            "app",
            metadata(vec![app()], vec![]),
            Error::Failed("the metadata: no resolve node for app 0.1.0".to_owned()),
        ),
        (
            "a node without deps",
            "app",
            metadata(vec![app()], vec![json!({"id": "app 0.1.0"})]),
            missing("app 0.1.0", "deps"),
        ),
        (
            "an edge without kinds",
            "app",
            metadata(
                vec![app()],
                vec![node("app 0.1.0", vec![json!({"pkg": "x 1.0.0"})])],
            ),
            missing("a dependency", "dep_kinds"),
        ),
        (
            "an edge without a package",
            "app",
            metadata(
                vec![app()],
                vec![node(
                    "app 0.1.0",
                    vec![json!({"dep_kinds": [{"kind": null}]})],
                )],
            ),
            missing("a dependency", "pkg"),
        ),
        (
            "an edge to a package that is not listed",
            "app",
            metadata(vec![app()], vec![edge(), x_node()]),
            Error::Failed("the metadata: no package x 1.0.0".to_owned()),
        ),
        (
            "a package without a name",
            "app",
            metadata(
                vec![
                    app(),
                    json!({"id": "x 1.0.0", "source": "r", "version": "1"}),
                ],
                vec![edge(), x_node()],
            ),
            missing("x 1.0.0", "name"),
        ),
        (
            "a package without a version",
            "app",
            metadata(
                vec![app(), json!({"id": "x 1.0.0", "source": "r", "name": "x"})],
                vec![edge(), x_node()],
            ),
            missing("x 1.0.0", "version"),
        ),
        (
            "a package without a manifest path",
            "app",
            metadata(
                vec![app(), {
                    let mut p = x();
                    p.as_object_mut().expect("object").remove("manifest_path");
                    p
                }],
                vec![edge(), x_node()],
            ),
            missing("x 1.0.0", "manifest_path"),
        ),
    ];
    for (what, root, meta, want) in rows {
        assert_eq!(notice(&meta, root), Err(want), "{what}");
    }
    assert!(notice(&metadata(vec![app(), x()], vec![edge(), x_node()]), "app").is_ok());
}

#[test]
fn a_crate_directory_that_cannot_be_read_is_an_error_naming_it() {
    // Catches: a crate whose sources are not where the metadata says (a registry that was
    // not fetched) skipped, so its licence is missing from the notice without a word.
    let meta = metadata(
        vec![
            pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
            pkg("x", "1.0.0", false, "/nowhere/x/Cargo.toml"),
        ],
        vec![
            node("app 0.1.0", vec![dep("x 1.0.0", &[NORMAL])]),
            node("x 1.0.0", vec![]),
        ],
    );
    let error = notice(&meta, "app").expect_err("unreadable");
    assert!(
        matches!(&error, Error::Spawn { what, .. } if what == "read /nowhere/x/"),
        "{error:?}"
    );
}

#[test]
fn licence_file_names_are_matched_by_their_beginning_in_any_case() {
    // Asserts which names count as a licence file: the usual ones and their cases, not a
    // file that merely mentions a licence later in its name.
    for name in [
        "LICENSE",
        "LICENSE-MIT",
        "license.md",
        "Licence",
        "COPYING.txt",
        "NOTICE",
        "UNLICENSE",
    ] {
        assert!(is_licence_file(name), "{name}");
    }
    for name in ["README", "build.rs", "Cargo.toml", "MY-LICENSE", "src"] {
        assert!(!is_licence_file(name), "{name}");
    }
}

#[test]
fn the_verb_writes_the_notice_once_and_refuses_what_it_cannot_read() {
    // Catches: an existing notice overwritten, metadata that is not JSON or not there taken
    // as empty, and a refused notice leaving a file behind.
    let dir = scratch("verb");
    let manifest = krate(&dir, "x", &[("LICENSE", "MIT text")]);
    let meta = metadata(
        vec![
            pkg("app", "0.1.0", true, "/nowhere/Cargo.toml"),
            pkg("x", "1.0.0", false, &manifest),
        ],
        vec![
            node("app 0.1.0", vec![dep("x 1.0.0", &[NORMAL])]),
            node("x 1.0.0", vec![]),
        ],
    );
    let file = dir.join("metadata.json");
    std::fs::write(&file, meta.to_string()).expect("metadata");
    let out = dir.join("NOTICE.txt");
    assert_eq!(
        licenses(&file, "app", &out),
        Ok(vec![format!("wrote {}", out.display())])
    );
    let written = std::fs::read_to_string(&out).expect("notice");
    assert!(written.contains("\nx 1.0.0\nlicence: MIT\n"), "{written}");
    let again = licenses(&file, "app", &out).expect_err("exists");
    assert!(
        again
            .to_string()
            .starts_with(&format!("write {}: ", out.display())),
        "{again}"
    );
    assert_eq!(std::fs::read_to_string(&out).expect("notice"), written);
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "not json").expect("bad");
    let e = licenses(&bad, "app", &dir.join("o1")).expect_err("not json");
    assert!(
        e.to_string()
            .starts_with(&format!("{} is not JSON: ", bad.display())),
        "{e}"
    );
    let e = licenses(&dir.join("absent.json"), "app", &dir.join("o2")).expect_err("absent");
    assert!(e.to_string().starts_with("read "), "{e}");
    let refused = dir.join("refused.json");
    std::fs::write(
        &refused,
        metadata(vec![pkg("app", "0.1.0", true, "/n")], vec![]).to_string(),
    )
    .expect("refused");
    licenses(&refused, "app", &dir.join("o3")).expect_err("no node");
    assert!(!dir.join("o3").exists());
}
