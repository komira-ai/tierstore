//! `hostkeeper-desired.yml` ([`super::desired`]): the file of the repository passes, each way of
//! breaking what it is held to is named, its `id-token: write` is held by `set-desired` alone and
//! under the variable gate, and no workflow reads another coordinator's variable, names
//! `deploy-farm`, or starts the run.

use super::desired::{
    AUDIENCE, CHECK, COORDINATOR_VARS, DESIRED_IF, FILE, SET_DESIRED, caller_problems,
    coordinator_problems, desired_problems,
};
use super::lint::lint;
use super::platforms_tests::in_job;
use super::{text_of, workflows};

#[test]
fn hostkeeper_desired_yml_is_a_run_by_hand_that_checks_the_release_then_sets_it() {
    // Catches: any change to hostkeeper-desired.yml that breaks what it is held to: a trigger
    // added, the gate dropped from a job, the check skipped or not handed on, hostkeeper's
    // workflow called with another version, digest, pin or coordinator.
    assert_eq!(desired_problems(&text_of(FILE)), Vec::<String>::new());
    assert_eq!(lint(FILE, &text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_both_jobs() {
    // Catches: a gate on a step (the job would start, and the token exist, with no coordinator
    // to call), a gate on another variable, and a job that runs from a branch other than main.
    assert!(DESIRED_IF.contains("vars.HOSTKEEPER_COORDINATOR_URL != ''"));
    assert!(DESIRED_IF.contains("github.ref == 'refs/heads/main'"));
    let text = text_of(FILE);
    for job in ["check", "set-desired"] {
        for (with, says) in [
            ("${{ github.ref == 'refs/heads/main' }}", "`if` is"),
            (
                "${{ github.ref == 'refs/heads/main' && vars.OTHER_URL != '' }}",
                "`if` is",
            ),
            ("${{ vars.HOSTKEEPER_COORDINATOR_URL != '' }}", "`if` is"),
        ] {
            let spoiled = in_job(&text, job, DESIRED_IF, with);
            let problems = desired_problems(&spoiled);
            assert!(
                problems
                    .iter()
                    .any(|p| p.starts_with(&format!("{job}: ")) && p.contains(says)),
                "{job}: {with}: {problems:?}"
            );
        }
        let spoiled = in_job(&text, job, &format!("    if: {DESIRED_IF}\n"), "");
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p == &format!("{job}: `if` is None, not {DESIRED_IF}")),
            "{job} without an `if`"
        );
    }
}

#[test]
fn the_token_is_held_by_set_desired_alone_and_only_under_the_gate() {
    // Catches: `id-token: write` moved to the check job, which runs this repository's code, or
    // held by set-desired under a condition that lets a run without the variable, or from another
    // branch, through.
    let text = text_of(FILE);
    let spoiled = in_job(
        &text,
        "set-desired",
        DESIRED_IF,
        "${{ github.ref == 'refs/heads/main' }}",
    );
    assert!(
        lint(FILE, &spoiled).iter().any(|p| p
            == &format!("job set-desired: `id-token: write` needs the exact `if: {DESIRED_IF}`")),
        "set-desired under another condition"
    );
    let spoiled = in_job(
        &text,
        "check",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
    );
    assert!(
        lint(FILE, &spoiled)
            .iter()
            .any(|p| p == "job check: `id-token: write` is not allowed in hostkeeper-desired.yml"),
        "check with id-token: write"
    );
    // The same job in another file is refused.
    assert!(
        lint("release.yml", &text)
            .iter()
            .any(|p| p == "job set-desired: `id-token: write` is not allowed in release.yml")
    );
}

/// `(job, text to replace once in it, its replacement, what a problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "check",
        "    runs-on: ubuntu-24.04\n",
        "    needs: [set-desired]\n    runs-on: ubuntu-24.04\n",
        "check: needs [\"set-desired\"]; it needs no job",
    ),
    (
        "check",
        "      contents: read\n",
        "      contents: read\n      actions: read\n",
        "check: permissions are [\"actions: read\", \"contents: read\"], not contents: read alone",
    ),
    (
        "check",
        "--dir \"$RUNNER_TEMP/desired\"",
        "--dir dist",
        "check: the step `desired` does not run",
    ),
    (
        "check",
        "        id: desired\n",
        "        id: index\n",
        "check: the step `desired` does not run",
    ),
    (
        "check",
        "VERSION: ${{ inputs.version }}",
        "VERSION: 0.1.0",
        "check: the step `desired` does not read VERSION from ${{ inputs.version }}",
    ),
    (
        "check",
        "package-digest: ${{ steps.desired.outputs.package-digest }}",
        "package-digest: ''",
        "check: output package-digest is",
    ),
    (
        "check",
        "package-version: ${{ steps.desired.outputs.package-version }}",
        "package-version: ${{ inputs.version }}",
        "check: output package-version is",
    ),
    (
        "set-desired",
        "needs: [check]",
        "needs: [check, publish]",
        "set-desired: needs are [\"check\", \"publish\"], not [\"check\"]",
    ),
    (
        "set-desired",
        "    needs: [check]\n",
        "",
        "set-desired: needs are [], not [\"check\"]",
    ),
    (
        "set-desired",
        "      id-token: write\n",
        "      id-token: write\n      actions: read\n",
        "set-desired: permissions are",
    ),
    (
        "set-desired",
        "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
        "komira-ai/other/.github/workflows/set-desired.yml@",
        "set-desired: `uses` is not",
    ),
    (
        "set-desired",
        "service: tierstore",
        "service: hello",
        "set-desired: with.service",
    ),
    // The tag alone: not the version the packages carry.
    (
        "set-desired",
        "version: ${{ needs.check.outputs.package-version }}",
        "version: ${{ inputs.version }}",
        "set-desired: with.version",
    ),
    // A digest from release.yml's publish job, which this run does not wait for.
    (
        "set-desired",
        "package_digest: ${{ needs.check.outputs.package-digest }}",
        "package_digest: ${{ needs.publish.outputs.package_sha256 }}",
        "set-desired: with.package_digest",
    ),
    (
        "set-desired",
        "hostkeeper_version: 0.1.33",
        "hostkeeper_version: 0.1.32",
        "set-desired: with.hostkeeper_version is Some(\"0.1.32\"), not \"0.1.33\"",
    ),
    (
        "set-desired",
        " # v0.1.33",
        "",
        "set-desired: with.hostkeeper_version is Some(\"0.1.33\"), not \"\"",
    ),
    (
        "set-desired",
        "hostkeeper_commit: 1adad60c2a7c0136b9f46cb79a987883f2185445",
        "hostkeeper_commit: 0123456789abcdef0123456789abcdef01234567",
        "set-desired: with.hostkeeper_commit",
    ),
    (
        "set-desired",
        "coordinator_url: ${{ vars.HOSTKEEPER_COORDINATOR_URL }}",
        "coordinator_url: ${{ vars.OTHER_URL }}",
        "set-desired: with.coordinator_url",
    ),
    (
        "set-desired",
        "coordinator_ca: ${{ vars.HOSTKEEPER_COORDINATOR_CA }}",
        "coordinator_ca: ''",
        "set-desired: with.coordinator_ca",
    ),
    (
        "set-desired",
        "audience: tierstore-hostkeeper-desired",
        "audience: tierstore",
        "set-desired: with.audience",
    ),
    (
        "set-desired",
        "wait: true",
        "wait: false",
        "set-desired: with.wait is Some(\"false\"), not \"true\"",
    ),
];

#[test]
fn each_defect_in_hostkeeper_desired_yml_is_named_by_the_lint() {
    // Catches: a pin of hostkeeper-desired.yml that holds nothing, because the file passes
    // whether or not the thing it pins is there.
    let text = text_of(FILE);
    for (job, replace, with, says) in DEFECTS {
        let problems = desired_problems(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_trigger_an_input_or_a_job_other_than_the_run_by_hand_is_named() {
    // Catches: the run started by the end of a release.yml run (the channel may not hold the
    // version yet), a second input or none, an input that is not a required string, a job beside
    // the two, and a document that does not parse being read as clean.
    let text = text_of(FILE);
    for (replace, with, says) in [
        (
            "on:\n  workflow_dispatch:\n",
            "on:\n  workflow_run:\n    workflows: [release]\n    types: [completed]\n  workflow_dispatch:\n",
            "triggers are [\"workflow_dispatch\", \"workflow_run\"], not workflow_dispatch alone",
        ),
        (
            "on:\n  workflow_dispatch:\n",
            "on:\n  workflow_call:\n  workflow_dispatch:\n",
            "triggers are",
        ),
        (
            "      version:\n        description",
            "      tag:\n        description",
            "inputs are [\"tag\"], not version alone",
        ),
        (
            "      version:\n        description",
            "      note:\n        type: string\n      version:\n        description",
            "inputs are [\"note\", \"version\"], not version alone",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: number\n        required: true\n",
            "input version: type is Some(\"number\"), not string",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: string\n        required: false\n",
            "input version: required is Some(\"false\"), not true",
        ),
        (
            "jobs:\n  check:\n",
            "jobs:\n  publish:\n    runs-on: ubuntu-24.04\n  check:\n",
            "jobs are [\"check\", \"publish\", \"set-desired\"], not check and set-desired",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in {FILE}");
        let problems = desired_problems(&text.replacen(replace, with, 1));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    assert!(!desired_problems("permissions: {\njobs: [\n").is_empty());
}

#[test]
fn the_check_runs_the_verb_that_refuses_an_index_without_a_platform_before_the_coordinator_is_called()
 {
    // Catches: set-desired starting without the check (it needs it), or the check running
    // another command than the verb whose unit tests refuse an index of the Linux platforms
    // alone (src/release/desired/tests.rs); the version and digest it sets are the check's.
    let text = text_of(FILE);
    assert!(text.contains(&format!("        run: {CHECK}\n")));
    assert!(text.contains("    needs: [check]\n"));
    assert!(text.contains(&format!("    uses: {SET_DESIRED}@")));
    assert!(text.contains(&format!("      audience: {AUDIENCE}\n")));
}

#[test]
fn no_workflow_reads_another_coordinator_variable_names_deploy_farm_or_starts_the_run() {
    // Catches: a job that sets the version through another coordinator than hostkeeper's (the
    // farm coordinator's variable or verb coming back), and release.yml or another workflow
    // starting or calling hostkeeper-desired.yml.
    let files = workflows();
    for (file, text) in &files {
        assert_eq!(
            coordinator_problems(file, text),
            Vec::<String>::new(),
            "{file}"
        );
    }
    assert_eq!(caller_problems(&files), Vec::<String>::new());
    // Hostkeeper's two variables, and a variable that is not a coordinator's, are read freely.
    let reads = format!(
        "a: ${{{{ vars.{} }}}} ${{{{ vars.{} }}}}\nb: ${{{{ vars.RELEASE_FREEZE }}}}\n",
        COORDINATOR_VARS[0], COORDINATOR_VARS[1]
    );
    assert_eq!(coordinator_problems("t.yml", &reads), Vec::<String>::new());
    assert_eq!(
        coordinator_problems(
            "t.yml",
            "x: 1\n    if: ${{ vars.FARM_COORDINATOR_URL != '' }}\n        run: cargo xtask deploy-farm --service tierstore\n"
        ),
        vec![
            "t.yml: line 2: reads vars.FARM_COORDINATOR_URL, not one of hostkeeper's [\"HOSTKEEPER_COORDINATOR_URL\", \"HOSTKEEPER_COORDINATOR_CA\"]".to_owned(),
            "t.yml: line 3: names deploy-farm".to_owned(),
        ]
    );
    let release = files
        .iter()
        .find(|(name, _)| name == "release.yml")
        .expect("release.yml")
        .1
        .clone();
    let calling = vec![
        (FILE.to_owned(), text_of(FILE)),
        (
            "release.yml".to_owned(),
            format!("{release}  set-desired:\n    uses: ./.github/workflows/{FILE}\n"),
        ),
    ];
    assert_eq!(
        caller_problems(&calling),
        vec!["release.yml: names hostkeeper-desired.yml, which only a person starts".to_owned()]
    );
}
