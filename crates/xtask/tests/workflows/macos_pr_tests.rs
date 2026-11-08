//! The macOS checks of a pull request ([`super::macos_pr`]): `macos.yml` passes, and each way of
//! skipping its macOS job on a pull request, of naming it so a skipped job passes for it, of
//! checking less than the release build does, or of handing it a secret, is named.

use super::lint::{entries, entry};
use super::macos_pr::{
    MAC_JOB, PR_MAC_TESTS, macos_pr_problems, pr_mac_commands, pull_request_job_problems, scalars,
};
use super::platforms::{BUILDS, MAC_TESTS, build_commands, pull_request_macos_problems};
use super::platforms_tests::in_job;
use super::text_of;
use super::yaml;
use xtask::macos_paths::SKIPPABLE_FILES;

fn problems_of(text: &str) -> Vec<String> {
    macos_pr_problems(text, &text_of("artifacts.yml"))
}

#[test]
fn macos_yml_is_the_macos_checks_of_a_pull_request() {
    // Catches: any change to macos.yml that the defects below name, in the file as it is.
    assert_eq!(problems_of(&text_of("macos.yml")), Vec::<String>::new());
}

#[test]
fn the_macos_job_runs_the_release_builds_commands_with_the_workspaces_tests() {
    // Catches: the macOS job of a pull request drifting from artifacts.yml's macOS build (a check
    // the release makes that the pull request does not), and its tests narrowed back to the
    // binary's crates.
    let mac = BUILDS
        .iter()
        .find(|b| b.platform == "macos-arm64")
        .expect("mac");
    let release = build_commands(mac);
    let pr = pr_mac_commands();
    assert_eq!(pr.len(), release.len());
    let differ: Vec<(&String, &String)> = release.iter().zip(&pr).filter(|(r, p)| r != p).collect();
    assert_eq!(differ, [(&MAC_TESTS.to_owned(), &PR_MAC_TESTS.to_owned())]);
    assert!(!PR_MAC_TESTS.contains(" -p "), "{PR_MAC_TESTS}");
}

/// `(job, text to replace in it, its replacement, what a problem must say)`; job `""` is the
/// whole file.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // The triggers, and the one group per pull request that cancels the older run.
    (
        "",
        "on:\n  pull_request:\n",
        "on:\n  pull_request:\n  push:\n",
        "triggers are",
    ),
    (
        "",
        "group: macos-${{ github.event.pull_request.number }}",
        "group: macos-${{ github.run_id }}",
        "concurrency group",
    ),
    (
        "",
        "cancel-in-progress: true",
        "cancel-in-progress: false",
        "cancel-in-progress",
    ),
    (
        "",
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 4c53",
        "  CYCLONEDX_SHA256_AARCH64_MACOS: 0c53",
        "env CYCLONEDX_SHA256_AARCH64_MACOS",
    ),
    (
        "",
        "  CYCLONEDX_VERSION: 0.5.9",
        "  CYCLONEDX_VERSION: 0.5.8",
        "env CYCLONEDX_VERSION",
    ),
    (
        "",
        "jobs:\n",
        "jobs:\n  extra:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo\n",
        "jobs are",
    ),
    // version: the version of the build, as artifacts.yml computes it.
    (
        "version",
        "runs-on: ubuntu-24.04",
        "runs-on: macos-15",
        "version: does not run on an ubuntu runner",
    ),
    (
        "version",
        "    runs-on: ubuntu-24.04\n",
        "    if: ${{ false }}\n    runs-on: ubuntu-24.04\n",
        "version: runs under `if: Some(\"${{ false }}\")`",
    ),
    (
        "version",
        "        run: cargo xtask release version",
        "        continue-on-error: true\n        run: cargo xtask release version",
        "version: the step",
    ),
    (
        "version",
        " --version-file VERSION",
        "",
        "version: does not run `cargo xtask release version",
    ),
    (
        "version",
        "        id: version\n",
        "",
        "version: the ids of its steps are",
    ),
    (
        "version",
        "      - name: Compile xtask\n",
        "      - name: Compile xtask\n        id: version\n",
        "version: the ids of its steps are",
    ),
    (
        "version",
        "version: ${{ steps.version.outputs.version }}",
        "version: 0.0.0",
        "version: output version",
    ),
    (
        "version",
        "      - name: Compile xtask\n",
        "      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - name: Compile xtask\n",
        "version: uses actions/cache/restore@",
    ),
    // A job named as artifacts.yml's macOS job, which a pull request's run of artifacts.yml skips.
    (
        "",
        "\n  macos:\n",
        "\n  macos-arm64:\n",
        "jobs are [\"changes\", \"macos-arm64\", \"macos-checks\", \"version\"]",
    ),
    // macos: on every pull request, the release build's commands, nothing uploaded or held.
    (
        "macos",
        MAC_IF_LINE,
        "    if: ${{ contains(github.event.pull_request.labels.*.name, 'macos') }}\n",
        "macos: runs under `if: Some(",
    ),
    (
        "macos",
        MAC_IF_LINE,
        "    if: ${{ always() }}\n",
        "macos: runs under `if: Some(\"${{ always() }}\")`",
    ),
    // A macOS job that runs only on an answer of `true` (a `changes` that failed would skip it),
    // or that is skipped on an answer of `false` from a `changes` that failed.
    (
        "macos",
        "!(needs.changes.result == 'success' && needs.changes.outputs.macos == 'false') }}",
        "needs.changes.outputs.macos == 'true' }}",
        "macos: runs under `if: Some(",
    ),
    (
        "macos",
        "!(needs.changes.result == 'success' && needs.changes.outputs.macos == 'false') }}",
        "needs.changes.outputs.macos != 'false' }}",
        "macos: runs under `if: Some(",
    ),
    // One that runs after a failed `version`, or in a cancelled run.
    (
        "macos",
        "!cancelled() && needs.version.result == 'success' && ",
        "!cancelled() && ",
        "macos: runs under `if: Some(",
    ),
    (
        "macos",
        "${{ !cancelled() && ",
        "${{ always() && ",
        "macos: runs under `if: Some(",
    ),
    ("macos", MAC_IF_LINE, "", "macos: runs under `if: None`"),
    (
        "macos",
        "-macos-arm64-${{ hashFiles",
        "-${{ github.job }}-${{ hashFiles",
        "macos: its cache restores are",
    ),
    (
        "macos",
        "restore-keys: cargo-${{ runner.os }}-${{ runner.arch }}-macos-arm64-",
        "restore-keys: cargo-${{ runner.os }}-${{ runner.arch }}-",
        "macos: its cache restores are",
    ),
    (
        "macos",
        "needs: [changes, version]",
        "needs: [version]",
        "macos: needs are",
    ),
    (
        "macos",
        "runs-on: macos-15",
        "runs-on: macos-14",
        "macos: runs on Some(\"macos-14\")",
    ),
    (
        "macos",
        "TARGET: aarch64-apple-darwin",
        "TARGET: x86_64-apple-darwin",
        "macos: TARGET is not",
    ),
    (
        "macos",
        "RELEASE_VERSION: ${{ needs.version.outputs.version }}",
        "RELEASE_VERSION: 0.0.0",
        "macos: env RELEASE_VERSION",
    ),
    (
        "macos",
        "      RELEASE_VERSION:",
        "      CARGO_BUILD_TARGET: x\n      RELEASE_VERSION:",
        "macos: its env sets",
    ),
    (
        "macos",
        " --workspace --features",
        " -p tierstore --features",
        "macos: does not run `cargo test --locked --workspace",
    ),
    (
        "macos",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - name: The packaged binary carries a valid code signature\n        if: ${{ false }}\n",
        "macos: the step `The packaged binary carries a valid code signature` has an `if:`",
    ),
    (
        "macos",
        "        run: codesign --verify --strict",
        "        continue-on-error: true\n        run: codesign --verify --strict",
        "has continue-on-error",
    ),
    (
        "macos",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - name: The packaged binary carries a valid code signature\n        env:\n          TARGET: x\n",
        "sets its own env",
    ),
    (
        "macos",
        "--platform macos-arm64 --scratch",
        "--platform linux-arm64 --scratch",
        "macos: does not run `cargo xtask verify-assets hostkeeper-package",
    ),
    (
        "macos",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1\n      - name: The packaged binary carries a valid code signature\n",
        "macos: uses actions/upload-artifact@",
    ),
    (
        "macos",
        "      - name: The packaged binary carries a valid code signature\n",
        "      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - name: The packaged binary carries a valid code signature\n",
        "macos: uses actions/cache/save@",
    ),
    (
        "macos",
        "          persist-credentials: false\n",
        "          persist-credentials: true\n",
        "macos: a checkout keeps the token",
    ),
    // changes: the paths of the pull request, judged by `cargo xtask macos-paths`.
    (
        "changes",
        "runs-on: ubuntu-24.04",
        "runs-on: macos-15",
        "changes: does not run on an ubuntu runner",
    ),
    (
        "changes",
        "    runs-on: ubuntu-24.04\n",
        "    if: ${{ github.event.pull_request.draft == false }}\n    runs-on: ubuntu-24.04\n",
        "changes: runs under `if: Some(",
    ),
    (
        "changes",
        "      contents: read\n",
        "      contents: read\n      pull-requests: write\n",
        "changes: its permissions are",
    ),
    (
        "changes",
        "    timeout-minutes: 15\n",
        "    timeout-minutes: 15\n    env:\n      GIT_DIR: x\n",
        "changes: it or a step sets an env",
    ),
    (
        "changes",
        "        id: changes\n",
        "        id: changes\n        env:\n          GIT_DIR: x\n",
        "changes: it or a step sets an env",
    ),
    (
        "changes",
        "        run: cargo xtask macos-paths",
        "        continue-on-error: true\n        run: cargo xtask macos-paths",
        "changes: the step `Whether the macOS job is needed` has continue-on-error",
    ),
    // A rename judged by its new path alone, and a diff against another commit than the base.
    (
        "changes",
        "git diff --no-renames --name-only",
        "git diff --name-only",
        "changes: does not run `git diff --no-renames",
    ),
    (
        "changes",
        " HEAD^1 HEAD >",
        " HEAD~5 HEAD >",
        "changes: does not run `git diff --no-renames",
    ),
    (
        "changes",
        "          fetch-depth: 2\n",
        "",
        "changes: its checkouts (persist-credentials, fetch-depth) are [(Some(\"false\"), None)]",
    ),
    (
        "changes",
        "          fetch-depth: 2\n",
        "          fetch-depth: 1\n",
        "changes: its checkouts",
    ),
    // A checkout of the head, or of anything but the merge commit: the diff would then be of the
    // last commit of the pull request alone.
    (
        "changes",
        "          fetch-depth: 2\n",
        "          fetch-depth: 2\n          ref: ${{ github.event.pull_request.head.sha }}\n",
        "changes: a checkout sets [[\"fetch-depth\", \"persist-credentials\", \"ref\"]]",
    ),
    (
        "changes",
        "          fetch-depth: 2\n",
        "          fetch-depth: 2\n          path: elsewhere\n",
        "changes: a checkout sets",
    ),
    (
        "changes",
        "          persist-credentials: false\n          fetch-depth: 2\n",
        "          fetch-depth: 2\n",
        "changes: its checkouts",
    ),
    (
        "changes",
        "      - run: rustup toolchain install\n",
        "      - uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - run: rustup toolchain install\n",
        "changes: uses actions/cache/restore@",
    ),
    (
        "changes",
        "        id: changes\n",
        "        id: answer\n",
        "changes: the ids of its steps are",
    ),
    (
        "changes",
        "      - name: The paths the pull request changes\n",
        "      - name: The paths the pull request changes\n        id: diff\n",
        "changes: the ids of its steps are",
    ),
    (
        "changes",
        "macos: ${{ steps.changes.outputs.macos }}",
        "macos: 'true'",
        "changes: its outputs are not",
    ),
    (
        "changes",
        "      macos: ${{ steps.changes.outputs.macos }}\n",
        "      macos: ${{ steps.changes.outputs.macos }}\n      extra: x\n",
        "changes: its outputs are not",
    ),
    // macos-checks: the one check, never skipped, failing unless the macOS checks passed or
    // were not needed.
    (
        "macos-checks",
        "    if: ${{ always() }}\n",
        "    if: ${{ !cancelled() }}\n",
        "macos-checks: runs under `if: Some(\"${{ !cancelled() }}\")`",
    ),
    (
        "macos-checks",
        "    if: ${{ always() }}\n",
        "",
        "macos-checks: runs under `if: None`",
    ),
    (
        "macos-checks",
        "needs: [changes, version, macos]",
        "needs: [version, macos]",
        "macos-checks: needs are",
    ),
    (
        "macos-checks",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: macos-15\n",
        "macos-checks: does not run on an ubuntu runner",
    ),
    (
        "macos-checks",
        "    permissions: {}\n",
        "    permissions:\n      contents: read\n",
        "macos-checks: its permissions are",
    ),
    (
        "macos-checks",
        "    permissions: {}\n",
        "",
        "macos-checks: its permissions are None",
    ),
    (
        "macos-checks",
        "    timeout-minutes: 5\n",
        "    timeout-minutes: 5\n    continue-on-error: true\n",
        "macos-checks: has continue-on-error",
    ),
    (
        "macos-checks",
        "        run: exit 1\n",
        "        run: exit 0\n",
        "macos-checks: its steps are not",
    ),
    (
        "macos-checks",
        "        run: exit 1\n",
        "        continue-on-error: true\n        run: exit 1\n",
        "macos-checks: its steps are not",
    ),
    (
        "macos-checks",
        "        run: exit 1\n",
        "        run: exit 1\n      - run: echo\n",
        "macos-checks: its steps are not",
    ),
    // A gate that passes on a skipped macOS job whatever `changes` answered, or did.
    (
        "macos-checks",
        " && needs.changes.outputs.macos == 'false'",
        "",
        "macos-checks: its steps are not",
    ),
    (
        "macos-checks",
        " && needs.changes.result == 'success'",
        "",
        "macos-checks: its steps are not",
    ),
    (
        "macos-checks",
        "!(needs.version.result == 'success' && (",
        "!((",
        "macos-checks: its steps are not",
    ),
    // A gate step that a cancelled run skips (the gate would then pass).
    (
        "macos-checks",
        "${{ always() && !(",
        "${{ !(",
        "macos-checks: its steps are not",
    ),
    (
        "",
        "\n  macos-checks:\n",
        "\n  macos-arm64:\n",
        "jobs are [\"changes\", \"macos\", \"macos-arm64\", \"version\"]",
    ),
];

/// The line of the macOS job's condition.
const MAC_IF_LINE: &str = "    if: ${{ !cancelled() && needs.version.result == 'success' && !(needs.changes.result == 'success' && needs.changes.outputs.macos == 'false') }}\n";

#[test]
fn each_defect_in_macos_yml_is_named_by_the_lint() {
    // Catches: a pin of macos.yml that holds nothing, because the file passes whether or not the
    // thing it pins is there.
    let text = text_of("macos.yml");
    for (job, replace, with, says) in DEFECTS {
        let spoiled = if job.is_empty() {
            assert!(text.contains(replace), "{replace:?} is not in macos.yml");
            text.replacen(replace, with, 1)
        } else {
            in_job(&text, job, replace, with)
        };
        let problems = problems_of(&spoiled);
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_job_missing_from_macos_yml_or_a_file_that_does_not_parse_is_named() {
    // Catches: a job deleted (its checks would then find nothing to refuse), and a file read as
    // clean because it could not be read at all.
    let text = text_of("macos.yml");
    for (job, says) in [
        ("changes", "no job changes"),
        ("version", "no job version"),
        ("macos", "no job macos"),
        ("macos-checks", "no job macos-checks"),
    ] {
        let renamed = text.replacen(&format!("\n  {job}:\n"), "\n  renamed:\n", 1);
        let problems = problems_of(&renamed);
        assert!(
            problems.iter().any(|p| p.ends_with(says)),
            "{job}: {problems:?}"
        );
    }
    assert_eq!(
        macos_pr_problems("jobs: [", &text_of("artifacts.yml")).len(),
        1
    );
    assert_eq!(macos_pr_problems(&text, "jobs: [").len(), 1);
}

/// The macOS job of macos.yml, with `replace` replaced by `with` in it.
fn spoiled_mac(replace: &str, with: &str) -> String {
    in_job(&text_of("macos.yml"), MAC_JOB, replace, with)
}

#[test]
fn a_secret_or_the_token_in_a_pull_request_macos_job_is_refused() {
    // Catches: a secret handed to the macOS job of a pull request in any way the job can read it
    // (a step's env, the job's env, the workflow's env every job sees, an index into the
    // context, the whole context, `secrets: inherit`), the token by either of its names, and each
    // named by the lint of every workflow as well as by the lint of macos.yml.
    let step = "      - name: The packaged binary carries a valid code signature\n";
    for (replace, with, word) in [
        (
            step,
            "      - name: The packaged binary carries a valid code signature\n        env:\n          KEY: ${{ secrets.SIGNING_KEY }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      KEY: ${{ secrets['SIGNING_KEY'] }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      ALL: ${{ toJSON(SECRETS) }}\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      GH_TOKEN: ${{ github.token }}\n",
            "github.token",
        ),
        (
            "run: codesign --verify --strict",
            "run: echo \"$GITHUB_TOKEN\" && codesign --verify --strict",
            "github_token",
        ),
        (
            "    timeout-minutes: 60\n",
            "    timeout-minutes: 60\n    secrets: inherit\n",
            "secrets",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      T: ${{ github['token'] }}\n",
            "github[",
        ),
        (
            "      TARGET: aarch64-apple-darwin\n",
            "      TARGET: aarch64-apple-darwin\n      ALL: ${{ toJSON(github) }}\n",
            "tojson(github",
        ),
    ] {
        let text = spoiled_mac(replace, with);
        let named = format!("names {word}");
        for problems in [
            problems_of(&text),
            pull_request_macos_problems("macos.yml", &text),
        ] {
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains("the job macos ") && p.contains(&named)),
                "{with:?}: {problems:?}"
            );
        }
    }
    let workflow_env = text_of("macos.yml").replacen(
        "  CARGO_TERM_COLOR: always\n",
        "  CARGO_TERM_COLOR: always\n  KEY: ${{ secrets.SIGNING_KEY }}\n",
        1,
    );
    assert!(
        pull_request_macos_problems("macos.yml", &workflow_env)
            .iter()
            .any(|p| p.contains("names secrets (`${{ secrets.SIGNING_KEY }}`)")),
        "a secret in the workflow's env"
    );
}

#[test]
fn a_write_or_oidc_scope_or_an_environment_in_a_pull_request_macos_job_is_refused() {
    // Catches: the macOS job of a pull request given an OIDC token (`id-token`, at any level), a
    // write scope, no `permissions` (which would inherit whatever the top level grants, so it is
    // spelled out), or an environment (whose secrets it would hold).
    let permissions = "    permissions:\n      contents: read\n";
    for with in [
        "    permissions:\n      contents: read\n      id-token: write\n",
        "    permissions:\n      contents: read\n      id-token: read\n",
        "    permissions:\n      contents: write\n",
        "    permissions:\n      contents: read\n      actions: write\n",
        "",
    ] {
        let text = spoiled_mac(permissions, with);
        assert!(
            pull_request_macos_problems("macos.yml", &text)
                .iter()
                .any(|p| p.contains("not contents: read alone")),
            "{with:?}"
        );
    }
    let text = spoiled_mac(
        permissions,
        &format!("{permissions}    environment: production\n"),
    );
    assert!(
        pull_request_macos_problems("macos.yml", &text)
            .iter()
            .any(|p| p.contains("in an environment")),
        "environment"
    );
}

#[test]
fn the_secret_check_names_the_exact_problems_and_nothing_in_a_clean_job() {
    // Catches: a check that refuses every job (the cases above would then prove nothing) and one
    // whose words do not say which job and what it holds.
    let doc = yaml::load(
        "runs-on: macos-15\npermissions:\n  contents: read\nsteps:\n  - run: cargo test\n",
    )
    .expect("load");
    assert_eq!(
        pull_request_job_problems("x.yml", "mac", &doc, None),
        Vec::<String>::new()
    );
    let doc = yaml::load(
        "runs-on: macos-15\nenvironment: prod\nsteps:\n  - run: echo ${{ secrets.A }}\n",
    )
    .expect("load");
    assert_eq!(
        pull_request_job_problems("x.yml", "mac", &doc, None),
        [
            "x.yml: the job mac runs on macOS for a pull request with permissions [], not contents: read alone",
            "x.yml: the job mac runs on macOS for a pull request in an environment, whose secrets it would hold",
            "x.yml: the job mac runs on macOS for a pull request and names secrets (`echo ${{ secrets.A }}`); it holds no secret and no token",
        ]
    );
}

#[test]
fn a_pull_request_job_whose_runner_the_lint_cannot_see_is_refused() {
    // Catches: a pull request's job put on macOS through a matrix or an input (`runs-on` as an
    // expression), a label in another case, or a reusable workflow, any of which would pass the
    // check of the job's runner by its name; while a workflow no pull request starts may do so.
    let head = "on:\n  pull_request:\njobs:\n";
    let matrix = format!(
        "{head}  t:\n    strategy:\n      matrix:\n        os: [macos-15]\n    runs-on: ${{{{ matrix.os }}}}\n    permissions:\n      contents: read\n"
    );
    assert_eq!(
        pull_request_macos_problems("x.yml", &matrix),
        [
            "x.yml: the job t of a pull request runs on an expression; name its runner, so the lint can tell whether it is macOS"
        ]
    );
    let called = format!(
        "{head}  t:\n    uses: o/r/.github/workflows/w.yml@3d3c42e5aac5ba805825da76410c181273ba90b1 # v1.0.0\n"
    );
    assert_eq!(
        pull_request_macos_problems("x.yml", &called),
        [
            "x.yml: the job t of a pull request calls a reusable workflow, whose runners the lint cannot see"
        ]
    );
    let upper =
        format!("{head}  t:\n    runs-on: MacOS-15\n    permissions:\n      contents: read\n");
    assert_eq!(pull_request_macos_problems("x.yml", &upper).len(), 1);
    let by_hand = matrix.replace("  pull_request:", "  workflow_dispatch:");
    assert_eq!(
        pull_request_macos_problems("x.yml", &by_hand),
        Vec::<String>::new()
    );
}

#[test]
fn every_workflow_a_pull_request_may_change_without_the_macos_job_exists_and_runs_nothing_on_macos()
{
    // Catches: a workflow of `xtask::macos_paths::SKIPPABLE_FILES` renamed or deleted (its entry
    // would then name nothing, and the new name would run the macOS job unseen), macos.yml or
    // artifacts.yml (which define the macOS build) listed, and a listed workflow given a job on
    // macOS, whose change the macOS job would then not see.
    let names: Vec<String> = super::workflows().into_iter().map(|(n, _)| n).collect();
    let listed: Vec<&str> = SKIPPABLE_FILES
        .iter()
        .filter_map(|p| p.strip_prefix(".github/workflows/"))
        .collect();
    assert!(!listed.is_empty());
    for file in listed {
        assert!(names.iter().any(|n| n == file), "{file} is not a workflow");
        assert!(file != "macos.yml" && file != "artifacts.yml", "{file}");
        let doc = yaml::load(&text_of(file)).expect("load");
        for (name, job) in entry(&doc, "jobs").map_or(&[][..], entries) {
            let mut labels = Vec::new();
            if let Some(runs_on) = entry(job, "runs-on") {
                scalars(runs_on, &mut labels);
            }
            assert!(
                !labels
                    .iter()
                    .any(|l| l.to_ascii_lowercase().contains("macos")),
                "{file}: the job {name:?} runs on {labels:?}"
            );
        }
    }
}
