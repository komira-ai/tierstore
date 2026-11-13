//! `hostkeeper-desired.yml` ([`super::desired`]): the file of the repository passes, each way of
//! breaking what it is held to is named, its `id-token: write` is held by `set-desired` alone and
//! under the variable gate, it sets the lab's apt targets and waits for the fleet, and no workflow
//! reads another coordinator's variable, names `deploy-farm`, or calls the run.

use super::desired::{
    AUDIENCE, CLIENT_COMMIT, CLIENT_RELEASE, COORDINATOR_VARS, DESIRED_IF, FILE, MAX_POLLS,
    SET_DESIRED, SET_IF, caller_problems, check_command, coordinator_problems, desired_problems,
    with_wanted,
};
use super::farm::{APT_FLOOR, farm_problems};
use super::lint::lint;
use super::platforms_tests::in_job;
use super::{text_of, workflows, yaml};

#[test]
fn hostkeeper_desired_yml_checks_the_release_then_sets_it_by_its_targets() {
    // Catches: any change to hostkeeper-desired.yml that breaks what it is held to: a trigger
    // added or dropped, the gate dropped from a job, the check skipped or not handed on,
    // hostkeeper's workflow called with another release, pin, coordinator or tailnet, or without
    // its wait.
    assert_eq!(desired_problems(&text_of(FILE)), Vec::<String>::new());
    assert_eq!(lint(FILE, &text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_both_jobs() {
    // Catches: a gate on a step (the job would start, and the token exist, with the deploy off),
    // a gate on another variable or on a value other than `true`, a job that runs from a branch
    // other than main, and a release run acted on although it failed, ran on another branch or
    // came from a fork.
    for condition in [DESIRED_IF, SET_IF] {
        assert!(condition.contains("github.ref == 'refs/heads/main'"));
        assert!(condition.contains("vars.TIERSTORE_SETS_DESIRED == 'true'"));
    }
    assert!(SET_IF.contains("needs.check.outputs.set == 'true'"));
    let text = text_of(FILE);
    let check_if = |with: &str| in_job(&text, "check", DESIRED_IF, with);
    for (part, with) in [
        (
            "vars.TIERSTORE_SETS_DESIRED == 'true'",
            "vars.TIERSTORE_SETS_DESIRED != ''",
        ),
        ("github.ref == 'refs/heads/main' && ", ""),
        ("github.event.workflow_run.conclusion == 'success' && ", ""),
        ("github.event.workflow_run.head_branch == 'main' && ", ""),
        (
            " && github.event.workflow_run.head_repository.full_name == github.repository",
            "",
        ),
    ] {
        assert!(DESIRED_IF.contains(part), "{part}");
        let spoiled = check_if(&DESIRED_IF.replacen(part, with, 1));
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with("check: `if` is")),
            "check without {part:?}"
        );
    }
    let spoiled = in_job(
        &text,
        "set-desired",
        " && needs.check.outputs.set == 'true'",
        "",
    );
    assert!(
        desired_problems(&spoiled)
            .iter()
            .any(|p| p.starts_with("set-desired: `if` is")),
        "set-desired whatever check said"
    );
    for (job, condition) in [("check", DESIRED_IF), ("set-desired", SET_IF)] {
        let spoiled = in_job(&text, job, &format!("    if: {condition}\n"), "");
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p == &format!("{job}: `if` is None, not {condition}")),
            "{job} without an `if`"
        );
    }
}

#[test]
fn the_token_is_held_by_set_desired_alone_and_only_under_the_gate() {
    // Catches: `id-token: write` moved to the check job, which runs this repository's code, held
    // by set-desired under a condition that lets a run without the variable, or from another
    // branch, through, and the job that joined the tailnet to read the release channel coming
    // back with the token.
    let text = text_of(FILE);
    let spoiled = in_job(
        &text,
        "set-desired",
        SET_IF,
        "${{ github.ref == 'refs/heads/main' }}",
    );
    assert!(
        lint(FILE, &spoiled)
            .iter()
            .any(|p| p
                == &format!("job set-desired: `id-token: write` needs the exact `if: {SET_IF}`")),
        "set-desired under another condition"
    );
    let channel = text.replacen(
        "  set-desired:\n",
        &format!(
            "  channel:\n    needs: [check]\n    if: {SET_IF}\n    runs-on: ubuntu-24.04\n    permissions:\n      id-token: write\n    steps:\n      - run: true\n\n  set-desired:\n"
        ),
        1,
    );
    assert!(
        lint(FILE, &channel)
            .iter()
            .any(|p| p == "job channel: `id-token: write` is not allowed in hostkeeper-desired.yml"),
        "channel with id-token: write"
    );
    assert!(
        desired_problems(&channel).iter().any(|p| p
            == "jobs are [\"channel\", \"check\", \"set-desired\"], not check and set-desired"),
        "a third job"
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
        "      actions: read\n",
        "      actions: write\n",
        "check: permissions are [\"actions: write\", \"contents: read\"], not actions: read and contents: read",
    ),
    // Without `actions: read` the jobs of the release run cannot be read.
    (
        "check",
        "      actions: read\n",
        "",
        "check: permissions are [\"contents: read\"], not actions: read and contents: read",
    ),
    (
        "check",
        "--dir \"$RUNNER_TEMP/desired\"",
        "--dir dist",
        "check: the step `desired` does not run",
    ),
    // A run after a release that ignores which run it was: every run would set a release.
    (
        "check",
        " --after-run \"$RUN_ID\"",
        "",
        "check: the step `desired` does not run",
    ),
    // Another floor than the lab's: the same version set by both with other targets, which the
    // coordinator refuses (`release_exists`).
    (
        "check",
        "--min-hostkeeper 0.1.225",
        "--min-hostkeeper 0.1.184",
        "check: the step `desired` does not run",
    ),
    // The verb of the lab alone, which never sets `set=false` after a run that published none.
    (
        "check",
        "cargo xtask release desired ",
        "cargo xtask release desired-apt ",
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
        "RUN_ID: ${{ github.event.workflow_run.id }}",
        "RUN_ID: ${{ github.run_id }}",
        "check: the step `desired` does not read RUN_ID from ${{ github.event.workflow_run.id }}",
    ),
    (
        "check",
        "set: ${{ steps.desired.outputs.set }}",
        "set: 'true'",
        "check: output set is",
    ),
    (
        "check",
        "package-version: ${{ steps.desired.outputs.package-version }}",
        "package-version: ${{ inputs.version }}",
        "check: output package-version is",
    ),
    (
        "check",
        "targets: ${{ steps.desired.outputs.targets }}",
        "targets: '{}'",
        "check: output targets is",
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
    // The revision before the client could join the tailnet or take targets.
    (
        "set-desired",
        "set-desired.yml@cc1f624d8a01439a2468a2bde0773b49351739b3",
        "set-desired.yml@32a9cc6f8242d0efbc3476391302062ff444dd47",
        "set-desired: `uses` is not",
    ),
    (
        "set-desired",
        " # v0.1.312",
        " # v0.1.44",
        "set-desired: the pin's comment names Some(\"0.1.44\"), not v0.1.312",
    ),
    (
        "set-desired",
        " # v0.1.312",
        "",
        "set-desired: the pin's comment names None, not v0.1.312",
    ),
    (
        "set-desired",
        "service: tierstore",
        "service: hello",
        "set-desired: with.service",
    ),
    // The input, not what check found and handed on.
    (
        "set-desired",
        "version: ${{ needs.check.outputs.package-version }}",
        "version: ${{ inputs.version }}",
        "set-desired: with.version",
    ),
    // Targets dropped: a set that names no artifact.
    (
        "set-desired",
        "      targets: ${{ needs.check.outputs.targets }}\n",
        "",
        "set-desired: with.targets is None",
    ),
    // An index beside the apt targets: it must list archive entries they do not have.
    (
        "set-desired",
        "      wait: true\n",
        "      wait: true\n      release_index: ${{ needs.check.outputs.release-index }}\n",
        "set-desired: with.release_index is not an input it gives",
    ),
    (
        "set-desired",
        "      wait: true\n",
        "      wait: true\n      release_index_digest: ${{ needs.check.outputs.release-index-digest }}\n",
        "set-desired: with.release_index_digest is not an input it gives",
    ),
    // One digest for every platform, beside the targets: the client refuses it.
    (
        "set-desired",
        "      wait: true\n",
        "      wait: true\n      package_digest: ${{ needs.check.outputs.package-digest }}\n",
        "set-desired: with.package_digest is not an input it gives",
    ),
    // The call names no group, or another: it would set each group that lists tierstore.
    (
        "set-desired",
        "      host_group: all-in-one\n",
        "",
        "set-desired: with.host_group is None, not \"all-in-one\"",
    ),
    (
        "set-desired",
        "host_group: all-in-one",
        "host_group: mac",
        "set-desired: with.host_group is Some(\"mac\"), not \"all-in-one\"",
    ),
    (
        "set-desired",
        "hostkeeper_version: 0.1.312",
        "hostkeeper_version: 0.1.183",
        "set-desired: with.hostkeeper_version is Some(\"0.1.183\"), not \"0.1.312\"",
    ),
    (
        "set-desired",
        "hostkeeper_commit: cc1f624d8a01439a2468a2bde0773b49351739b3",
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
    // Without the join a GitHub-hosted runner does not reach the private listener.
    (
        "set-desired",
        "join_tailnet: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
        "join_tailnet: false",
        "set-desired: with.join_tailnet",
    ),
    (
        "set-desired",
        "tailnet_client_id: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        "tailnet_client_id: ''",
        "set-desired: with.tailnet_client_id",
    ),
    (
        "set-desired",
        "tailnet_audience: ${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        "tailnet_audience: ''",
        "set-desired: with.tailnet_audience",
    ),
    (
        "set-desired",
        "tailnet_coordinator: ${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        "tailnet_coordinator: hk-coordinator",
        "set-desired: with.tailnet_coordinator",
    ),
    // The client's default tag, which the federation of this repository may not issue.
    (
        "set-desired",
        "      tailnet_tag: ${{ vars.HOSTKEEPER_TAILNET_TAG }}\n",
        "",
        "set-desired: with.tailnet_tag is None",
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
fn a_set_that_orders_no_host_ends_the_job_red_not_green_with_not_waited() {
    // Catches: the call made without its wait (`wait: false`, or the input dropped and left to the
    // client's default), where the client ends the job green (`not_waited`) whatever the hosts do,
    // so that a version every host holds `installer_dropped` or `no_build_for_host` passes; a wait
    // the client refuses (`max_polls` above 180, `invalid_wait`) or one too short for any roll;
    // and the input left to the client's default, which another pin may change.
    let text = text_of(FILE);
    assert!(text.contains("      wait: true\n      max_polls: 180\n"));
    assert_eq!(MAX_POLLS, "180");
    for (replace, with, says) in [
        (
            "      wait: true\n",
            "      wait: false\n",
            "set-desired: with.wait is Some(\"false\"), not \"true\"",
        ),
        (
            "      wait: true\n",
            "",
            "set-desired: with.wait is None, not \"true\"",
        ),
        (
            "      max_polls: 180\n",
            "      max_polls: 181\n",
            "set-desired: with.max_polls is Some(\"181\"), not \"180\"",
        ),
        (
            "      max_polls: 180\n",
            "      max_polls: 1\n",
            "set-desired: with.max_polls is Some(\"1\"), not \"180\"",
        ),
        (
            "      max_polls: 180\n",
            "",
            "set-desired: with.max_polls is None, not \"180\"",
        ),
    ] {
        let problems = desired_problems(&in_job(&text, "set-desired", replace, with));
        assert!(
            problems.iter().any(|p| p == says),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn the_check_gives_the_floor_the_lab_stage_of_release_yml_is_held_to() {
    // Catches: hostkeeper-desired.yml and the lab stage of release.yml building the targets of
    // the same release with two hostkeeper floors, so that the second set of the version is
    // refused (`release_exists`) and the fleet is not moved; and a floor that the lab's lint
    // moves without this workflow following.
    let command = check_command();
    assert!(
        command.contains(&format!(" --min-hostkeeper {APT_FLOOR} ")),
        "{command}"
    );
    let release = text_of("release.yml");
    assert!(release.contains(&format!(
        "cargo xtask release desired-apt --repo \"$GITHUB_REPOSITORY\" --version \"${{TAG#v}}\" --min-hostkeeper {APT_FLOOR} "
    )));
    let doc = yaml::load(&release).expect("release.yml parses");
    assert_eq!(farm_problems(&doc, &release), Vec::<String>::new());
}

#[test]
fn a_trigger_an_input_or_a_job_other_than_the_two_ways_in_is_named() {
    // Catches: a push or a schedule setting a version, the completion of another workflow (ci,
    // whose success says nothing of a release) or of release on another branch, the dispatch
    // dropped (no rollback by hand), the two ways in sharing one concurrency group or a run
    // cancelled, a second input or none, an input that is not a required
    // string, a job beside the two, and a document that does not parse being read as clean.
    let text = text_of(FILE);
    for (replace, with, says) in [
        (
            "on:\n  workflow_run:\n",
            "on:\n  push:\n  workflow_run:\n",
            "triggers are [\"push\", \"workflow_dispatch\", \"workflow_run\"], not workflow_dispatch and workflow_run",
        ),
        (
            "  workflow_dispatch:\n    inputs:\n      version:\n        description: \"A released version of tierstore, X.Y.Z (the tag without the v)\"\n        type: string\n        required: true\n",
            "",
            "triggers are [\"workflow_run\"], not workflow_dispatch and workflow_run",
        ),
        (
            "    workflows: [release]\n",
            "    workflows: [ci]\n",
            "workflow_run.workflows is [\"ci\"], not [\"release\"]",
        ),
        (
            "    workflows: [release]\n",
            "    workflows: [release, ci]\n",
            "workflow_run.workflows is [\"ci\", \"release\"], not [\"release\"]",
        ),
        (
            "    types: [completed]\n",
            "    types: [requested]\n",
            "workflow_run.types is [\"requested\"], not [\"completed\"]",
        ),
        (
            "    branches: [main]\n",
            "",
            "workflow_run.branches is [], not [\"main\"]",
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
        // One group for both ways in: a waiting rollback by hand is replaced by a release run.
        (
            "  group: hostkeeper-desired-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}\n",
            "  group: hostkeeper-desired\n",
            "concurrency: group is Some(\"hostkeeper-desired\"), not",
        ),
        (
            "  cancel-in-progress: false\n",
            "  cancel-in-progress: true\n",
            "concurrency: cancel-in-progress is Some(\"true\"), not false",
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
fn the_check_runs_the_verb_and_the_call_pins_the_client_hostkeeper_releases_with() {
    // Catches: set-desired starting without the check (it needs it), the check running another
    // command than the verb whose unit tests hand on the apt targets of the testing suite
    // (src/release/desired/tests.rs), and a pin whose commit and release disagree with the
    // inputs that name them.
    let text = text_of(FILE);
    assert!(text.contains(&format!("        run: {}\n", check_command())));
    assert!(text.contains("    needs: [check]\n"));
    assert!(text.contains(&format!(
        "    uses: {SET_DESIRED}@{CLIENT_COMMIT} # v{CLIENT_RELEASE}\n"
    )));
    assert!(text.contains(&format!("      audience: {AUDIENCE}\n")));
    let wanted = with_wanted();
    assert!(wanted.contains(&("hostkeeper_commit", CLIENT_COMMIT)));
    assert!(wanted.contains(&("hostkeeper_version", CLIENT_RELEASE)));
    for absent in ["package_digest", "release_index", "release_index_digest"] {
        assert!(!wanted.iter().any(|(k, _)| *k == absent), "{absent}");
    }
}

#[test]
fn no_workflow_reads_another_coordinator_variable_names_deploy_farm_or_calls_the_run() {
    // Catches: a job that sets the version through another coordinator than hostkeeper's (the
    // farm coordinator's variable or verb coming back), and release.yml or another workflow
    // calling hostkeeper-desired.yml (it starts by its own trigger once release.yml completes).
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
        vec!["release.yml: names hostkeeper-desired.yml, which no other workflow calls".to_owned()]
    );
}
