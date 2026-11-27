//! The rules of `farm.yml` ([`super::farm_lane`]), the farm lane: the one job that holds
//! `id-token: write` on a pull request, and only under its exact `if:`; farm-connect pinned by
//! commit in that file alone, with exactly the variables it is given; the triggers, the runner,
//! the rights, and the build through komira's buck2-farm action after farm-connect and the farm
//! check, with the tool built from komira's sources at the actions' one commit; and the four steps
//! after it that offer the regenerated buildrules tables of a failed build. Each rule is shown
//! to refuse its defect by putting it into the committed workflow and reading the exact problems
//! the lint reports.

use super::farm_lane::{
    BUCK2_FARM, BUILD_INPUTS, CHECK_RUN, CONNECT_INPUTS, FARM_CONNECT, FILE, JOB_IF, OFFER_IF,
    OFFER_INPUTS, OFFER_ORDER_PROBLEM, ON_PROBLEM, RUNNER, SUMMARY_IF, SUMMARY_RUN,
    SUMMARY_TOOL_ID, SUMMARY_TOOL_IF, SUMMARY_TOOL_INPUTS, SYNC_ID, SYNC_IF, SYNC_INPUTS, TOOL_RUN,
    UPLOAD, unversioned_pin_allowed,
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

/// The `- name:` line of the step that builds `//:ci`.
const BUILD_STEP: &str = "      - name: buck2 build //:ci on the farm, local 0\n";

/// The problems of the committed farm.yml with the one occurrence of `from` in the step that
/// builds `//:ci` (the steps of the sync patch repeat its `uses:` and two of its inputs) replaced
/// by `to`.
fn problems_in_build_with(from: &str, to: &str) -> Vec<String> {
    let text = committed();
    let start = text
        .find(BUILD_STEP)
        .expect("the build step is in farm.yml");
    let end = start
        + BUILD_STEP.len()
        + text[start + BUILD_STEP.len()..]
            .find("\n      - ")
            .expect("a step follows the build");
    let step = &text[start..end];
    assert_eq!(
        step.matches(from).count(),
        1,
        "{from:?} once in the build step"
    );
    let changed = format!(
        "{}{}{}",
        &text[..start],
        step.replace(from, to),
        &text[end..]
    );
    lint(FILE, &changed)
}

/// The line (from 1) of the committed farm.yml that holds `needle`.
fn line_of(needle: &str) -> usize {
    committed()
        .lines()
        .position(|l| l.contains(needle))
        .expect("the text is in farm.yml")
        + 1
}

/// The lines (from 1) of the committed farm.yml that hold `needle`, in order.
fn lines_of(needle: &str) -> Vec<usize> {
    committed()
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(at, _)| at + 1)
        .collect()
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
        lines_of(BUCK2_FARM).len(),
        3,
        "the build and the two steps of the sync patch"
    );
    assert_eq!(
        lint("ci.yml", &committed()),
        std::iter::once(format!(
            "line {line}: `uses: {}` has no version comment (`# v1.2.3`) on its line",
            pinned()
        ))
        .chain(lines_of(BUCK2_FARM).into_iter().map(|line| {
            format!(
                "line {line}: `uses: {}` has no version comment (`# v1.2.3`) on its line",
                build_pin()
            )
        }))
        .chain(["job build: `id-token: write` is not allowed in ci.yml".to_owned()])
        .collect::<Vec<_>>()
    );
}

#[test]
fn a_second_job_is_refused() {
    // Catches: a job added beside `build`, which the lint's other rules would let through.
    let second = with(
        "\njobs:\n",
        "\njobs:\n  other:\n    runs-on: ubuntu-24.04\n    timeout-minutes: 30\n    steps:\n      - run: true\n        timeout-minutes: 10\n",
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
            problems_in_build_with(from, to),
            std::slice::from_ref(&inputs),
            "{to:?}"
        );
    }
    assert_eq!(
        problems_in_build_with(
            &format!("        uses: {}\n", build_pin()),
            "        run: buck2 build //:ci\n",
        ),
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
        problems_in_build_with(&pin, &pin.replace(&commit, other)),
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

/// The problem of a job with no step that builds `//:sync_patch` as it must.
fn no_sync() -> String {
    format!(
        "job build: no step `id: {SYNC_ID}` uses buck2-farm under `if: {SYNC_IF}` with the `with:` {SYNC_INPUTS:?}"
    )
}

/// The problem of a job with no step that uploads the patch as it must.
fn no_upload() -> String {
    format!("job build: no {UPLOAD} step under `if: {OFFER_IF}` has the `with:` {OFFER_INPUTS:?}")
}

/// The problem of a job with no step that builds the summary tool as it must.
fn no_summary_tool() -> String {
    format!(
        "job build: no step `id: {SUMMARY_TOOL_ID}` uses buck2-farm under `if: {SUMMARY_TOOL_IF}` with the `with:` {SUMMARY_TOOL_INPUTS:?}"
    )
}

/// The problem of a job with no step that writes the summary as it must.
fn no_summary() -> String {
    format!("job build: no step runs `{SUMMARY_RUN}` under `if: {SUMMARY_IF}`")
}

/// The problem of a second step that uses buck2-farm beside the build.
fn second_build() -> String {
    format!("job build: exactly one step must use {BUCK2_FARM}")
}

/// The committed farm.yml cut at its four steps of the sync patch: what comes before them, each
/// of the four (the sync, the upload, the tool, the summary), and what comes after them.
fn offer_steps() -> [String; 6] {
    let text = committed();
    let at = |needle: &str| text.find(needle).expect("the step is in farm.yml");
    let cuts = [
        at("      - name: Regenerate the generated files"),
        at("      - name: Offer the patch"),
        at("      - name: Build buildrules' sync_patch tool"),
        at("      - name: Say what the patch is"),
        at("      # The daemon outlives the build"),
    ];
    assert!(
        cuts.windows(2).all(|p| p[0] < p[1]),
        "the steps in farm.yml's order"
    );
    [
        text[..cuts[0]].to_owned(),
        text[cuts[0]..cuts[1]].to_owned(),
        text[cuts[1]..cuts[2]].to_owned(),
        text[cuts[2]..cuts[3]].to_owned(),
        text[cuts[3]..cuts[4]].to_owned(),
        text[cuts[4]..].to_owned(),
    ]
}

#[test]
fn a_failed_build_is_followed_by_the_sync_of_the_patch_on_the_farm() {
    // Catches: the sync of another target, under another condition, or with its output elsewhere
    // than the upload and the summary read.
    let args = "          args: build //:sync_patch --out ${{ runner.temp }}/sync-patch\n";
    for (from, to) in [
        (
            args,
            "          args: build //:ci --out ${{ runner.temp }}/sync-patch\n",
        ),
        (
            args,
            "          args: build //:sync_patch --out ${{ runner.temp }}/elsewhere\n",
        ),
        ("        if: failure()\n", "        if: always()\n"),
    ] {
        assert_eq!(problems_with(from, to), [no_sync()], "{to}");
    }
    // Catches: the sync step renamed, which would leave a second build through buck2-farm and no
    // id for the steps after it to read.
    assert_eq!(
        problems_with("        id: sync_patch\n", "        id: sync\n"),
        [second_build(), no_sync()]
    );
}

#[test]
fn the_patch_is_uploaded_as_the_artifact_sync_patch_only_when_the_sync_finished() {
    // Catches: the artifact renamed (the summary's commands download `sync-patch`), the whole
    // output uploaded instead of `offer/`, an empty offer failing the step, an input added, and
    // an upload attempted after a sync that did not finish.
    let path = "          path: ${{ runner.temp }}/sync-patch/offer/\n";
    let empty = "          if-no-files-found: ignore\n";
    for (from, to) in [
        ("          name: sync-patch\n", "          name: patch\n"),
        (path, "          path: ${{ runner.temp }}/sync-patch/\n"),
        (empty, "          if-no-files-found: error\n"),
        (empty, ""),
        (
            empty,
            "          if-no-files-found: ignore\n          retention-days: 1\n",
        ),
        (
            "        if: failure() && steps.sync_patch.outcome == 'success'\n",
            "        if: failure()\n",
        ),
    ] {
        assert_eq!(problems_with(from, to), [no_upload()], "{to}");
    }
    let problems = problems_with(
        "uses: actions/upload-artifact@",
        "uses: actions/cache/save@",
    );
    assert!(problems.contains(&no_upload()), "{problems:?}");
}

#[test]
fn the_summary_tool_is_built_on_the_farm_whenever_the_sync_ran() {
    // Catches: another target built as the tool, the tool put where the summary does not run it,
    // the tool skipped after a sync that failed (the reader would not learn there is no patch),
    // and the step renamed so that the summary reads nothing.
    let args = "          args: build buildrules//rules/rust:sync_patch --out ${{ runner.temp }}/bin/sync_patch\n";
    for (from, to) in [
        (
            args,
            "          args: build buildrules//rules/rust:sync_patch_test --out ${{ runner.temp }}/bin/sync_patch\n",
        ),
        (
            args,
            "          args: build buildrules//rules/rust:sync_patch --out ${{ runner.temp }}/sync_patch\n",
        ),
        (
            "        if: failure() && steps.sync_patch.outcome != 'skipped'\n",
            "        if: failure() && steps.sync_patch.outcome == 'success'\n",
        ),
    ] {
        assert_eq!(problems_with(from, to), [no_summary_tool()], "{to}");
    }
    assert_eq!(
        problems_with("        id: sync_patch_tool\n", "        id: tool\n"),
        [second_build(), no_summary_tool()]
    );
}

#[test]
fn the_summary_runs_the_tool_and_names_the_artifact() {
    // Catches: a summary run when its tool was not built, commands that download another
    // artifact, a summary written to the log alone, and the summary run through `buck2 run`, which
    // no watchdog watches.
    for (from, to) in [
        (
            "        if: failure() && steps.sync_patch_tool.outcome == 'success'\n",
            "        if: failure()\n",
        ),
        ("--artifact sync-patch --to", "--artifact patch --to"),
        (" --to \"$GITHUB_STEP_SUMMARY\"", ""),
    ] {
        assert_eq!(problems_with(from, to), [no_summary()], "{to}");
    }
    let problems = problems_with(
        "'\"$RUNNER_TEMP/bin/sync_patch\" summary",
        "'\"$RUNNER_TEMP/bin/buck2\" run buildrules//rules/rust:sync_patch -- summary",
    );
    assert!(problems.contains(&no_summary()), "{problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("runs `buck2 run` itself")),
        "{problems:?}"
    );
}

#[test]
fn the_steps_of_the_patch_are_required() {
    // Catches: the steps dropped, which leaves a pull request that changes a manifest with no
    // patch and no word of one.
    let [before, _, _, _, _, after] = offer_steps();
    assert_eq!(
        lint(FILE, &format!("{before}{after}")),
        [no_sync(), no_upload(), no_summary_tool(), no_summary()]
    );
}

#[test]
fn the_steps_of_the_patch_follow_the_build_in_their_order() {
    // Catches: the summary before its tool, the upload before the sync it reads, and the four
    // moved before the build (they would run on a failure before it, never on the build's).
    let [before, sync, upload, tool, summary, after] = offer_steps();
    for reordered in [
        format!("{before}{sync}{upload}{summary}{tool}{after}"),
        format!("{before}{upload}{sync}{tool}{summary}{after}"),
    ] {
        assert_eq!(lint(FILE, &reordered), [OFFER_ORDER_PROBLEM]);
    }
    assert_eq!(before.matches(BUILD_STEP).count(), 1, "the build step once");
    let early = before.replace(
        BUILD_STEP,
        &format!("{sync}{upload}{tool}{summary}{BUILD_STEP}"),
    );
    assert_eq!(
        lint(FILE, &format!("{early}{after}")),
        [OFFER_ORDER_PROBLEM]
    );
}
