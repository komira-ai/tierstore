//! The farm jobs of `release.yml` ([`super::farm`]): the file passes, each way of leaving the farm
//! out of the order, giving a job more than it needs, or calling hostkeeper with other inputs is
//! named, and every job that makes the release public is shown to wait for `promote-check`, which waits for the soak.

use super::desired::{TAILNET_COORDINATOR, coordinator_problems};
use super::farm::{FARM_IF, SOAK_IF, farm_problems, pin_problems};
use super::lint::{lint, release_problems};
use super::platforms_tests::{each_named, in_job};
use super::{text_of, yaml};

const GATE_IF: &str = "        if: ${{ steps.gate.outputs.enabled == 'true' }}\n";

#[test]
fn the_farm_jobs_of_release_yml_pass_and_hold_what_the_design_gives_them() {
    // Catches: a farm job lost or renamed, which the defects below would not notice, and a
    // release.yml that no longer passes the whole lint with them in it.
    let text = text_of("release.yml");
    let doc = yaml::load(&text).expect("release.yml parses");
    assert_eq!(farm_problems(&doc, &text), Vec::<String>::new());
    assert_eq!(release_problems(&text), Vec::<String>::new());
    assert_eq!(lint("release.yml", &text), Vec::<String>::new());
    assert!(FARM_IF.contains("needs.farm-check.outputs.enabled == 'true'"));
    assert!(SOAK_IF.contains("needs.farm.outputs.converged == 'true'"));
}

/// `(job, text to replace once in it, its replacement, what the problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    // farm-check
    (
        "farm-check",
        "needs: [plan, check-apt, check-brew, check-install-sh]",
        "needs: [plan]",
        "farm-check: needs are not plan and the three install checks",
    ),
    (
        "farm-check",
        "    if: ${{ !cancelled() && !failure() && needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}\n",
        "",
        "farm-check: `if` is not",
    ),
    (
        "farm-check",
        "    timeout-minutes: 45\n",
        "    timeout-minutes: 45\n    environment: release\n",
        "farm-check: names an environment",
    ),
    (
        "farm-check",
        "      contents: read\n    outputs:",
        "      contents: read\n      id-token: write\n    outputs:",
        "farm-check: permissions are",
    ),
    (
        "farm-check",
        "      enabled: ${{ steps.gate.outputs.enabled }}",
        "      enabled: 'true'",
        "farm-check: output enabled is not ${{ steps.gate.outputs.enabled }}",
    ),
    (
        "farm-check",
        "      tailnet: ${{ steps.tailnet.outputs.enabled }}",
        "      tailnet: 'true'",
        "farm-check: output tailnet is not",
    ),
    (
        "farm-check",
        "      package-digest: ${{ steps.desired.outputs.package-digest }}",
        "      package-digest: ''",
        "farm-check: output package-digest is not",
    ),
    (
        "farm-check",
        "        id: gate\n",
        "        id: check\n",
        "farm-check: no step has the id `gate`",
    ),
    (
        "farm-check",
        "channels gate --label farm --output",
        "channels gate --label farmx --output",
        "farm-check: the gate does not run `cargo xtask channels gate --label farm ...`",
    ),
    (
        "farm-check",
        " --setting \"HOSTKEEPER_COORDINATOR_CA=${HOSTKEEPER_COORDINATOR_CA:+set}\"",
        "",
        "farm-check: the gate is not given --setting \"HOSTKEEPER_COORDINATOR_CA=${HOSTKEEPER_COORDINATOR_CA:+set}\"",
    ),
    (
        "farm-check",
        "\"HOSTKEEPER_COORDINATOR_URL=${HOSTKEEPER_COORDINATOR_URL:+set}\"",
        "\"HOSTKEEPER_COORDINATOR_URL=${{ vars.HOSTKEEPER_COORDINATOR_URL }}\"",
        "farm-check: the gate's command line holds a value",
    ),
    (
        "farm-check",
        "--setting \"HOSTKEEPER_COORDINATOR_CA=${HOSTKEEPER_COORDINATOR_CA:+set}\"\n",
        "--setting \"HOSTKEEPER_COORDINATOR_CA=${HOSTKEEPER_COORDINATOR_CA:+set}\" --setting \"X=${X:+set}\"\n",
        "farm-check: the gate is given a setting it should not be",
    ),
    (
        "farm-check",
        " --setting \"HOSTKEEPER_TAILNET_AUDIENCE=${HOSTKEEPER_TAILNET_AUDIENCE:+set}\"",
        "",
        "farm-check: the tailnet gate is not given --setting \"HOSTKEEPER_TAILNET_AUDIENCE=",
    ),
    (
        "farm-check",
        "channels gate --label farm-tailnet ",
        "channels gate --label farm ",
        "farm-check: the tailnet gate does not run `cargo xtask channels gate --label farm-tailnet ...`",
    ),
    (
        "farm-check",
        "        id: tailnet\n",
        "        id: net\n",
        "farm-check: no step has the id `tailnet`",
    ),
    (
        "farm-check",
        "        id: desired\n        if: ${{ steps.gate.outputs.enabled == 'true' }}\n",
        "        id: desired\n",
        "farm-check: the step `Refuse a release the testing suite does not list with the digests of its SHA256SUMS` after the gate does not have",
    ),
    (
        "farm-check",
        "run: cargo xtask release desired-deb --repo",
        "run: echo --repo",
        "farm-check: no step `desired` runs `cargo xtask release desired-deb` for the tag of this release",
    ),
    (
        "farm-check",
        "--version \"${TAG#v}\" --dir",
        "--version \"$TAG\" --dir",
        "farm-check: no step `desired` runs `cargo xtask release desired-deb` for the tag of this release",
    ),
    (
        "farm-check",
        "      - name: Gate on the settings of the coordinator",
        "      - run: echo ${{ vars.HOSTKEEPER_COORDINATOR_URL }}\n      - name: Gate on the settings of the coordinator",
        "farm-check: a step before the gate names vars.HOSTKEEPER",
    ),
    // farm
    (
        "farm",
        "needs: [plan, farm-check]",
        "needs: [plan, publish]",
        "farm: needs are not plan and farm-check",
    ),
    (
        "farm",
        " && needs.farm-check.outputs.enabled == 'true'",
        "",
        "farm: `if` is not",
    ),
    (
        "farm",
        "      id-token: write\n    uses:",
        "    uses:",
        "farm: permissions are [\"contents: read\"], not contents: read and id-token: write",
    ),
    (
        "farm",
        "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
        "komira-ai/hostkeeper/.github/workflows/other.yml@",
        "farm: `uses` is not komira-ai/hostkeeper/.github/workflows/set-desired.yml@<commit>",
    ),
    (
        "farm",
        "      wait: true\n",
        "      wait: false\n",
        "farm: with.wait is",
    ),
    (
        "farm",
        "      service: tierstore\n",
        "      service: hostkeeper\n",
        "farm: with.service is",
    ),
    (
        "farm",
        "      version: ${{ needs.farm-check.outputs.package-version }}\n",
        "      version: ${{ needs.plan.outputs.tag }}\n",
        "farm: with.version is",
    ),
    (
        "farm",
        "      package_digest: ${{ needs.farm-check.outputs.package-digest }}\n",
        "      package_digest: ''\n",
        "farm: with.package_digest is",
    ),
    (
        "farm",
        "      audience: tierstore-hostkeeper-release\n",
        "      audience: tierstore-hostkeeper-desired\n",
        "farm: with.audience is",
    ),
    (
        "farm",
        "      coordinator_url: ${{ vars.HOSTKEEPER_COORDINATOR_URL }}\n",
        "      coordinator_url: ${{ vars.OTHER_URL }}\n",
        "farm: with.coordinator_url is",
    ),
    (
        "farm",
        "      join_tailnet: ${{ needs.farm-check.outputs.tailnet == 'true' }}\n",
        "      join_tailnet: true\n",
        "farm: with.join_tailnet is",
    ),
    (
        "farm",
        "      tailnet_client_id: ${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}\n",
        "      tailnet_client_id: abc\n",
        "farm: with.tailnet_client_id is",
    ),
    (
        "farm",
        "      hostkeeper_commit: 1adad60c2a7c0136b9f46cb79a987883f2185445\n",
        "      hostkeeper_commit: 2321ed897b4b9269471eadf169d7f29989161b4a\n",
        "farm: with.hostkeeper_commit is",
    ),
    (
        "farm",
        "      hostkeeper_version: 0.1.33\n",
        "      hostkeeper_version: 0.1.66\n",
        "farm: with.hostkeeper_version is",
    ),
    // soak
    (
        "soak",
        "needs: [plan, farm-check, farm]",
        "needs: [plan, farm-check]",
        "soak: needs are not plan, farm-check and farm",
    ),
    (
        "soak",
        " && needs.farm.outputs.converged == 'true'",
        "",
        "soak: `if` is not",
    ),
    (
        "soak",
        "    if: ${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.farm.outputs.converged == 'true' }}\n",
        "    if: ${{ always() }}\n",
        "soak: `if` is not",
    ),
    (
        "soak",
        "      contents: read\n    env:",
        "      contents: read\n      id-token: write\n    env:",
        "soak: permissions are",
    ),
    (
        "soak",
        "    timeout-minutes: 345\n",
        "    timeout-minutes: 345\n    environment: release\n",
        "soak: names an environment",
    ),
    (
        "soak",
        "cargo xtask channels soak",
        "echo soak",
        "soak: it needs one step that runs `cargo xtask channels soak`",
    ),
    (
        "soak",
        "--hours \"${FARM_SOAK_HOURS:-0}\" ",
        "",
        "soak: the soak is not given --hours \"${FARM_SOAK_HOURS:-0}\"",
    ),
    (
        "soak",
        "--reached-at \"$(date +%s)\" ",
        "",
        "soak: the soak is not given --reached-at \"$(date +%s)\"",
    ),
    (
        "soak",
        " --version \"$PACKAGE_VERSION\"",
        "",
        "soak: the soak is not given --version \"$PACKAGE_VERSION\"",
    ),
    (
        "soak",
        "FARM_SOAK_HOURS: ${{ vars.FARM_SOAK_HOURS }}",
        "FARM_SOAK_HOURS: 0",
        "soak: env FARM_SOAK_HOURS is not ${{ vars.FARM_SOAK_HOURS }}",
    ),
    (
        "soak",
        "PACKAGE_VERSION: ${{ needs.farm-check.outputs.package-version }}",
        "PACKAGE_VERSION: ${{ needs.plan.outputs.tag }}",
        "soak: env PACKAGE_VERSION is not",
    ),
    // the jobs that make the release public wait for promote-check, which waits for the lab
    (
        "promote-latest",
        "needs: [plan, promote-check, testing-upload]",
        "needs: [plan, testing-upload]",
        "promote-latest: does not wait for promote-check",
    ),
    (
        "tap",
        "needs: [plan, promote-check, formula]",
        "needs: [plan, formula]",
        "tap: does not wait for promote-check",
    ),
    (
        "stable-apt-read",
        "needs: [plan, promote-check, testing-apt-read]",
        "needs: [plan, testing-apt-read]",
        "stable-apt-read: does not wait for promote-check",
    ),
    (
        "stable-apt-index",
        "needs: [plan, promote-check, stable-apt-read, testing-apt-index]",
        "needs: [plan, stable-apt-read, testing-apt-index]",
        "stable-apt-index: needs are not",
    ),
    (
        "stable-apt-read-3",
        "needs: [plan, stable-apt-write-2]",
        "needs: [plan]",
        "stable-apt-read-3: does not wait for promote-check",
    ),
    (
        "promote-check",
        ", soak]",
        "]",
        "promote-check: does not wait for soak, so it would decide without its result",
    ),
];

#[test]
fn each_defect_in_the_farm_jobs_and_their_order_is_named_by_the_lint() {
    // Catches: a pin of the farm jobs that holds nothing, because the file passes whether or not
    // the thing it pins is there; and a public job that no longer waits for the promotion check.
    each_named("release.yml", release_problems, DEFECTS);
}

#[test]
fn the_farm_job_is_the_only_one_with_the_coordinators_token_and_only_under_its_condition() {
    // Catches: `id-token: write` moving to farm-check or soak (jobs that run this repository's
    // code), and `farm` holding it under a condition that lets a run without the gate through.
    let text = text_of("release.yml");
    for job in ["farm-check", "soak"] {
        let spoiled = in_job(
            &text,
            job,
            "    permissions:\n",
            "    permissions:\n      id-token: write\n",
        );
        assert!(
            lint("release.yml", &spoiled).iter().any(
                |p| p == &format!("job {job}: `id-token: write` is not allowed in release.yml")
            ),
            "{job}"
        );
    }
    let spoiled = in_job(
        &text,
        "farm",
        " && needs.farm-check.outputs.enabled == 'true'",
        "",
    );
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p.starts_with("job farm: `id-token: write` needs the exact `if:")),
        "{:?}",
        lint("release.yml", &spoiled)
    );
    let spoiled = in_job(
        &text,
        "farm",
        "      contents: read\n",
        "      contents: write\n",
    );
    assert!(
        lint("release.yml", &spoiled)
            .iter()
            .any(|p| p == "job farm: `contents: write` is not allowed in release.yml"),
        "{:?}",
        lint("release.yml", &spoiled)
    );
    // Not in another file either.
    assert!(
        lint("ci.yml", &text)
            .iter()
            .any(|p| p == "job farm: `id-token: write` is not allowed in ci.yml")
    );
}

#[test]
fn a_farm_job_that_is_missing_or_a_soak_without_steps_is_named() {
    // Catches: a farm job deleted, and a farm-check with no gate or a soak with no step, which the
    // rules about the steps would otherwise skip.
    let text = "jobs: {}\n";
    let doc = yaml::load(text).expect("parses");
    assert_eq!(
        farm_problems(&doc, text),
        [
            "farm-check: there is no such job",
            "farm: there is no such job",
            "soak: there is no such job"
        ]
    );
    let short = "jobs:
  farm-check:
    steps:
      - run: echo
  farm:
    uses: komira-ai/hostkeeper/.github/workflows/set-desired.yml@abc # v0.1.0
  soak:
    steps:
      - run: echo
";
    let problems = farm_problems(&yaml::load(short).expect("parses"), short);
    for says in [
        "farm-check: no step has the id `gate`",
        "farm: with.service is None, not \"tierstore\"",
        "soak: it needs one step that runs `cargo xtask channels soak`",
    ] {
        assert!(problems.contains(&says.to_owned()), "{says}: {problems:?}");
    }
}

#[test]
fn farm_calls_the_same_hostkeeper_release_as_the_run_by_hand() {
    // Catches: the coordinator's allow-list naming the `job_workflow_ref` of one pin while the
    // farm job calls another, so that its token is refused after a bump of one file alone.
    let release = text_of("release.yml");
    let desired = text_of("hostkeeper-desired.yml");
    assert_eq!(pin_problems(&release, &desired), Vec::<String>::new());
    let bumped = desired.replace(
        "1adad60c2a7c0136b9f46cb79a987883f2185445 # v0.1.33",
        "2321ed897b4b9269471eadf169d7f29989161b4a # v0.1.66",
    );
    assert_ne!(bumped, desired);
    let problems = pin_problems(&release, &bumped);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("farm: the pin Some((\"1adad60c"),
        "{problems:?}"
    );
    // The comment is part of the pin: a release named differently is another pin.
    let renamed = release.replace("# v0.1.33", "# v0.1.34");
    assert_eq!(pin_problems(&renamed, &desired).len(), 1);
    // A farm job that is not hostkeeper's workflow has no pin at all.
    let other = release.replace(
        "komira-ai/hostkeeper/.github/workflows/set-desired.yml@",
        "komira-ai/hostkeeper/.github/workflows/other.yml@",
    );
    assert!(pin_problems(&other, &desired)[0].starts_with("farm: the pin None"));
    // A file that does not parse is named, not skipped.
    assert!(!pin_problems("a: [", &desired).is_empty());
    assert!(!pin_problems(&release, "a: [").is_empty());
}

#[test]
fn the_tailnet_name_is_the_one_coordinator_variable_besides_hostkeepers_two() {
    // Catches: the rule that keeps a farm coordinator's variables out of the workflows being
    // loosened for any name with COORDINATOR in it, or tightened until the join cannot be set.
    let read = |name: &str| format!("      x: ${{{{ vars.{name} }}}}\n");
    assert_eq!(
        coordinator_problems("t.yml", &read(TAILNET_COORDINATOR)),
        Vec::<String>::new()
    );
    let problems = coordinator_problems("t.yml", &read("HOSTKEEPER_TAILNET_COORDINATOR_OLD"));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("reads vars.HOSTKEEPER_TAILNET_COORDINATOR_OLD"),
        "{problems:?}"
    );
}
