//! `beta.yml` and the `red` job of it and of `release.yml` ([`super::red`]): the files of the
//! repository pass, and each way of breaking what they are held to is named.

use super::lint::{lint, release_problems};
use super::platforms_tests::in_job;
use super::red::{
    BETA_RED_COMMAND, BETA_RED_IF, RELEASE_RED_COMMAND, RELEASE_RED_IF, beta_problems,
    release_red_problems,
};
use super::{text_of, yaml};

/// The problems of release.yml's `red` job in `text`.
fn release_red(text: &str) -> Vec<String> {
    release_red_problems(&yaml::load(text).expect("parses"))
}

#[test]
fn beta_yml_and_both_red_jobs_pass() {
    // Catches: beta.yml or a red job of the repository that breaks what it is held to.
    let beta = text_of("beta.yml");
    assert_eq!(beta_problems(&beta), Vec::<String>::new());
    assert_eq!(lint("beta.yml", &beta), Vec::<String>::new());
    assert_eq!(release_red(&text_of("release.yml")), Vec::<String>::new());
}

/// `(job, text to replace once in it, its replacement, the problem it gives)` for beta.yml.
const BETA_DEFECTS: &[(&str, &str, &str, &str)] = &[
    // The tests narrowed, or let fail.
    (
        "linux-arm64",
        "cargo test --locked --workspace --features tierstore/s3 --no-fail-fast",
        "cargo test --locked -p tierstore",
        "beta.yml: linux-arm64: no step runs `cargo test --locked --workspace --features tierstore/s3 --no-fail-fast`",
    ),
    (
        "macos-arm64",
        "      - name: The tests of the workspace, on macOS\n",
        "      - name: The tests of the workspace, on macOS\n        continue-on-error: true\n",
        "beta.yml: macos-arm64: `continue-on-error` would hide a failed test from red",
    ),
    (
        "linux-arm64",
        "    timeout-minutes: 45\n",
        "    timeout-minutes: 45\n    continue-on-error: true\n",
        "beta.yml: linux-arm64: `continue-on-error` would hide a failed test from red",
    ),
    // Another platform than the one a pull request does not run.
    (
        "macos-arm64",
        "    runs-on: macos-15\n",
        "    runs-on: ubuntu-24.04\n",
        "beta.yml: macos-arm64: runs-on is Some(\"ubuntu-24.04\"), not macos-15",
    ),
    (
        "linux-arm64",
        "    runs-on: ubuntu-24.04-arm\n",
        "    if: ${{ github.event_name == 'workflow_dispatch' }}\n    runs-on: ubuntu-24.04-arm\n",
        "beta.yml: linux-arm64: has an `if`; the tests run on every push to main",
    ),
    (
        "linux-arm64",
        "    runs-on: ubuntu-24.04-arm\n",
        "    needs: [macos-arm64]\n    runs-on: ubuntu-24.04-arm\n",
        "beta.yml: linux-arm64: needs [\"macos-arm64\"]; it needs no job",
    ),
    // The report: skipped when a test failed, before a test job ended, from a branch, with
    // another stage or a commit it did not test, or able to write anything but its issue.
    (
        "red",
        "    if: ${{ !cancelled() && github.ref == 'refs/heads/main' }}\n",
        "    if: ${{ github.ref == 'refs/heads/main' }}\n",
        "beta.yml: red: `if` is Some(\"${{ github.ref == 'refs/heads/main' }}\")",
    ),
    (
        "red",
        "    needs: [linux-arm64, macos-arm64]\n",
        "    needs: [linux-arm64]\n",
        "beta.yml: red does not wait for macos-arm64: it reads the conclusion of every job of the run",
    ),
    (
        "red",
        "--stage beta",
        "--stage release",
        "beta.yml: red: no step runs `cargo xtask release red --repo \"$GITHUB_REPOSITORY\" --stage beta",
    ),
    (
        "red",
        "--commit \"$GITHUB_SHA\"",
        "--commit \"$COMMIT\"",
        "beta.yml: red: no step runs",
    ),
    (
        "red",
        "      issues: write\n",
        "      issues: write\n      contents: write\n",
        "beta.yml: red: permissions are [\"actions: read\", \"contents: read\", \"contents: write\", \"issues: write\"]",
    ),
    (
        "red",
        "    timeout-minutes: 15\n",
        "    timeout-minutes: 15\n    concurrency:\n      group: release-red-beta\n      cancel-in-progress: false\n",
        "beta.yml: red: has a concurrency group, which drops a waiting report",
    ),
];

#[test]
fn each_defect_in_beta_yml_is_named_by_the_lint() {
    // Catches: a rule of beta.yml that holds nothing, because the file passes whether or not the
    // thing it pins is there.
    let text = text_of("beta.yml");
    for (job, replace, with, says) in BETA_DEFECTS {
        let problems = beta_problems(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.starts_with(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_trigger_a_group_or_a_job_beside_the_tests_and_red_is_named() {
    // Catches: the beta tests started by a pull request (the macOS and arm runners a pull request
    // does not run on) or not by a push to main, a run of one commit cancelled by the next, a job
    // beside the three, a missing red job, and a document that does not parse read as clean.
    let text = text_of("beta.yml");
    for (replace, with, says) in [
        (
            "on:\n  push:\n",
            "on:\n  pull_request:\n  push:\n",
            "beta.yml: triggers are [\"pull_request\", \"push\", \"workflow_dispatch\"], not push and workflow_dispatch",
        ),
        (
            "    branches: [main]\n",
            "    branches: [main, beta]\n",
            "beta.yml: push.branches is [\"beta\", \"main\"], not [\"main\"]",
        ),
        (
            "  group: beta-${{ github.sha }}\n",
            "  group: beta\n",
            "beta.yml: concurrency: group is Some(\"beta\"), not beta-${{ github.sha }}",
        ),
        (
            "  cancel-in-progress: false\n\nenv:",
            "  cancel-in-progress: true\n\nenv:",
            "beta.yml: concurrency: cancel-in-progress is Some(\"true\"), not false",
        ),
        (
            "jobs:\n",
            "jobs:\n  linux-x86_64:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n\n",
            "beta.yml: jobs are [\"linux-arm64\", \"linux-x86_64\", \"macos-arm64\", \"red\"], not linux-arm64, macos-arm64 and red",
        ),
        (
            "  red:\n    needs:",
            "  report:\n    needs:",
            "beta.yml: there is no job red",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in beta.yml");
        let problems = beta_problems(&text.replacen(replace, with, 1));
        assert!(
            problems.iter().any(|p| p == says),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    assert!(!beta_problems("permissions: {\njobs: [\n").is_empty());
}

#[test]
fn the_release_red_job_waits_for_every_job_and_runs_whatever_they_did() {
    // Catches: a red job that starts before a job it must report on ended (a leaf dropped from its
    // needs), one skipped when a job failed or when the plan failed, one that reports a run that
    // planned nothing, a job after it (whose failure nobody reports), a step that closes the
    // issue from a run that released nothing, and a red job that is gone.
    let text = text_of("release.yml");
    assert!(RELEASE_RED_IF.contains("!cancelled()"));
    for (replace, with, says) in [
        (
            ", tap, stable-apt-write-3",
            ", stable-apt-write-3",
            "release.yml: red does not wait for tap: it reads the conclusion of every job of the run",
        ),
        (
            "    needs: [plan, deploy, deploy-mac, ",
            "    needs: [plan, deploy-mac, ",
            "release.yml: red does not wait for deploy: it reads the conclusion of every job of the run",
        ),
        (
            RELEASE_RED_IF,
            "${{ success() && github.ref == 'refs/heads/main' && (needs.plan.result == 'failure' || needs.plan.outputs.release == 'true') }}",
            "release.yml: red: `if` is Some(\"${{ success() && ",
        ),
        (
            " && (needs.plan.result == 'failure' || needs.plan.outputs.release == 'true') }}\n    runs-on: ubuntu-24.04\n    timeout-minutes: 15\n",
            " }}\n    runs-on: ubuntu-24.04\n    timeout-minutes: 15\n",
            "release.yml: red: `if` is Some(\"${{ !cancelled() && github.ref == 'refs/heads/main' }}\")",
        ),
        (
            "COMPLETE: ${{ needs.plan.outputs.release == 'true' }}",
            "COMPLETE: 'true'",
            "release.yml: red: the step that reports does not read COMPLETE from ${{ needs.plan.outputs.release == 'true' }}",
        ),
        (
            "COMMIT: ${{ needs.plan.outputs.commit || github.sha }}",
            "COMMIT: ${{ github.sha }}",
            "release.yml: red: the step that reports does not read COMMIT from ${{ needs.plan.outputs.commit || github.sha }}",
        ),
        (
            "--stage release --run",
            "--stage beta --run",
            "release.yml: red: no step runs `cargo xtask release red --repo \"$GITHUB_REPOSITORY\" --stage release",
        ),
        (
            "  tap:\n    needs: [plan, promote-check, formula]\n",
            "  tap:\n    needs: [plan, promote-check, formula, red]\n",
            "release.yml: tap needs red, which must be last",
        ),
        (
            "\n  red:\n",
            "\n  report:\n",
            "release.yml: there is no job red",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in release.yml");
        let problems = release_red(&text.replacen(replace, with, 1));
        assert!(
            problems.iter().any(|p| p.starts_with(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    assert!(RELEASE_RED_COMMAND.contains("--complete \"$COMPLETE\""));
    assert!(BETA_RED_COMMAND.ends_with("--complete true"));
}

#[test]
fn issues_write_is_held_by_red_alone_and_only_under_its_exact_condition() {
    // Catches: the issue scope held by a job that runs code beside a build, by red under a
    // condition that lets a branch write the issue, and in a workflow that has no red job.
    let beta = text_of("beta.yml");
    let spoiled = in_job(&beta, "red", BETA_RED_IF, "${{ !cancelled() }}");
    assert!(
        lint("beta.yml", &spoiled)
            .iter()
            .any(|p| p == &format!("job red: `issues: write` needs the exact `if: {BETA_RED_IF}`")),
        "beta red without main"
    );
    let spoiled = in_job(
        &beta,
        "linux-arm64",
        "      contents: read\n",
        "      contents: read\n      issues: write\n",
    );
    assert!(
        lint("beta.yml", &spoiled)
            .iter()
            .any(|p| p == "job linux-arm64: `issues: write` is not allowed in beta.yml"),
        "a test job with issues: write"
    );
    let release = text_of("release.yml");
    let spoiled = in_job(&release, "red", RELEASE_RED_IF, "${{ !cancelled() }}");
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p
                == &format!("job red: `issues: write` needs the exact `if: {RELEASE_RED_IF}`")),
        "release red without its condition"
    );
    let spoiled = in_job(
        &release,
        "deploy-check",
        "      contents: read\n",
        "      contents: read\n      issues: write\n",
    );
    assert!(
        release_problems(&spoiled)
            .iter()
            .any(|p| p == "deploy-check: permissions grant `issues: write`; only publish writes"),
        "deploy-check with issues: write"
    );
}
