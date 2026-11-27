//! The rules of `farm.yml` ([`super::farm_lane`]), the farm lane: the one job that holds
//! `id-token: write` on a pull request, and only under its exact `if:`; farm-connect pinned by
//! commit in that file alone, with exactly the variables it is given; the triggers, the runner,
//! the rights, and the build through komira's buck2-farm action after farm-connect and the farm
//! check, with the tool built from komira's sources at the actions' one commit. Each rule is shown
//! to refuse its defect by putting it into the committed workflow and reading the exact problems
//! the lint reports.

use super::farm_lane::{
    BUCK2_FARM, BUILD_INPUTS, CHECK_RUN, CONNECT_INPUTS, FARM_CONNECT, FILE, JOB_IF, ON_PROBLEM,
    RUNNER, TOOL_RUN, unversioned_pin_allowed,
};
use super::lint::lint;
use super::text_of;

/// The committed farm.yml.
fn committed() -> String {
    text_of(FILE)
}

/// The committed farm.yml with its one occurrence of `from` replaced by `to`.
fn with(from: &str, to: &str) -> String {
    let text = committed();
    assert_eq!(text.matches(from).count(), 1, "{from:?} once in farm.yml");
    text.replace(from, to)
}

/// The problems of the committed farm.yml with `from` replaced by `to`.
fn problems_with(from: &str, to: &str) -> Vec<String> {
    lint(FILE, &with(from, to))
}

/// The line (from 1) of the committed farm.yml that holds `needle`.
fn line_of(needle: &str) -> usize {
    committed()
        .lines()
        .position(|l| l.contains(needle))
        .expect("the text is in farm.yml")
        + 1
}

/// The `uses:` value of farm-connect in the committed file.
fn pinned() -> String {
    let at = committed();
    let start = at.find(FARM_CONNECT).expect("farm-connect is used");
    at[start..].lines().next().expect("its line").to_owned()
}

/// The problem of the permissions of the job `build` that are not exactly its two.
const PERMISSIONS: &str =
    "job build: permissions must be exactly contents: read and id-token: write";

/// The problem of an `id-token: write` under another `if:` than the job's.
fn needs_the_if() -> String {
    format!("job build: `id-token: write` needs the exact `if: {JOB_IF}`")
}

#[test]
fn the_committed_farm_yml_passes_every_rule() {
    // Catches: a farm.yml that the rules below would refuse, or rules that refuse the file they
    // were written for.
    assert_eq!(lint(FILE, &committed()), Vec::<String>::new());
}

#[test]
fn the_token_is_held_only_under_the_jobs_exact_if() {
    // Catches: the job's `if:` loosened (a fork's pull request let in, Dependabot's branches let
    // in, the condition dropped) while it still holds `id-token: write`.
    assert_eq!(
        problems_with(&format!("    if: {JOB_IF}\n"), ""),
        [needs_the_if()],
        "no if:"
    );
    for condition in [
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository }}",
        "${{ always() }}",
    ] {
        assert_eq!(
            problems_with(JOB_IF, condition),
            [needs_the_if()],
            "{condition}"
        );
    }
}

#[test]
fn the_token_in_another_job_or_another_workflow_is_refused() {
    // Catches: the exception keyed on the file alone (another job of farm.yml), or on the job name
    // alone (the same job in another workflow that runs for a pull request).
    let renamed = with("\njobs:\n  build:\n", "\njobs:\n  lane:\n");
    assert_eq!(
        lint(FILE, &renamed),
        [
            "job lane: `id-token: write` is not allowed in farm.yml".to_owned(),
            "farm.yml: no job build".to_owned(),
            "job lane is not a job of farm.yml".to_owned(),
        ]
    );
    let line = line_of(FARM_CONNECT);
    assert_eq!(
        lint("ci.yml", &committed()),
        [
            format!(
                "line {line}: `uses: {}` has no version comment (`# v1.2.3`) on its line",
                pinned()
            ),
            "job build: `id-token: write` is not allowed in ci.yml".to_owned(),
        ]
    );
}

#[test]
fn a_second_job_is_refused() {
    // Catches: a job added beside `build`, which the lint's other rules would let through.
    let second = with(
        "\njobs:\n",
        "\njobs:\n  other:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n",
    );
    assert_eq!(lint(FILE, &second), ["job other is not a job of farm.yml"]);
}

#[test]
fn no_other_write_scope_is_allowed_in_the_job() {
    // Catches: a second write scope beside the token, the token's companion widened, or every
    // scope granted at once.
    assert_eq!(
        problems_with("      contents: read\n", "      contents: write\n"),
        [
            "job build: `contents: write` is not allowed in farm.yml".to_owned(),
            PERMISSIONS.to_owned(),
        ]
    );
    assert_eq!(
        problems_with(
            "      contents: read\n",
            "      contents: read\n      packages: read\n"
        ),
        [PERMISSIONS]
    );
    let all = with(
        "    permissions:\n      contents: read\n      # For farm-connect's tailnet join alone: the OIDC token the farm's network trusts.\n      id-token: write\n",
        "    permissions: write-all\n",
    );
    assert_eq!(
        lint(FILE, &all),
        [
            "job build: `permissions` is not a mapping of scopes".to_owned(),
            PERMISSIONS.to_owned(),
        ]
    );
}

#[test]
fn only_farm_connect_goes_without_a_version_comment_and_only_pinned_by_commit() {
    // Catches: another action in farm.yml without a version comment, and farm-connect at a branch
    // or a short commit, which the exception must not let through.
    let pin = pinned();
    let other = pin.replace(FARM_CONNECT, "komira-ai/komira/.github/actions/other");
    let line = line_of(FARM_CONNECT);
    let problems = problems_with(&pin, &other);
    assert!(
        problems.contains(&format!(
            "line {line}: `uses: {other}` has no version comment (`# v1.2.3`) on its line"
        )),
        "{problems:?}"
    );
    for at in ["main", "a26616d"] {
        let moved = format!("{FARM_CONNECT}@{at}");
        let problems = problems_with(&pin, &moved);
        assert!(
            problems.contains(&format!(
                "line {line}: `uses: {FARM_CONNECT}@{at}` is not pinned to a 40-digit lowercase commit SHA"
            )),
            "{at}: {problems:?}"
        );
    }
}

#[test]
fn exactly_one_step_uses_farm_connect() {
    // Catches: the join replaced by another action (whose name only starts like farm-connect's),
    // or a second join with other inputs beside the first.
    let pin = pinned();
    let lookalike = pin.replace(FARM_CONNECT, &format!("{FARM_CONNECT}-v2"));
    assert!(problems_with(&pin, &lookalike).contains(&format!(
        "job build: exactly one step must use {FARM_CONNECT}"
    )));
    let twice = with(
        "      - name: Connect to the farm\n",
        &format!(
            "      - uses: {pin}\n        with:\n          farm-address: elsewhere\n      - name: Connect to the farm\n"
        ),
    );
    assert!(lint(FILE, &twice).contains(&format!(
        "job build: exactly one step must use {FARM_CONNECT}"
    )));
}

#[test]
fn farm_connect_takes_exactly_its_variables() {
    // Catches: an input added (a secret, the token), one taken from pull-request data, one
    // missing, or one that is not a plain value.
    let inputs =
        format!("job build: the farm-connect step's `with:` must be exactly {CONNECT_INPUTS:?}");
    let tls = "          tls: ${{ vars.FARM_TLS || 'false' }}\n";
    for changed in [
        format!("{tls}          token: ${{{{ secrets.GITHUB_TOKEN }}}}\n"),
        "          tls: ${{ github.event.pull_request.title }}\n".to_owned(),
        String::new(),
        "          tls: [true]\n".to_owned(),
    ] {
        assert_eq!(
            problems_with(tls, &changed),
            std::slice::from_ref(&inputs),
            "{changed:?}"
        );
    }
}

/// The committed buck2-farm step's `uses:` line, from its value on.
fn build_pin() -> String {
    let at = committed();
    let start = at.find(BUCK2_FARM).expect("buck2-farm is used");
    at[start..].lines().next().expect("its line").to_owned()
}

/// The committed step that runs `run`, from its `- name:` line to the end of its `run:` line.
fn step_running(run: &str) -> String {
    let text = committed();
    let at = text.find(run).expect("the run is in farm.yml");
    let end = at + text[at..].find('\n').expect("the line ends") + 1;
    let start = text[..end]
        .rfind("      - name:")
        .expect("the step is named");
    text[start..end].to_owned()
}

#[test]
fn the_build_goes_through_buck2_farm_with_exactly_its_inputs() {
    // Catches: a build of another target, an input added or missing (the job's own buck2 not
    // passed, so the action would run a `./buck2` this repository does not have), or the action
    // replaced by a plain buck2 build that no watchdog watches and that checks no `local: 0`.
    let inputs =
        format!("job build: the buck2-farm step's `with:` must be exactly {BUILD_INPUTS:?}");
    let buck2 = "          buck2: ${{ runner.temp }}/bin/buck2\n";
    for (from, to) in [
        (
            "          args: build //:ci\n",
            "          args: build //:lint\n",
        ),
        (buck2, ""),
        (buck2, &format!("{buck2}          extra: x\n")),
    ] {
        assert_eq!(
            problems_with(from, to),
            std::slice::from_ref(&inputs),
            "{to:?}"
        );
    }
    let plain = with(
        &format!("        uses: {}\n", build_pin()),
        "        run: buck2 build //:ci\n",
    );
    assert_eq!(
        lint(FILE, &plain),
        [
            format!("job build: exactly one step must use {BUCK2_FARM}"),
            "job build: no step uses buck2-farm".to_owned(),
            format!(
                "job build, step 11 (buck2 build //:ci on the farm, local 0): runs `buck2 build` itself; a buck2 build goes through {BUCK2_FARM}, whose watchdog ends a run hung on a dead connection to the farm"
            ),
        ]
    );
}

#[test]
fn the_tool_sources_the_tool_the_join_the_check_and_the_build_come_in_order() {
    // Catches: the farm check dropped (a build with no farm says so only at analysis) or moved
    // after the build, and the tool built before its sources are there.
    let check = step_running(CHECK_RUN);
    assert_eq!(
        problems_with(&check, ""),
        ["job build: no step runs the farm check"]
    );
    let late = with(&check, "").replace(
        "      # The daemon outlives the build",
        &format!("{check}      # The daemon outlives the build"),
    );
    assert_eq!(
        lint(FILE, &late),
        [
            "job build: the step that runs the farm check must come before the step that uses buck2-farm"
        ]
    );
    let tool = step_running(TOOL_RUN);
    let early = with(&tool, "").replace(
        "      - name: Install Zig",
        &format!("{tool}      - name: Install Zig"),
    );
    assert_eq!(
        lint(FILE, &early),
        [
            "job build: the step that checks out komira's sources must come before the step that builds assert_remote"
        ]
    );
    assert_eq!(
        problems_with(TOOL_RUN, &TOOL_RUN.replace("ReleaseSafe", "Debug")),
        ["job build: no step builds assert_remote"]
    );
}

#[test]
fn the_actions_and_the_tool_sources_are_at_one_komira_commit() {
    // Catches: buck2-farm moved to a commit whose tool is not the one built here, or the tool
    // built from other sources than the action that runs it, or a checkout with no ref (komira's
    // main at the time of the run).
    let pin = build_pin();
    let commit = pin.split_once('@').expect("pinned").1.to_owned();
    let other = "0123456789abcdef0123456789abcdef01234567";
    let mut both = vec![commit.clone(), other.to_owned()];
    both.sort_unstable();
    let one = format!(
        "job build: farm-connect, buck2-farm and komira's sources must be at one komira commit, not {both:?}"
    );
    assert_eq!(
        problems_with(&pin, &pin.replace(&commit, other)),
        std::slice::from_ref(&one)
    );
    let source_ref = format!("          ref: {commit}\n");
    assert_eq!(
        problems_with(&source_ref, &format!("          ref: {other}\n")),
        std::slice::from_ref(&one)
    );
    assert_eq!(
        problems_with(&source_ref, ""),
        [format!(
            "job build: farm-connect, buck2-farm and komira's sources must be at one komira commit, not [\"\", \"{commit}\"]"
        )]
    );
}

#[test]
fn only_farm_connect_and_buck2_farm_go_unversioned_and_only_in_farm_yml() {
    // Catches: the exception for komira's two actions let through in another workflow, or for
    // another komira action, or for an action at no ref.
    let at = "@0123456789abcdef0123456789abcdef01234567";
    for action in [FARM_CONNECT, BUCK2_FARM] {
        assert!(unversioned_pin_allowed(FILE, &format!("{action}{at}")));
        assert!(!unversioned_pin_allowed("ci.yml", &format!("{action}{at}")));
        assert!(!unversioned_pin_allowed(FILE, action));
    }
    assert!(!unversioned_pin_allowed(
        FILE,
        &format!("komira-ai/komira/.github/actions/other{at}")
    ));
}

#[test]
fn the_triggers_are_every_pull_request_and_a_push_to_main() {
    // Catches: a filtered pull request, a push of another branch or of more branches, a push by
    // tags, another trigger beside them, `branches` as a scalar, and the list form of `on`.
    let on = "on:\n  pull_request:\n  push:\n    branches: [main]\n";
    for other in [
        "on:\n  pull_request:\n    paths: [crates/**]\n  push:\n    branches: [main]\n",
        "on:\n  pull_request:\n  push:\n    branches: [dev]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main, dev]\n",
        "on:\n  pull_request:\n  push:\n    branches: main\n",
        "on:\n  pull_request:\n  push:\n    tags: ['v*']\n",
        "on:\n  pull_request:\n  push:\n    branches: [main]\n    paths: [crates/**]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        "on: [pull_request, push]\n",
    ] {
        assert_eq!(problems_with(on, other), [ON_PROBLEM], "{other}");
    }
}

#[test]
fn the_job_runs_on_its_one_runner() {
    // Catches: the job moved to another runner label, even another Linux x86_64 one.
    assert_eq!(
        problems_with(
            &format!("    runs-on: {RUNNER}\n"),
            "    runs-on: ubuntu-latest\n"
        ),
        [format!("job build: runs-on must be {RUNNER}")]
    );
}
