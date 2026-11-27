//! The rules of `farm.yml` ([`super::farm_lane`]), the farm lane: the one job that holds
//! `id-token: write` on a pull request, and only under its exact `if:`; farm-connect pinned by
//! commit in that file alone, with exactly the variables it is given; the triggers, the runner,
//! the rights, the build through `cargo xtask farm-build` after farm-connect, and the three steps
//! after it that offer the regenerated buildrules tables of a failed build. Each rule is shown
//! to refuse its defect by putting it into the committed workflow and reading the exact problems
//! the lint reports.

use super::farm_lane::{
    BUILD_RUN, CONNECT_INPUTS, FARM_CONNECT, FILE, JOB_IF, OFFER_IF, OFFER_INPUTS,
    OFFER_ORDER_PROBLEM, ON_PROBLEM, RUNNER, SUMMARY_IF, SUMMARY_RUN, SYNC_ID, SYNC_IF, SYNC_RUN,
    UPLOAD,
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

#[test]
fn the_job_runs_farm_build_of_ci_after_farm_connect() {
    // Catches: a build of another target, a plain `buck2 build` that checks no `local: 0`, or the
    // build moved before the join (it would find no farm).
    let no_build = format!("job build: no step runs `{BUILD_RUN}`");
    for other in [
        BUILD_RUN.replace("//:ci", "//:lint"),
        "buck2 build //:ci".to_owned(),
    ] {
        assert_eq!(
            problems_with(BUILD_RUN, &other),
            std::slice::from_ref(&no_build),
            "{other}"
        );
    }
    let build =
        format!("      - name: buck2 build //:ci on the farm, local 0\n        run: {BUILD_RUN}\n");
    let moved = with(&build, "").replace(
        "      - name: Connect to the farm\n",
        &format!("{build}      - name: Connect to the farm\n"),
    );
    assert_eq!(
        lint(FILE, &moved),
        ["job build: the build does not come after the farm-connect step"]
    );
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
    format!("job build: no step `id: {SYNC_ID}` runs `{SYNC_RUN}` under `if: {SYNC_IF}`")
}

/// The problem of a job with no step that uploads the patch as it must.
fn no_upload() -> String {
    format!("job build: no {UPLOAD} step under `if: {OFFER_IF}` has the `with:` {OFFER_INPUTS:?}")
}

/// The problem of a job with no step that writes the summary as it must.
fn no_summary() -> String {
    format!("job build: no step runs `{SUMMARY_RUN}` under `if: {SUMMARY_IF}`")
}

/// The committed farm.yml cut into: what comes before the three steps of the patch, then each of
/// the three (the sync, the upload, the summary), which end the file.
fn offer_steps() -> [String; 4] {
    let text = committed();
    let at = |needle: &str| text.find(needle).expect("the step is in farm.yml");
    let sync = at("      - name: Regenerate the generated files");
    let upload = at("      - name: Offer the patch");
    let summary = at("      - name: Say what the patch is");
    assert!(
        sync < upload && upload < summary,
        "the steps in farm.yml's order"
    );
    [
        text[..sync].to_owned(),
        text[sync..upload].to_owned(),
        text[upload..summary].to_owned(),
        text[summary..].to_owned(),
    ]
}

#[test]
fn a_failed_build_is_followed_by_the_sync_of_the_patch_on_the_farm() {
    // Catches: the sync of another target, under another condition, or without the id the two
    // steps after it read.
    for (from, to) in [
        ("build //:sync_patch --out", "build //:ci --out"),
        ("        if: failure()\n", "        if: always()\n"),
        ("        id: sync_patch\n", "        id: sync\n"),
        (
            "--out \"$RUNNER_TEMP/sync-patch\"'\n",
            "--out \"$RUNNER_TEMP/elsewhere\"'\n",
        ),
    ] {
        assert_eq!(problems_with(from, to), [no_sync()], "{to}");
    }
}

#[test]
fn the_patch_is_uploaded_as_the_artifact_sync_patch_only_when_the_sync_finished() {
    // Catches: the artifact renamed (the summary's commands download `sync-patch`), the whole
    // output uploaded instead of `offer/`, an empty offer failing the step, an input added, and
    // an upload attempted after a sync that did not finish.
    let path = "          path: ${{ runner.temp }}/sync-patch/offer/\n";
    for (from, to) in [
        ("          name: sync-patch\n", "          name: patch\n"),
        (path, "          path: ${{ runner.temp }}/sync-patch/\n"),
        (
            "          if-no-files-found: ignore\n",
            "          if-no-files-found: error\n",
        ),
        ("          if-no-files-found: ignore\n", ""),
        (
            "          if-no-files-found: ignore\n",
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
fn the_summary_runs_whenever_the_sync_ran_and_names_the_artifact() {
    // Catches: no summary after a sync that failed (the reader would not learn there is no
    // patch), commands that download another artifact, and a summary written to the log alone.
    for (from, to) in [
        (
            "        if: failure() && steps.sync_patch.outcome != 'skipped'\n",
            "        if: failure() && steps.sync_patch.outcome == 'success'\n",
        ),
        ("--artifact sync-patch --to", "--artifact patch --to"),
        (" --to \"$GITHUB_STEP_SUMMARY\"", ""),
    ] {
        assert_eq!(problems_with(from, to), [no_summary()], "{to}");
    }
}

#[test]
fn the_three_steps_of_the_patch_are_required() {
    // Catches: the steps dropped, which leaves a pull request that changes a manifest with no
    // patch and no word of one.
    let [before, ..] = offer_steps();
    assert_eq!(lint(FILE, &before), [no_sync(), no_upload(), no_summary()]);
}

#[test]
fn the_steps_of_the_patch_follow_the_build_in_their_order() {
    // Catches: the summary before the upload, the upload before the sync it reads, and the three
    // moved before the build (they would run on a farm-connect failure, never on the build's).
    let [before, sync, upload, summary] = offer_steps();
    for reordered in [
        format!("{before}{sync}{summary}{upload}"),
        format!("{before}{upload}{sync}{summary}"),
    ] {
        assert_eq!(lint(FILE, &reordered), [OFFER_ORDER_PROBLEM]);
    }
    let build = "      - name: buck2 build //:ci on the farm, local 0\n";
    assert_eq!(before.matches(build).count(), 1, "the build step once");
    let early = before.replace(build, &format!("{sync}{upload}{summary}{build}"));
    assert_eq!(lint(FILE, &early), [OFFER_ORDER_PROBLEM]);
}
