//! The workflow lint: every file in `.github/workflows` is held to the rules in [`lint`], and
//! the lint is shown to refuse each defect by planting it in a workflow that is otherwise
//! clean.

#![allow(dead_code)]

mod lint;
mod yaml;

use std::path::PathBuf;

use lint::{SIGN_IF, lint};

/// The directory of the workflows of this repository.
fn workflows_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .canonicalize()
        .expect("the workflows directory")
}

/// `(file name, text)` of every workflow, sorted by name.
fn workflows() -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(workflows_dir()).expect("list the workflows") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            found.push((name, std::fs::read_to_string(&path).expect("read")));
        }
    }
    found.sort();
    found
}

fn text_of(file: &str) -> String {
    workflows()
        .into_iter()
        .find(|(name, _)| name == file)
        .unwrap_or_else(|| panic!("{file} is not in .github/workflows"))
        .1
}

#[test]
fn every_workflow_of_the_repository_passes_the_lint() {
    // Catches: an unpinned action, a missing version comment, a missing top-level
    // `permissions: {}`, pull_request_target, or a write scope outside the table, in any
    // workflow file.
    let mut problems = Vec::new();
    for (file, text) in workflows() {
        for p in lint(&file, &text) {
            problems.push(format!("{file}: {p}"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_workflows_a_release_needs_are_all_there() {
    // Catches: a workflow deleted, which the lint above would not notice. The names are
    // fixed: the host installer pins `artifacts.yml` as the signer.
    let names: Vec<String> = workflows().into_iter().map(|(n, _)| n).collect();
    for needed in [
        "advisories.yml",
        "artifacts.yml",
        "ci.yml",
        "release.yml",
        "tierstore-site-publish.yml",
        "tierstore-site.yml",
    ] {
        assert!(
            names.iter().any(|n| n == needed),
            "{needed} is missing: {names:?}"
        );
    }
}

#[test]
fn only_artifacts_signs_and_only_on_a_push_to_main() {
    // Catches: the signing moved out of artifacts.yml (the signer identity the installers
    // verify), a second signing job, and a signing job that runs for anything but a push to
    // main.
    for (file, text) in workflows() {
        let signs = text.contains("attest-build-provenance") || text.contains("attest-sbom");
        assert_eq!(signs, file == "artifacts.yml", "{file}");
    }
    let artifacts = text_of("artifacts.yml");
    assert!(
        artifacts.contains(&format!("    if: {SIGN_IF}")),
        "the attest job's condition"
    );
    let release = text_of("release.yml");
    assert!(
        !release.contains("cargo build"),
        "release.yml builds nothing"
    );
    assert!(!release.contains("--release"), "release.yml builds nothing");
}

/// A workflow that is clean under the lint, for the planted defects below to spoil.
const CLEAN: &str = "name: t
on:
  push:
    branches: [main]
permissions: {}
jobs:
  build:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
";

fn planted(replace: &str, with: &str) -> Vec<String> {
    assert!(
        CLEAN.contains(replace),
        "{replace:?} is not in the clean workflow"
    );
    lint("t.yml", &CLEAN.replace(replace, with))
}

#[test]
fn the_clean_workflow_is_clean() {
    // Catches: a lint that refuses everything, which would make the planted defects below
    // prove nothing.
    assert_eq!(lint("t.yml", CLEAN), Vec::<String>::new());
}

#[test]
fn an_action_not_pinned_to_a_full_sha_is_refused() {
    // Catches: a tag, a branch, a short SHA, an upper-case SHA, and no pin at all.
    for pin in [
        "v4",
        "main",
        "3d3c42e",
        "3D3C42E5AAC5BA805825DA76410C181273BA90B1",
        "3d3c42e5aac5ba805825da76410c181273ba90b10",
    ] {
        let problems = planted(
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
            &format!("actions/checkout@{pin}"),
        );
        assert_eq!(problems.len(), 1, "{pin}: {problems:?}");
        assert!(problems[0].contains("40-digit"), "{problems:?}");
    }
    let problems = planted(
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        "actions/checkout",
    );
    assert!(problems[0].contains("is not pinned"), "{problems:?}");
    let problems = planted(
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        "./local",
    );
    assert!(problems[0].contains("is not pinned"), "{problems:?}");
    let problems = planted(
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
        "./local@3d3c42e5aac5ba805825da76410c181273ba90b1",
    );
    assert!(problems[0].contains("local path"), "{problems:?}");
}

#[test]
fn a_pin_without_a_version_comment_is_refused() {
    // Catches: a pin nobody can read the version of, and a comment that is not a version.
    for comment in ["", " # latest", " #v7", " # 7.0.1"] {
        let problems = planted(" # v7.0.1", comment);
        assert_eq!(problems.len(), 1, "{comment:?}: {problems:?}");
        assert!(problems[0].contains("version comment"), "{problems:?}");
    }
}

#[test]
fn a_workflow_without_empty_top_level_permissions_is_refused() {
    // Catches: a missing `permissions`, one that grants something, and one that is not a map.
    for permissions in [
        "",
        "permissions:\n  contents: read\n",
        "permissions: write-all\n",
    ] {
        let problems = planted("permissions: {}\n", permissions);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("no top-level `permissions: {}`")),
            "{permissions:?}: {problems:?}"
        );
    }
}

#[test]
fn pull_request_target_and_self_hosted_runners_are_refused() {
    // Catches: the trigger in each of its shapes, and a runner label that is self-hosted.
    for on in [
        "on:\n  pull_request_target:\n",
        "on: pull_request_target\n",
        "on: [push, pull_request_target]\n",
        "on: { pull_request_target: {} }\n",
    ] {
        let problems = planted("on:\n  push:\n    branches: [main]\n", on);
        assert!(
            problems.iter().any(|p| p.contains("pull_request_target")),
            "{on:?}: {problems:?}"
        );
    }
    let problems = planted("ubuntu-24.04", "[self-hosted, linux]");
    assert!(
        problems.iter().any(|p| p.contains("self-hosted")),
        "{problems:?}"
    );
}

#[test]
fn a_write_scope_outside_the_table_is_refused() {
    // Catches: id-token or attestations written by a job that is not the signing one, in a
    // file that is not artifacts.yml, contents written by a build job, a scope given as a
    // bare string, and `read-all`-style shorthand that hides what a job holds.
    for scope in [
        "id-token",
        "attestations",
        "contents",
        "packages",
        "actions",
    ] {
        let problems = planted("contents: read", &format!("{scope}: write"));
        assert_eq!(problems.len(), 1, "{scope}: {problems:?}");
        assert!(problems[0].contains("is not allowed"), "{problems:?}");
    }
    let problems = planted(
        "permissions:\n      contents: read\n",
        "permissions: write-all\n",
    );
    assert!(problems[0].contains("not a mapping"), "{problems:?}");
}

#[test]
fn the_signing_job_signs_only_under_its_exact_condition() {
    // Catches: the attest job of artifacts.yml with a condition that admits a pull request,
    // a tag or any branch, and with no condition at all.
    let signing = |condition: &str| {
        format!(
            "name: t
on:
  push:
    branches: [main]
permissions: {{}}
jobs:
  attest:
{condition}    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
    steps:
      - run: echo
"
        )
    };
    let good = signing(&format!("    if: {SIGN_IF}\n"));
    assert_eq!(lint("artifacts.yml", &good), Vec::<String>::new());
    // The same job in another file is refused.
    assert_eq!(lint("ci.yml", &good).len(), 2);
    for condition in [
        "",
        "    if: ${{ github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.event_name == 'push' }}\n",
        "    if: ${{ always() }}\n",
    ] {
        let problems = lint("artifacts.yml", &signing(condition));
        assert_eq!(problems.len(), 2, "{condition:?}: {problems:?}");
        assert!(
            problems.iter().all(|p| p.contains("needs the exact `if:")),
            "{problems:?}"
        );
    }
}

#[test]
fn a_document_the_lint_cannot_evaluate_is_refused() {
    // Catches: a tag, a second document and a file that does not parse being read as clean.
    for text in [
        "name: !!str t\npermissions: {}\njobs: {}\n",
        "permissions: {}\njobs: {}\n---\npermissions: {}\n",
        "permissions: {\njobs: [\n",
        "",
    ] {
        assert!(!lint("t.yml", text).is_empty(), "{text:?}");
    }
}
