//! The workflow lint: every file in `.github/workflows` is held to the rules in [`lint`], and
//! the lint is shown to refuse each defect by planting it in a workflow that is otherwise
//! clean.

#![allow(dead_code)]

mod lint;
mod yaml;

use std::path::PathBuf;

use lint::{
    ARTIFACTS_GROUP, RELEASE_EXEMPT, RELEASE_GROUP, SIGN_IF, artifacts_problems,
    concurrency_problems, discovery_problems, lint, release_problems,
};

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

#[test]
fn release_yml_is_started_by_the_two_workflows_and_by_hand_and_nothing_else() {
    // Catches: any change to release.yml that breaks the chain below: a trigger added or lost,
    // a guard dropped, the group made cancelling, a write scope or a build step where it must
    // not be, the plan job given more than read access.
    assert_eq!(
        release_problems(&text_of("release.yml")),
        Vec::<String>::new()
    );
}

/// `(text to replace once, its replacement, what the problem must name)`.
const RELEASE_DEFECTS: &[(&str, &str, &str)] = &[
    (
        "on:\n  workflow_run:",
        "on:\n  push:\n  workflow_run:",
        "triggers",
    ),
    ("  workflow_dispatch:", "  workflow_call:", "triggers"),
    (
        "workflows: [ci, artifacts]",
        "workflows: [ci]",
        "workflow_run.workflows",
    ),
    (
        "workflows: [ci, artifacts]",
        "workflows: [artifacts]",
        "workflow_run.workflows",
    ),
    (
        "types: [completed]",
        "types: [requested]",
        "workflow_run.types",
    ),
    (
        "branches: [main]",
        "branches: [main, dev]",
        "workflow_run.branches",
    ),
    (
        "cancel-in-progress: false",
        "cancel-in-progress: true",
        "cancel-in-progress",
    ),
    ("group: release-", "group: rel-", "group"),
    (
        "github.event.workflow_run.conclusion == 'success'",
        "true",
        "plan: `if` lacks github.event.workflow_run.conclusion == 'success'",
    ),
    (
        "github.event.workflow_run.event == 'push'",
        "true",
        "plan: `if` lacks github.event.workflow_run.event == 'push'",
    ),
    (
        "github.event.workflow_run.head_branch == 'main'",
        "true",
        "plan: `if` lacks github.event.workflow_run.head_branch == 'main'",
    ),
    (
        "github.event.workflow_run.head_repository.full_name == github.repository",
        "true",
        "plan: `if` lacks github.event.workflow_run.head_repository.full_name == github.repository",
    ),
    (
        "actions: read\n    outputs:",
        "actions: write\n    outputs:",
        "plan: permissions",
    ),
    (
        "    permissions:\n      contents: read\n      actions: read\n    outputs:",
        "    permissions:\n      contents: write\n      actions: read\n    outputs:",
        "plan: permissions",
    ),
    ("needs: [plan]\n", "needs: []\n", "verify: needs"),
    ("needs: [plan, verify]", "needs: [verify]", "publish: needs"),
    ("needs: [plan, publish]", "needs: [plan]", "gamma: needs"),
    (
        "needs: [plan, publish]\n    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}",
        "needs: [plan, publish]\n    if: ${{ github.ref == 'refs/heads/main' }}",
        "gamma: `if` is not",
    ),
    (
        "--service tierstore",
        "--service kbf",
        "gamma: deploy-farm is not run with --service tierstore",
    ),
    (
        "deploy-farm --service",
        "deploy-farm --min-up-storage 1 --service",
        "gamma: deploy-farm is passed --min-up",
    ),
    (
        "cargo xtask deploy-farm",
        "cargo xtask deploy",
        "gamma: no step runs `cargo xtask deploy-farm`",
    ),
    (
        " && vars.KBF_COORDINATOR_URL != '' }}\n    runs-on",
        " }}\n    runs-on",
        "gamma: `if` is not",
    ),
    (
        "vars.KBF_COORDINATOR_URL != ''",
        "vars.KBF_COORDINATOR_URL == ''",
        "gamma: `if` is not",
    ),
    (
        "path: kbf\n          persist-credentials: false",
        "path: kbf\n          persist-credentials: true",
        "gamma: the kbf checkout persists credentials",
    ),
    (
        "--version \"${TAG#v}+${COMMIT:0:12}\"",
        "--version \"${TAG#v}\"",
        "gamma: deploy-farm is not passed --version",
    ),
    (
        "TAG: ${{ needs.plan.outputs.tag }}\n          COMMIT: ${{ needs.plan.outputs.commit }}\n        run: cargo xtask deploy-farm",
        "TAG: ${{ github.ref_name }}\n          COMMIT: ${{ needs.plan.outputs.commit }}\n        run: cargo xtask deploy-farm",
        "gamma: env TAG",
    ),
    (
        "COMMIT: ${{ needs.plan.outputs.commit }}\n        run: cargo xtask deploy-farm",
        "COMMIT: ${{ github.sha }}\n        run: cargo xtask deploy-farm",
        "gamma: env COMMIT",
    ),
    // Pins what the lint already held (kbf's flag is refused); this row is green on arrival.
    (
        "deploy-farm --service",
        "deploy-farm --require-coordinator-current --service",
        "gamma: deploy-farm is passed --require-coordinator-current",
    ),
    ("    environment: release\n", "", "gamma: environment"),
    (
        "      contents: read\n      id-token: write\n",
        "      contents: write\n      id-token: write\n",
        "gamma: permissions",
    ),
    (
        "      contents: read\n      id-token: write\n",
        "      contents: read\n      id-token: write\n      packages: write\n",
        "gamma: permissions",
    ),
    (
        "needs.plan.outputs.release == 'true'",
        "true",
        "release == 'true'",
    ),
    ("vars.RELEASE_FREEZE", "vars.OTHER", "RELEASE_FREEZE"),
    (
        "jobs:\n  plan:",
        "jobs:\n  tag:\n    runs-on: ubuntu-24.04\n  plan:",
        "jobs",
    ),
    (
        "release check-binary",
        "release check-bin",
        "verify: no step runs `cargo xtask release check-binary`",
    ),
    (
        "      TAG: ${{ needs.plan.outputs.tag }}\n    outputs:",
        "    outputs:",
        "verify: env TAG",
    ),
    (
        "--tag \"$TAG\" --commit \"$COMMIT\" --scratch",
        "--scratch",
        "verify: the check-binary step",
    ),
];

#[test]
fn each_defect_in_the_release_chain_is_named_by_the_lint() {
    // Catches: a pin of release.yml that holds nothing, because the file passes whether or
    // not the thing it pins is there.
    let text = text_of("release.yml");
    for (replace, with, names) in RELEASE_DEFECTS {
        assert!(text.contains(replace), "{replace:?} is not in release.yml");
        let problems = release_problems(&text.replace(replace, with));
        assert!(
            problems.iter().any(|p| p.contains(names)),
            "{replace:?} -> {with:?} must name {names:?}: {problems:?}"
        );
    }
}

#[test]
fn a_build_or_a_signing_step_in_release_yml_is_named_by_the_lint() {
    // Catches: release.yml compiling the product, installing a tool or signing: every `cargo`
    // there must be `cargo xtask`, and the attest actions are not used.
    let text = text_of("release.yml");
    for step in [
        "run: cargo build --release",
        "run: cargo xtask package && cargo build",
        "run: cargo xtask package",
        "run: cargo install cargo-deny",
        "uses: actions/attest-build-provenance@3d3c42e5aac5ba805825da76410c181273ba90b1 # v4.0.0",
        "uses: actions/attest-sbom@3d3c42e5aac5ba805825da76410c181273ba90b1 # v4.0.0",
    ] {
        let spoiled = format!("{text}      - {step}\n");
        let problems = release_problems(&spoiled);
        assert!(
            problems.iter().any(|p| p.contains("builds or signs")),
            "{step}: {problems:?}"
        );
    }
}

#[test]
fn contents_write_is_held_by_publish_and_only_under_the_release_condition() {
    // Catches: the write scope moving to another job of release.yml, or publish holding it
    // under a condition that lets a run without a plan through.
    let text = text_of("release.yml");
    let spoiled = text.replacen(
        "    permissions:\n      contents: read\n      actions: read\n    outputs:",
        "    permissions:\n      contents: write\n      actions: read\n    outputs:",
        1,
    );
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p.contains("job plan: `contents: write` is not allowed")),
        "plan with contents: write"
    );
    let spoiled = text.replace("needs.plan.outputs.release == 'true' && ", "");
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p.contains("job publish: `contents: write` needs the exact `if:")),
        "publish without the plan's answer in its condition"
    );
}

#[test]
fn artifacts_yml_builds_under_the_version_the_release_will_tag() {
    // Catches: any change to artifacts.yml that lets a binary be built without the version
    // computed for it (the v0.3.1 binary said 0.3.0): the `version` job gone or no longer the
    // `cargo xtask release version` the release plan shares, a build job that does not wait for
    // it or does not pass it to the build, a build-commit step that does not check it.
    assert_eq!(
        artifacts_problems(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
}

/// `(text to replace, its replacement, what the problem must name)`.
const ARTIFACTS_DEFECTS: &[(&str, &str, &str)] = &[
    ("needs: [version]", "needs: []", "needs"),
    (
        "TIERSTORE_BUILD_VERSION_OVERRIDE: ${{ needs.version.outputs.version }}",
        "TIERSTORE_BUILD_VERSION_OVERRIDE: ''",
        "TIERSTORE_BUILD_VERSION_OVERRIDE",
    ),
    (
        " --version \"$TIERSTORE_BUILD_VERSION_OVERRIDE\"",
        "",
        "build-commit",
    ),
    (
        "release version --repo",
        "release plan --repo",
        "release version",
    ),
    (" --version-file VERSION", "", "--version-file"),
    (
        "version: ${{ steps.version.outputs.version }}",
        "version: ''",
        "output version",
    ),
];

#[test]
fn each_defect_in_the_version_chain_of_artifacts_yml_is_named_by_the_lint() {
    // Catches: a pin of artifacts.yml that holds nothing, because the file passes whether or
    // not the thing it pins is there.
    let text = text_of("artifacts.yml");
    for (replace, with, names) in ARTIFACTS_DEFECTS {
        assert!(
            text.contains(replace),
            "{replace:?} is not in artifacts.yml"
        );
        let problems = artifacts_problems(&text.replace(replace, with));
        assert!(
            problems.iter().any(|p| p.contains(names)),
            "{replace:?} -> {with:?} must name {names:?}: {problems:?}"
        );
    }
}

#[test]
fn every_workflow_that_tags_publishes_or_deploys_is_in_the_one_release_group() {
    // Catches: a workflow that can tag, publish or deploy (found by what it does, not by a
    // list of names) outside the shared group, so that two release actions overlap, or one
    // that cancels a running release action.
    assert_eq!(
        discovery_problems(&workflows(), RELEASE_EXEMPT),
        Vec::<String>::new()
    );
    assert_eq!(
        concurrency_problems("release.yml", &text_of("release.yml")),
        Vec::<String>::new()
    );
}

/// A workflow with `step` in a job of its own, and no concurrency block.
fn with_step(step: &str) -> (String, String) {
    (
        "new.yml".to_owned(),
        format!(
            "name: new\non:\n  workflow_dispatch:\npermissions: {{}}\njobs:\n  go:\n    runs-on: ubuntu-24.04\n    permissions:\n      contents: read\n    steps:\n      - run: {step}\n"
        ),
    )
}

/// An empty exempt list, for checks on a single made-up workflow: the real list names
/// artifacts.yml, which such a set does not hold.
const NO_EXEMPT: &[(&str, &str)] = &[];

#[test]
fn a_new_workflow_that_tags_publishes_or_deploys_outside_the_group_is_named() {
    // Catches: discovery that finds only the files it was told about. Each of these must be
    // named, by file, when it has no group; `id-token` with attest alone is the artifacts
    // workflow and must not be.
    for step in [
        "gh release create v1 --repo x/y",
        "git tag v1 && git push origin v1",
        "git push origin refs/tags/v1",
        "cargo xtask deploy-farm --service tierstore",
        "./farm-deploy.sh",
    ] {
        let problems = discovery_problems(&[with_step(step)], NO_EXEMPT);
        assert!(
            problems.iter().any(|p| p.contains("new.yml")),
            "{step}: {problems:?}"
        );
    }
    let (name, text) = with_step("gh release create v1");
    let write = text
        .replace("contents: read", "contents: write")
        .replace("gh release create v1", "echo");
    let problems = discovery_problems(&[(name, write)], NO_EXEMPT);
    assert!(
        problems.iter().any(|p| p.contains("new.yml")),
        "{problems:?}"
    );
    let (name, text) = with_step("echo build");
    let attest = text.replace(
        "contents: read",
        "contents: read\n      id-token: write\n      attestations: write",
    );
    assert_eq!(
        discovery_problems(&[(name, attest)], NO_EXEMPT),
        Vec::<String>::new()
    );
}

#[test]
fn a_release_path_workflow_in_the_group_or_exempt_with_a_reason_is_accepted_and_the_rest_not() {
    // Catches: a group that is not checked once the workflow is found, an exempt entry that
    // needs no reason, and one that names a file that is not there.
    let (name, text) = with_step("gh release create v1");
    let grouped = format!(
        "{}concurrency:\n  group: {RELEASE_GROUP}\n  cancel-in-progress: false\njobs:",
        text.split("jobs:").next().expect("head")
    ) + text.split("jobs:").nth(1).expect("jobs");
    assert_eq!(
        discovery_problems(&[(name.clone(), grouped.clone())], NO_EXEMPT),
        Vec::<String>::new()
    );
    let cancelling = grouped.replace("cancel-in-progress: false", "cancel-in-progress: true");
    let problems = discovery_problems(&[(name.clone(), cancelling)], NO_EXEMPT);
    assert!(
        problems.iter().any(|p| p.contains("cancel-in-progress")),
        "{problems:?}"
    );
    let files = [(name.clone(), text.clone())];
    assert_eq!(
        discovery_problems(&files, &[("new.yml", "it only reads, and says why")]),
        Vec::<String>::new()
    );
    for reason in ["", "  "] {
        let problems = discovery_problems(&files, &[("new.yml", reason)]);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("new.yml") && p.contains("reason")),
            "{reason:?}: {problems:?}"
        );
    }
    let problems = discovery_problems(&files, &[("gone.yml", "was removed")]);
    assert!(
        problems.iter().any(|p| p.contains("gone.yml")),
        "{problems:?}"
    );
}

#[test]
fn artifacts_yml_has_its_own_non_cancelling_group_per_commit() {
    // Catches: artifacts.yml moving into the release group (it only builds and attests, so it
    // must not wait for a release), a group not keyed on the commit (two pushes would share
    // it), and a push run that cancels (a commit that is signed is never left half checked).
    let text = text_of("artifacts.yml");
    let block = format!(
        "concurrency:\n  group: {ARTIFACTS_GROUP}\n  cancel-in-progress: ${{{{ github.event_name == 'pull_request' }}}}\n"
    );
    assert!(text.contains(&block), "artifacts.yml carries its group");
    for (with, names) in [
        (
            "concurrency:\n  group: artifacts-${{ github.run_id }}\n  cancel-in-progress: ${{ github.event_name == 'pull_request' }}\n",
            "concurrency group",
        ),
        (
            &format!("concurrency:\n  group: {RELEASE_GROUP}\n  cancel-in-progress: false\n"),
            "concurrency group",
        ),
        (
            &format!("concurrency:\n  group: {ARTIFACTS_GROUP}\n  cancel-in-progress: true\n"),
            "cancel-in-progress",
        ),
    ] {
        let problems = artifacts_problems(&text.replace(&block, with));
        assert!(
            problems.iter().any(|p| p.contains(names)),
            "{with:?} must name {names:?}: {problems:?}"
        );
    }
}

#[test]
fn the_plan_job_if_is_pinned_whole_and_its_inputs_are_pinned() {
    // Catches: one `&&` turned into `||` (every substring of the condition stays in the text),
    // the plan reading another commit than the run built, a freeze that is not passed on or
    // not read from the repository variable, and a group that does not need a push from this
    // repository.
    let text = text_of("release.yml");
    for (replace, with, names) in [
        (
            "event == 'push' && github.event.workflow_run.head_branch == 'main'",
            "event == 'push' || github.event.workflow_run.head_branch == 'main'",
            "plan: `if` is not",
        ),
        (
            "github.event_name == 'workflow_run' && github.event.workflow_run.conclusion",
            "github.event_name == 'workflow_run' || github.event.workflow_run.conclusion",
            "plan: `if` is not",
        ),
        (
            "ref: ${{ github.event_name == 'workflow_run' && github.event.workflow_run.head_sha || inputs.commit }}",
            "ref: ${{ inputs.commit }}",
            "plan: checkout ref",
        ),
        (
            "COMMIT: ${{ github.event.workflow_run.head_sha }}",
            "COMMIT: ${{ github.sha }}",
            "plan: env COMMIT",
        ),
        (" --freeze \"$FREEZE\"", "", "plan: --freeze"),
        (
            "FREEZE: ${{ vars.RELEASE_FREEZE }}",
            "FREEZE: ''",
            "plan: env FREEZE",
        ),
        (
            " && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_repository.full_name == github.repository) && 'main'",
            ") && 'main'",
            "concurrency group",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in release.yml");
        let problems = release_problems(&text.replace(replace, with));
        assert!(
            problems.iter().any(|p| p.contains(names)),
            "{replace:?} must name {names:?}: {problems:?}"
        );
    }
}

#[test]
fn a_release_path_workflow_without_the_group_or_that_cancels_is_named_by_the_lint() {
    // Catches: a check that holds nothing: a copy with no concurrency block, one with another
    // group, one that cancels, and one that does not say, must each be named.
    let text = text_of("release.yml");
    let block = format!("concurrency:\n  group: {RELEASE_GROUP}\n  cancel-in-progress: false\n");
    assert!(
        text.contains(&block),
        "release.yml carries the shared block"
    );
    for (with, names) in [
        ("", "concurrency"),
        (
            "concurrency:\n  group: release\n  cancel-in-progress: false\n",
            "group",
        ),
        (
            "concurrency:\n  group: release-${{ github.run_id }}\n  cancel-in-progress: false\n",
            "group",
        ),
        (
            &format!("concurrency:\n  group: {RELEASE_GROUP}\n  cancel-in-progress: true\n"),
            "cancel-in-progress",
        ),
        (
            &format!("concurrency:\n  group: {RELEASE_GROUP}\n"),
            "cancel-in-progress",
        ),
    ] {
        let problems = concurrency_problems("release.yml", &text.replace(&block, with));
        assert!(
            problems.iter().any(|p| p.contains(names)),
            "{with:?} must name {names:?}: {problems:?}"
        );
    }
}

#[test]
fn every_exempt_workflow_exists_and_says_why() {
    // Catches: an exemption left behind by a deleted file, and one without a reason.
    for (file, reason) in RELEASE_EXEMPT {
        text_of(file);
        assert!(!reason.trim().is_empty(), "{file}");
    }
}

#[test]
fn gamma_checks_out_kbf_at_a_pinned_commit() {
    // Catches: the kbf checkout moving to a branch or a short hash (every release would build
    // whatever kbf's main is at that minute, with the OIDC token in the environment), or the
    // checkout no longer using the pin.
    let text = text_of("release.yml");
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("KBF_DEPLOY_REF: "))
        .expect("KBF_DEPLOY_REF is in release.yml");
    let pin = line.trim_start().trim_start_matches("KBF_DEPLOY_REF: ");
    for with in ["main", &pin[..12], &pin.to_uppercase(), ""] {
        let problems =
            release_problems(&text.replace(line, &format!("      KBF_DEPLOY_REF: {with}")));
        assert!(
            problems.iter().any(|p| p.contains("KBF_DEPLOY_REF")),
            "{with:?}: {problems:?}"
        );
    }
    let problems = release_problems(&text.replace("ref: ${{ env.KBF_DEPLOY_REF }}", "ref: main"));
    assert!(
        problems.iter().any(|p| p.contains("the kbf checkout ref")),
        "{problems:?}"
    );
}
