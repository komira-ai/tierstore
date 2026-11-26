//! The rules of the merge queue ([`super::merge_queue`]): the required workflow has the
//! `merge_group` trigger and an unfiltered `pull_request`, no other workflow has the trigger, its
//! jobs are exactly the required checks and each reports under its own name on a Linux x86_64
//! runner with nothing written, and no queue run is cancelled. Each rule is shown to refuse its
//! defect by putting it into the committed workflows and reading the exact problems reported.

use super::merge_queue::{CANCEL, GROUP_TAIL, REQUIRED, merge_queue_problems};
use super::{text_of, workflows};

const CI: &str = "ci.yml";

/// The committed workflows with ci.yml's one occurrence of `from` replaced by `to`.
fn ci_with(from: &str, to: &str) -> Vec<(String, String)> {
    let ci = text_of(CI);
    assert_eq!(ci.matches(from).count(), 1, "{from:?} once in ci.yml");
    let changed = ci.replace(from, to);
    workflows()
        .into_iter()
        .map(|(name, text)| {
            let text = if name == CI { changed.clone() } else { text };
            (name, text)
        })
        .collect()
}

/// The problems of the committed workflows with ci.yml's `from` replaced by `to`.
fn problems_with(from: &str, to: &str) -> Vec<String> {
    merge_queue_problems(&ci_with(from, to))
}

/// The problems when the committed ci.yml is replaced by `text` alone.
fn problems_of_ci(text: &str) -> Vec<String> {
    merge_queue_problems(&[(CI.to_owned(), text.to_owned())])
}

/// ci.yml's `on:` block, exactly.
const ON: &str = "on:\n  pull_request:\n  merge_group:\n  push:\n    branches: [main]\n";

/// The problem of a job of ci.yml named `job`.
fn job_problem(job: &str, what: &str) -> Vec<String> {
    vec![format!("ci.yml: job {job}: {what}")]
}

/// A minimal ci.yml that passes every rule, for the shapes the committed file does not have.
fn minimal(on: &str, concurrency: &str) -> String {
    format!(
        "{on}{concurrency}jobs:\n  lint:\n    runs-on: ubuntu-latest\n  test:\n    runs-on: ubuntu-24.04\n"
    )
}

#[test]
fn the_committed_workflows_pass_every_rule() {
    // Catches: a committed workflow the rules below would refuse, or rules that refuse the files
    // they were written for.
    assert_eq!(merge_queue_problems(&workflows()), Vec::<String>::new());
}

#[test]
fn the_required_checks_are_lint_and_test_of_ci() {
    // Catches: the table the ruleset is written from losing or gaining a check unseen; the
    // ruleset's required status checks name exactly these.
    assert_eq!(
        REQUIRED
            .iter()
            .map(|(f, j)| (*f, j.to_vec()))
            .collect::<Vec<_>>(),
        [("ci.yml", vec!["lint", "test"])]
    );
}

#[test]
fn a_required_workflow_without_the_trigger_is_refused() {
    // Catches: merge_group dropped from ci.yml, after which every queued pull request waits for
    // lint and test until the queue gives up on it.
    assert_eq!(
        problems_with("  merge_group:\n", ""),
        [
            "ci.yml: no merge_group trigger, so the merge queue waits for its required checks until it gives up"
        ]
    );
    assert_eq!(
        problems_of_ci(&minimal("on: [pull_request, push]\n", "")),
        [
            "ci.yml: no merge_group trigger, so the merge queue waits for its required checks until it gives up"
        ]
    );
    assert_eq!(problems_of_ci(&minimal("", "")), ["ci.yml: no `on`"]);
}

#[test]
fn a_filter_on_merge_group_is_refused_and_its_one_type_is_not() {
    // Catches: a merge_group filtered so that some queue runs start no check; the one type
    // GitHub has, in list or scalar form, is the same as no filter.
    for allowed in ["types: [checks_requested]", "types: checks_requested"] {
        assert_eq!(
            problems_with(
                "  merge_group:\n",
                &format!("  merge_group:\n    {allowed}\n")
            ),
            Vec::<String>::new(),
            "{allowed}"
        );
    }
    assert_eq!(
        problems_with("  merge_group:\n", "  merge_group: {}\n"),
        Vec::<String>::new()
    );
    for filter in [
        "    branches: [main]\n",
        "    types: [checks_requested, other]\n",
        "    types: [checks_requested]\n    branches: [main]\n",
    ] {
        assert_eq!(
            problems_with("  merge_group:\n", &format!("  merge_group:\n{filter}")),
            ["ci.yml: merge_group must have no filter (or `types: [checks_requested]`)"],
            "{filter}"
        );
    }
}

#[test]
fn a_filtered_or_missing_pull_request_trigger_is_refused() {
    // Catches: a pull request whose paths a filter skips never reports lint and test, so it can
    // never be queued; and a required workflow that no pull request starts at all.
    let problem = [
        "ci.yml: pull_request must be a trigger with no filter: a pull request it skips never reports the required checks and cannot be queued",
    ];
    for to in [
        "  pull_request:\n    paths: [crates/**]\n",
        "  pull_request:\n    branches: [main]\n",
        "",
    ] {
        assert_eq!(problems_with("  pull_request:\n", to), problem, "{to:?}");
    }
    assert_eq!(problems_of_ci(&minimal("on: merge_group\n", "")), problem);
}

#[test]
fn the_list_form_of_on_holds_no_filter_and_passes() {
    // Catches: a rule that reads only the mapping form of `on` and refuses the others.
    assert_eq!(
        problems_of_ci(&minimal("on: [pull_request, merge_group, push]\n", "")),
        Vec::<String>::new()
    );
    assert_eq!(
        problems_with(ON, "on: [pull_request, merge_group, push]\n"),
        Vec::<String>::new()
    );
}

#[test]
fn another_workflow_with_the_trigger_is_refused() {
    // Catches: a queue run that starts a workflow the queue does not wait for, such as one that
    // builds release files or holds a write scope.
    let mut files = workflows();
    let farm = files
        .iter_mut()
        .find(|(name, _)| name == "farm.yml")
        .expect("farm.yml");
    farm.1 = farm
        .1
        .replacen("  pull_request:\n", "  pull_request:\n  merge_group:\n", 1);
    assert_eq!(
        merge_queue_problems(&files),
        [
            "farm.yml: runs on merge_group, but none of its jobs is a required check of the merge queue (merge_queue::REQUIRED)"
        ]
    );
}

#[test]
fn a_missing_required_workflow_is_refused() {
    // Catches: ci.yml deleted or renamed, which leaves the ruleset's checks with no workflow.
    let files: Vec<(String, String)> = workflows().into_iter().filter(|(n, _)| n != CI).collect();
    assert_eq!(
        merge_queue_problems(&files),
        [
            r#"ci.yml: missing, and its jobs ["lint", "test"] are the required checks of the merge queue"#
        ]
    );
}

#[test]
fn a_workflow_that_is_not_yaml_is_named() {
    // Catches: a file the rules cannot read passing because nothing was found in it.
    let problems = merge_queue_problems(&[(CI.to_owned(), "jobs: [".to_owned())]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("ci.yml: line "), "{problems:?}");
}

#[test]
fn the_jobs_are_exactly_the_required_checks() {
    // Catches: a job added to ci.yml that the queue runs but does not wait for, and a required
    // job renamed or removed, whose check the queue would then wait for in vain.
    let extra = "  docs:\n    runs-on: ubuntu-latest\n";
    assert_eq!(
        problems_of_ci(&format!("{}{extra}", minimal(ON, ""))),
        [
            r#"ci.yml: jobs are ["docs", "lint", "test"], not the required checks of the merge queue ["lint", "test"]"#
        ]
    );
    assert_eq!(
        problems_with("\n  test:\n", "\n  tests:\n"),
        [
            r#"ci.yml: jobs are ["lint", "tests"], not the required checks of the merge queue ["lint", "test"]"#
        ]
    );
}

#[test]
fn a_job_that_can_be_skipped_or_renamed_is_refused() {
    // Catches: an `if:` (a skipped required check counts as passed), a matrix and a `name:` (both
    // rename the check), and a call of a reusable workflow, on a required job.
    let at = "  test:\n    runs-on: ubuntu-latest\n";
    for (add, what) in [
        (
            "    if: ${{ github.event_name != 'merge_group' }}\n",
            "has an `if:`, and a required check that is skipped counts as passed",
        ),
        (
            "    strategy:\n      matrix:\n        os: [a]\n",
            "has a `strategy`, whose matrix renames the check the queue waits for",
        ),
        (
            "    name: coverage\n",
            "has a `name:` other than its id, which renames the check the queue waits for",
        ),
    ] {
        assert_eq!(
            problems_with(at, &format!("{at}{add}")),
            job_problem("test", what),
            "{add}"
        );
    }
    assert_eq!(
        problems_with(at, &format!("{at}    name: test\n")),
        Vec::<String>::new(),
        "a name equal to the id"
    );
    let called = minimal(ON, "").replace(
        "  test:\n    runs-on: ubuntu-24.04\n",
        "  test:\n    runs-on: ubuntu-24.04\n    uses: ./.github/workflows/x.yml\n",
    );
    assert_eq!(
        problems_of_ci(&called),
        job_problem(
            "test",
            "calls a reusable workflow, whose checks are named after its jobs"
        )
    );
}

#[test]
fn a_required_job_runs_on_a_named_linux_x86_64_runner() {
    // Catches: a required check moved to macOS, Windows, an arm image, a self-hosted or larger
    // runner label, a list of labels, an expression, or no runner at all.
    let what = "must run on one named GitHub-hosted Linux x86_64 runner (ubuntu-*, not -arm)";
    for label in [
        "macos-15",
        "ubuntu-24.04-arm",
        "windows-latest",
        "[ubuntu-latest]",
        "${{ matrix.os }}",
    ] {
        assert_eq!(
            problems_with(
                "  lint:\n    runs-on: ubuntu-latest\n",
                &format!("  lint:\n    runs-on: {label}\n")
            ),
            job_problem("lint", what),
            "{label}"
        );
    }
    assert_eq!(
        problems_with("  lint:\n    runs-on: ubuntu-latest\n", "  lint:\n"),
        job_problem("lint", what)
    );
}

#[test]
fn a_required_job_with_a_write_scope_is_refused() {
    // Catches: a queue run that could publish, tag, deploy or sign.
    let at = "  lint:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: read\n";
    for scopes in [
        "      contents: write\n",
        "      contents: read\n      id-token: write\n",
    ] {
        assert_eq!(
            problems_with(
                at,
                &format!("  lint:\n    runs-on: ubuntu-latest\n    permissions:\n{scopes}")
            ),
            job_problem(
                "lint",
                "holds a write scope, and nothing is published from a merge queue's run"
            ),
            "{scopes}"
        );
    }
    assert_eq!(
        problems_with(
            at,
            "  lint:\n    runs-on: ubuntu-latest\n    permissions: write-all\n"
        ),
        job_problem(
            "lint",
            "holds a write scope, and nothing is published from a merge queue's run"
        )
    );
}

#[test]
fn a_concurrency_that_cancels_or_shares_a_queue_run_is_refused() {
    // Catches: a queue run cancelled by a newer one (cancel-in-progress true for it), and a queue
    // run made to share a group, in which GitHub cancels a run that is still waiting.
    let group = format!("  group: ci-{GROUP_TAIL}\n");
    let cancel = format!("  cancel-in-progress: {}\n", CANCEL[1]);
    for value in ["true", "${{ github.event_name != 'push' }}"] {
        assert_eq!(
            problems_with(&cancel, &format!("  cancel-in-progress: {value}\n")),
            [format!(
                "ci.yml: concurrency cancel-in-progress Some({value:?}) must be one of {CANCEL:?}, which never cancel a merge queue run"
            )],
            "{value}"
        );
    }
    for value in ["ci", "ci-${{ github.ref }}"] {
        assert_eq!(
            problems_with(&group, &format!("  group: {value}\n")),
            [format!(
                "ci.yml: concurrency group Some({value:?}) must end in {GROUP_TAIL}, a group of its own for each merge queue run"
            )],
            "{value}"
        );
    }
    // The scalar form, `concurrency: <group>`, names one group for every run.
    assert_eq!(
        problems_of_ci(&minimal(ON, "concurrency: ci\n")),
        [format!(
            "ci.yml: concurrency group None must end in {GROUP_TAIL}, a group of its own for each merge queue run"
        )]
    );
    assert_eq!(
        problems_with(&cancel, "  cancel-in-progress: [true]\n"),
        [format!(
            "ci.yml: concurrency cancel-in-progress None must be one of {CANCEL:?}, which never cancel a merge queue run"
        )]
    );
}

#[test]
fn a_concurrency_that_never_cancels_passes() {
    // Catches: rules that refuse the shapes that keep every queue run: no concurrency at all, no
    // cancel-in-progress (GitHub's default, false), and false.
    assert_eq!(problems_of_ci(&minimal(ON, "")), Vec::<String>::new());
    let group = format!("concurrency:\n  group: x-{GROUP_TAIL}\n");
    assert_eq!(problems_of_ci(&minimal(ON, &group)), Vec::<String>::new());
    assert_eq!(
        problems_of_ci(&minimal(
            ON,
            &format!("{group}  cancel-in-progress: false\n")
        )),
        Vec::<String>::new()
    );
}
