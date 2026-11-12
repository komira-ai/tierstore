//! `hostkeeper-desired.yml` ([`super::desired`]): the file of the repository passes, each way of
//! breaking what it is held to is named, its `id-token: write` is held by `channel` and
//! `set-desired` alone and under the variable gate, the release channel is read only by
//! `channel` and only after its tailnet join, and no workflow reads another coordinator's
//! variable, names `deploy-farm`, or calls the run.

use super::desired::{
    AUDIENCE, CHANNEL_SAME, CHANNEL_WAIT, CHECK, CLIENT_COMMIT, CLIENT_RELEASE, COORDINATOR_VARS,
    DESIRED_IF, FILE, JOIN_IF, JOIN_WITH, RELEASE_CHANNEL, SET_DESIRED, SET_IF, TAILSCALE,
    caller_problems, coordinator_problems, desired_problems, with_wanted,
};
use super::lint::lint;
use super::platforms_tests::in_job;
use super::{text_of, workflows};

#[test]
fn hostkeeper_desired_yml_checks_the_release_then_sets_it_by_its_targets() {
    // Catches: any change to hostkeeper-desired.yml that breaks what it is held to: a trigger
    // added or dropped, the gate dropped from a job, the check or the channel wait skipped or not
    // handed on, hostkeeper's workflow called with another release, pin, coordinator or tailnet.
    assert_eq!(desired_problems(&text_of(FILE)), Vec::<String>::new());
    assert_eq!(lint(FILE, &text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_all_three_jobs() {
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
    for job in ["channel", "set-desired"] {
        let spoiled = in_job(&text, job, " && needs.check.outputs.set == 'true'", "");
        assert!(
            desired_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with(&format!("{job}: `if` is"))),
            "{job} whatever check said"
        );
    }
    for (job, condition) in [
        ("check", DESIRED_IF),
        ("channel", SET_IF),
        ("set-desired", SET_IF),
    ] {
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
fn the_token_is_held_by_channel_and_set_desired_alone_and_only_under_the_gate() {
    // Catches: `id-token: write` moved to the check job, which runs this repository's code, or
    // held by channel or set-desired under a condition that lets a run without the variable, or
    // from another branch, through.
    let text = text_of(FILE);
    for job in ["channel", "set-desired"] {
        let spoiled = in_job(&text, job, SET_IF, "${{ github.ref == 'refs/heads/main' }}");
        assert!(
            lint(FILE, &spoiled)
                .iter()
                .any(|p| p
                    == &format!("job {job}: `id-token: write` needs the exact `if: {SET_IF}`")),
            "{job} under another condition"
        );
    }
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
        "release-index: ${{ steps.desired.outputs.release-index }}",
        "release-index: ''",
        "check: output release-index is",
    ),
    (
        "check",
        "release-index-digest: ${{ steps.desired.outputs.release-index-digest }}",
        "release-index-digest: ''",
        "check: output release-index-digest is",
    ),
    (
        "check",
        "targets: ${{ steps.desired.outputs.targets }}",
        "targets: '{}'",
        "check: output targets is",
    ),
    // The channel job: the join dropped, skipped while the client ID is set, run on another
    // condition, or given another identity, tag, Tailscale release or a cache; the wait or the
    // comparison dropped, made conditional or reading another value; another step (a checkout,
    // which would run this repository's code beside the token); other permissions or needs.
    (
        "channel",
        "    needs: [check]\n",
        "    needs: [check, set-desired]\n",
        "channel: needs are [\"check\", \"set-desired\"], not [\"check\"]",
    ),
    (
        "channel",
        "      id-token: write\n",
        "      id-token: write\n      contents: read\n",
        "channel: permissions are [\"contents: read\", \"id-token: write\"], not id-token: write alone",
    ),
    (
        "channel",
        "      id-token: write\n",
        "      id-token: none\n",
        "channel: permissions are [\"id-token: none\"], not id-token: write alone",
    ),
    (
        "channel",
        "        uses: tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd # v4.2.0\n",
        "        uses: tailscale/github-action@6986d2c82a91fbac2949fe01f5bab95cf21b5102 # v3.2.2\n",
        "channel: the first step does not use tailscale/github-action@d1b6cd204f8dceda5b3eaad7f1f767be390056cd",
    ),
    (
        "channel",
        "        if: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}\n",
        "        if: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID == '' }}\n",
        "channel: the join's `if` is not ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
    ),
    (
        "channel",
        "        if: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}\n",
        "        if: false\n",
        "channel: the join's `if` is not",
    ),
    (
        "channel",
        "        if: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}\n",
        "",
        "channel: the join's `if` is not",
    ),
    (
        "channel",
        "oauth-client-id: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        "oauth-client-id: ${{ vars.OTHER_CLIENT_ID }}",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "audience: ${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        "audience: tierstore",
        "channel: the join's inputs are",
    ),
    // The action's default tag, which the federation of this repository may not issue.
    (
        "channel",
        "          tags: ${{ vars.HOSTKEEPER_TAILNET_TAG }}\n",
        "",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "version: 1.94.2",
        "version: latest",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "          sha256sum: c6f99a5d774c7783b56902188d69e9756fc3dddfb08ac6be4cb2585f3fecdc32\n",
        "",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "use-cache: false",
        "use-cache: true",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "          use-cache: false\n",
        "          use-cache: false\n          ping: ${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}\n",
        "channel: the join's inputs are",
    ),
    (
        "channel",
        "--retry-max-time 1800",
        "--retry-max-time 60",
        "channel: step 2 does not run `curl",
    ),
    (
        "channel",
        "        run: printf",
        "        run: true || printf",
        "channel: step 3 does not run `printf",
    ),
    (
        "channel",
        "        env:\n          RELEASE_CHANNEL",
        "        if: ${{ always() }}\n        env:\n          RELEASE_CHANNEL",
        "channel: the wait has `if` Some(\"${{ always() }}\"); it runs whenever the job does",
    ),
    (
        "channel",
        "        env:\n          INDEX_DIGEST",
        "        if: ${{ success() }}\n        env:\n          INDEX_DIGEST",
        "channel: the comparison has `if` Some(\"${{ success() }}\")",
    ),
    (
        "channel",
        "RELEASE_CHANNEL: ${{ vars.HOSTKEEPER_RELEASE_CHANNEL }}",
        "RELEASE_CHANNEL: https://channel.example",
        "channel: the wait reads",
    ),
    (
        "channel",
        "PACKAGE_VERSION: ${{ needs.check.outputs.package-version }}",
        "PACKAGE_VERSION: ${{ inputs.version }}",
        "channel: the wait reads",
    ),
    (
        "channel",
        "          PACKAGE_VERSION: ${{ needs.check.outputs.package-version }}\n",
        "          PACKAGE_VERSION: ${{ needs.check.outputs.package-version }}\n          CURL_HOME: .\n",
        "channel: the wait reads",
    ),
    (
        "channel",
        "INDEX_DIGEST: ${{ needs.check.outputs.release-index-digest }}",
        "INDEX_DIGEST: ${{ needs.check.outputs.package-version }}",
        "channel: the comparison reads",
    ),
    (
        "channel",
        "    steps:\n",
        "    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        "channel: has 4 steps, not the join, the wait and the comparison",
    ),
    (
        "set-desired",
        "needs: [check, channel]",
        "needs: [check]",
        "set-desired: needs are [\"check\"], not [\"channel\", \"check\"]",
    ),
    (
        "set-desired",
        "needs: [check, channel]",
        "needs: [check, channel, publish]",
        "set-desired: needs are [\"channel\", \"check\", \"publish\"], not [\"channel\", \"check\"]",
    ),
    (
        "set-desired",
        "    needs: [check, channel]\n",
        "",
        "set-desired: needs are [], not [\"channel\", \"check\"]",
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
        "set-desired.yml@2c767d1842a88edb7120073d2dff3b4bfccc834c",
        "set-desired.yml@32a9cc6f8242d0efbc3476391302062ff444dd47",
        "set-desired: `uses` is not",
    ),
    (
        "set-desired",
        " # v0.1.184",
        " # v0.1.44",
        "set-desired: the pin's comment names Some(\"0.1.44\"), not v0.1.184",
    ),
    (
        "set-desired",
        " # v0.1.184",
        "",
        "set-desired: the pin's comment names None, not v0.1.184",
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
    (
        "set-desired",
        "release_index: ${{ needs.check.outputs.release-index }}",
        "release_index: ''",
        "set-desired: with.release_index is",
    ),
    (
        "set-desired",
        "release_index_digest: ${{ needs.check.outputs.release-index-digest }}",
        "release_index_digest: ''",
        "set-desired: with.release_index_digest",
    ),
    // Targets dropped: the release named by its index alone.
    (
        "set-desired",
        "      targets: ${{ needs.check.outputs.targets }}\n",
        "",
        "set-desired: with.targets is None",
    ),
    // One digest for every platform, beside the targets: the client refuses it.
    (
        "set-desired",
        "      wait: false\n",
        "      wait: false\n      package_digest: ${{ needs.check.outputs.release-index-digest }}\n",
        "set-desired: with.package_digest is not an input it gives",
    ),
    (
        "set-desired",
        "      wait: false\n",
        "      wait: false\n      max_polls: 180\n",
        "set-desired: with.max_polls is not an input it gives",
    ),
    (
        "set-desired",
        "hostkeeper_version: 0.1.184",
        "hostkeeper_version: 0.1.183",
        "set-desired: with.hostkeeper_version is Some(\"0.1.183\"), not \"0.1.184\"",
    ),
    (
        "set-desired",
        "hostkeeper_commit: 2c767d1842a88edb7120073d2dff3b4bfccc834c",
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
    // A wait would hold the group for a staged rollout, and fail when it is longer.
    (
        "set-desired",
        "wait: false",
        "wait: true",
        "set-desired: with.wait is Some(\"true\"), not \"false\"",
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
fn the_channel_is_read_only_after_the_tailnet_join_and_compared_after_the_wait() {
    // Catches: the channel read before the runner joins the tailnet (a tailnet-only channel
    // cannot be reached, and the wait runs out its half hour), the comparison made before the
    // served index was downloaded, and the wait moved back into check, or into set-desired,
    // where no join comes before it.
    let text = text_of(FILE);
    let join_step = format!(
        "      - name: Join the tailnet\n        if: {JOIN_IF}\n        uses: {TAILSCALE} # v4.2.0\n        with:\n{}",
        JOIN_WITH
            .iter()
            .map(|(k, v)| format!("          {k}: {v}\n"))
            .collect::<String>()
    );
    let wait_step = format!(
        "      - name: Wait until the release channel serves the release's index\n        env:\n          RELEASE_CHANNEL: ${{{{ vars.{RELEASE_CHANNEL} }}}}\n          PACKAGE_VERSION: ${{{{ needs.check.outputs.package-version }}}}\n        run: {CHANNEL_WAIT}\n"
    );
    let same_step = format!(
        "      - name: The channel serves the release's index, byte for byte\n        env:\n          INDEX_DIGEST: ${{{{ needs.check.outputs.release-index-digest }}}}\n        run: {CHANNEL_SAME}\n"
    );
    for step in [&join_step, &wait_step, &same_step] {
        assert!(text.contains(step.as_str()), "{step}");
    }
    let wait_first = text.replacen(&join_step, "", 1).replacen(
        &same_step,
        &format!("{same_step}{join_step}"),
        1,
    );
    let problems = desired_problems(&wait_first);
    assert!(
        problems.contains(&format!(
            "channel: the first step does not use {TAILSCALE}; the channel is read before the tailnet join"
        )),
        "{problems:?}"
    );
    let swapped = text.replacen(&same_step, "", 1).replacen(
        &wait_step,
        &format!("{same_step}{wait_step}"),
        1,
    );
    let problems = desired_problems(&swapped);
    assert!(
        problems.contains(&format!("channel: step 2 does not run `{CHANNEL_WAIT}`")),
        "{problems:?}"
    );
    assert!(
        problems.contains(&format!("channel: step 3 does not run `{CHANNEL_SAME}`")),
        "{problems:?}"
    );
    // The join dropped: the channel's wait is then the first step.
    let unjoined = text.replacen(&join_step, "", 1);
    let problems = desired_problems(&unjoined);
    assert!(
        problems.contains(
            &"channel: has 2 steps, not the join, the wait and the comparison".to_owned()
        ),
        "{problems:?}"
    );
    assert!(
        problems.contains(&format!(
            "channel: the first step does not use {TAILSCALE}; the channel is read before the tailnet join"
        )),
        "{problems:?}"
    );
    // The wait back in check, where it was before, without a join: named.
    let output = "--output \"$GITHUB_OUTPUT\"\n";
    assert!(text.contains(output));
    let in_check = in_job(&text, "check", output, &format!("{output}{wait_step}"));
    assert!(
        desired_problems(&in_check).contains(&format!(
            "check: names {RELEASE_CHANNEL}; only channel, after the tailnet join, reads it"
        )),
        "the wait in check"
    );
    let in_set = in_job(
        &text,
        "set-desired",
        "      wait: false\n",
        &format!("      wait: false\n      channel: ${{{{ vars.{RELEASE_CHANNEL} }}}}\n"),
    );
    assert!(
        desired_problems(&in_set).contains(&format!(
            "set-desired: names {RELEASE_CHANNEL}; only channel, after the tailnet join, reads it"
        )),
        "the channel read in set-desired"
    );
}

#[test]
fn a_trigger_an_input_or_a_job_other_than_the_two_ways_in_is_named() {
    // Catches: a push or a schedule setting a version, the completion of another workflow (ci,
    // whose success says nothing of a release) or of release on another branch, the dispatch
    // dropped (no rollback by hand), the two ways in sharing one concurrency group or a run
    // cancelled, a second input or none, an input that is not a required
    // string, a job beside the three, and a document that does not parse being read as clean.
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
            "jobs are [\"channel\", \"check\", \"publish\", \"set-desired\"], not check, channel and set-desired",
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
    // command than the verb whose unit tests refuse an index without a platform
    // (src/release/desired/tests.rs), and a pin whose commit and release disagree with the
    // inputs that name them.
    let text = text_of(FILE);
    assert!(text.contains(&format!("        run: {CHECK}\n")));
    assert!(text.contains("    needs: [check]\n"));
    assert!(text.contains(&format!(
        "    uses: {SET_DESIRED}@{CLIENT_COMMIT} # v{CLIENT_RELEASE}\n"
    )));
    assert!(text.contains(&format!("      audience: {AUDIENCE}\n")));
    let wanted = with_wanted();
    assert!(wanted.contains(&("hostkeeper_commit", CLIENT_COMMIT)));
    assert!(wanted.contains(&("hostkeeper_version", CLIENT_RELEASE)));
    assert!(!wanted.iter().any(|(k, _)| *k == "package_digest"));
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
