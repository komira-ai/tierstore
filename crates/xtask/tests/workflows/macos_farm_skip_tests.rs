//! The jobs of artifacts.yml after its two macOS jobs ([`super::macos_farm::after_macos`]): a run
//! of the committed file is played out job by job as GitHub decides whether a job runs, to show
//! that the macOS job a push to main skips stops nothing after it, while a job that failed stops
//! every job after it; and the rule that holds those jobs' `if:`
//! ([`super::macos_farm::after_macos_problems`]) is shown to refuse each condition that would let
//! the skipped macOS job skip them, by putting the defect into the repository's file and reading
//! the exact problems the lint reports.

use std::collections::BTreeMap;

use super::expr::{Context, holds};
use super::graph::{ancestors, jobs};
use super::lint::{entries, entry};
use super::macos_farm::{AFTER_MACOS, CALLER, after_macos, after_macos_problems};
use super::platforms_tests::in_job;
use super::text_of;
use super::yaml::{self, Node};

const SUCCESS: &str = "success";
const FAILURE: &str = "failure";
const SKIPPED: &str = "skipped";

/// The hosted macOS job.
const HOSTED: &str = "macos-arm64";

/// The jobs after the two macOS jobs in the committed file, in its order.
const AFTER: [&str; 4] = ["sign-pkg", "check-pkg", "sums", "attest"];

/// `check-pkg`'s and `attest`'s `if:` lines as they were when a skipped macOS job skipped them.
const OLD_CHECK_PKG: &str =
    "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
const OLD_ATTEST: &str = OLD_CHECK_PKG;
const CHECK_PKG_LINE: &str = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.sign-pkg.result == 'success' }}\n";
const ATTEST_LINE: &str = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.sums.result == 'success' }}\n";

/// The run an event starts: its name, its ref and the farm's variable.
struct Run {
    event: &'static str,
    reference: &'static str,
    farm: &'static str,
}

/// A push to main, with the farm's variable `farm`.
fn push(farm: &'static str) -> Run {
    Run {
        event: "push",
        reference: "refs/heads/main",
        farm,
    }
}

/// The outcome of each job of the workflow `text` in the run `run`, as GitHub decides it: a job
/// runs when its `if:` holds, read as `success() && (...)` when it names no status function,
/// where `success()` is that every job it waits for, at any depth, succeeded; `failure()` is that
/// one of them failed, and the run is never cancelled. A job in `failing` fails when it runs, and
/// every other job that runs succeeds.
fn play(text: &str, run: &Run, failing: &[&str]) -> BTreeMap<String, &'static str> {
    let jobs = jobs(&yaml::load(text).expect("the workflow parses"));
    let mut outcomes: BTreeMap<String, &'static str> = BTreeMap::new();
    while outcomes.len() < jobs.len() {
        let before = outcomes.len();
        for (name, info) in &jobs {
            if outcomes.contains_key(name) || !info.needs.iter().all(|n| outcomes.contains_key(n)) {
                continue;
            }
            let above: Vec<&str> = ancestors(&jobs, name).iter().map(|a| outcomes[a]).collect();
            let mut names = BTreeMap::from([
                ("github.event_name".to_owned(), run.event.to_owned()),
                ("github.ref".to_owned(), run.reference.to_owned()),
                ("vars.MACOS_PACKAGE_ON_FARM".to_owned(), run.farm.to_owned()),
            ]);
            for need in &info.needs {
                names.insert(format!("needs.{need}.result"), outcomes[need].to_owned());
            }
            let context = Context {
                names,
                cancelled: false,
                failed: above.contains(&FAILURE),
            };
            let all_succeeded = above.iter().all(|o| *o == SUCCESS);
            let runs = if info.condition.is_empty() {
                all_succeeded
            } else {
                let condition = format!("${{{{ {} }}}}", info.condition);
                holds(&condition, &context).unwrap_or_else(|e| panic!("job {name}: {e}"))
                    && (info.has_status() || all_succeeded)
            };
            let outcome = match (runs, failing.contains(&name.as_str())) {
                (false, _) => SKIPPED,
                (true, true) => FAILURE,
                (true, false) => SUCCESS,
            };
            outcomes.insert(name.clone(), outcome);
        }
        assert!(
            outcomes.len() > before,
            "the needs of the workflow form a cycle"
        );
    }
    outcomes
}

/// The jobs of `outcomes` that ended as `want`, sorted.
fn ended<'a>(outcomes: &'a BTreeMap<String, &'static str>, want: &str) -> Vec<&'a str> {
    outcomes
        .iter()
        .filter(|(_, o)| **o == want)
        .map(|(n, _)| n.as_str())
        .collect()
}

/// The document of the workflow `text`.
fn doc_of(text: &str) -> Node {
    yaml::load(text).expect("the workflow parses")
}

/// What [`after_macos_problems`] reports for the jobs of the workflow `text`.
fn after_problems_of(text: &str) -> Vec<String> {
    let doc = doc_of(text);
    after_macos_problems(entry(&doc, "jobs").map_or(&[][..], entries))
}

/// The two problems [`after_macos_problems`] reports for the job `name` under an `if:` with no
/// status function that does not test the result of `need`.
fn old_shape_problems(name: &str, need: &str) -> [String; 2] {
    [
        format!(
            "{name}: waits for [\"{HOSTED}\", \"{CALLER}\"], one of which a push to main skips, so its `if:` must start with `{AFTER_MACOS}`: without a status function GitHub skips it with the skipped one"
        ),
        format!(
            "{name}: its `if:` does not test needs.{need}.result, so `!cancelled()` lets it run when {need} did not pass"
        ),
    ]
}

/// artifacts.yml with `check-pkg` and `attest` under the conditions they had when the release
/// found no sums, no signed installer and no bundles.
fn old_artifacts_yml() -> String {
    let text = in_job(
        &text_of("artifacts.yml"),
        "check-pkg",
        CHECK_PKG_LINE,
        OLD_CHECK_PKG,
    );
    in_job(&text, "attest", ATTEST_LINE, OLD_ATTEST)
}

/// Catches the artifacts run that made no `sha256sums`, no `assets-pkg-macos-arm64` and no
/// `bundles`: on a push to main, the farm configured or not, every job runs but the macOS job of
/// the other kind, so sign-pkg, check-pkg, sums and attest all succeed.
#[test]
fn a_push_to_main_skips_only_the_macos_job_that_is_not_used() {
    let text = text_of("artifacts.yml");
    for (farm, idle) in [("true", HOSTED), ("false", CALLER), ("", CALLER)] {
        let outcomes = play(&text, &push(farm), &[]);
        assert_eq!(ended(&outcomes, SKIPPED), [idle], "farm {farm:?}");
        assert_eq!(
            ended(&outcomes, FAILURE),
            Vec::<&str>::new(),
            "farm {farm:?}"
        );
        for job in AFTER {
            assert_eq!(outcomes[job], SUCCESS, "farm {farm:?}: {job}");
        }
    }
}

/// The simulation shows the defect it guards: under the old `if:` of check-pkg and attest, a push
/// to main skips check-pkg, sums and attest behind the skipped macOS job (as artifacts run
/// 45343212448 did), while sign-pkg, whose `if:` had `!cancelled()`, still succeeds; and the lint
/// refuses the file with exactly the two problems of each of the two jobs.
#[test]
fn the_old_conditions_skip_the_checked_installer_the_sums_and_the_signing_and_are_refused() {
    let text = old_artifacts_yml();
    for farm in ["true", "false"] {
        let outcomes = play(&text, &push(farm), &[]);
        assert_eq!(outcomes["sign-pkg"], SUCCESS, "farm {farm:?}");
        for job in ["check-pkg", "sums", "attest"] {
            assert_eq!(outcomes[job], SKIPPED, "farm {farm:?}: {job}");
        }
    }
    let mut want = old_shape_problems("check-pkg", "sign-pkg").to_vec();
    want.extend(old_shape_problems("attest", "sums"));
    assert_eq!(after_problems_of(&text), want);
}

/// A job that fails stops every job that waits for it, at any depth, and no other: the running
/// macOS job (hosted or on the farm), the signing of the installer, its check, either Linux
/// build, the version and the sums.
#[test]
fn a_failed_job_skips_every_job_after_it_and_no_other() {
    let text = text_of("artifacts.yml");
    let all = jobs(&doc_of(&text));
    for (farm, idle, failing) in [
        ("false", CALLER, HOSTED),
        ("true", HOSTED, CALLER),
        ("true", HOSTED, "sign-pkg"),
        ("false", CALLER, "check-pkg"),
        ("true", HOSTED, "linux-arm64"),
        ("false", CALLER, "linux-x86_64"),
        ("true", HOSTED, "sums"),
        ("false", CALLER, "version"),
    ] {
        let outcomes = play(&text, &push(farm), &[failing]);
        assert_eq!(outcomes[failing], FAILURE, "{failing}");
        for (name, outcome) in &outcomes {
            let after = ancestors(&all, name).contains(failing);
            let want = if after || name == idle {
                SKIPPED
            } else if name == failing {
                FAILURE
            } else {
                SUCCESS
            };
            assert_eq!(*outcome, want, "{failing} failed: {name}");
        }
    }
}

/// A pull request runs the version, the linux-x86_64 build and the sums alone; a run by hand also
/// builds linux-arm64 and the hosted macOS job and sums them, and signs nothing: the jobs after
/// the macOS jobs that test for a push to main are skipped, though no job failed.
#[test]
fn a_pull_request_and_a_run_by_hand_sign_nothing() {
    let text = text_of("artifacts.yml");
    let pull_request = Run {
        event: "pull_request",
        reference: "refs/pull/1/merge",
        farm: "true",
    };
    let outcomes = play(&text, &pull_request, &[]);
    assert_eq!(
        ended(&outcomes, SUCCESS),
        ["linux-x86_64", "sums", "version"]
    );
    let by_hand = Run {
        event: "workflow_dispatch",
        reference: "refs/heads/topic",
        farm: "true",
    };
    let outcomes = play(&text, &by_hand, &[]);
    assert_eq!(
        ended(&outcomes, SKIPPED),
        ["attest", "check-pkg", CALLER, "sign-pkg"]
    );
    assert_eq!(ended(&outcomes, FAILURE), Vec::<&str>::new());
}

/// The jobs held to the rule are exactly those that wait for a macOS job, at any depth, in the
/// order of the file: `sums` waits for both directly, `attest` only through `sums`; the builds and
/// the version are not among them, nor are the two macOS jobs themselves.
#[test]
fn the_jobs_after_the_macos_jobs_are_those_that_wait_for_one_at_any_depth() {
    let doc = doc_of(&text_of("artifacts.yml"));
    assert_eq!(
        after_macos(entry(&doc, "jobs").map_or(&[][..], entries)),
        AFTER
    );
    assert_eq!(
        after_problems_of(&text_of("artifacts.yml")),
        Vec::<String>::new()
    );
}

/// Each way a job after the macOS jobs could be skipped with the skipped one, or run when a job
/// it needs did not pass, is refused by exactly its problem: `always()` in place of `!cancelled()`
/// (a cancelled run would sign), `!cancelled()` at the end rather than the start, and the test of
/// the result of the job it needs left out (check-pkg would check a pkg sign-pkg never made; attest
/// would sign sums that were never made). `version`, which attest needs only beside `sums`, which
/// waits for it, need not be tested.
#[test]
fn each_condition_that_lets_the_skipped_macos_job_through_is_refused() {
    let text = text_of("artifacts.yml");
    let [no_status, no_test] = old_shape_problems("check-pkg", "sign-pkg");
    let [_, no_sums] = old_shape_problems("attest", "sums");
    let cases = [
        (
            "check-pkg",
            "    if: ${{ !cancelled() &&",
            "    if: ${{ always() &&",
            vec![no_status.clone()],
        ),
        (
            "check-pkg",
            CHECK_PKG_LINE,
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.sign-pkg.result == 'success' && !cancelled() }}\n",
            vec![no_status.clone()],
        ),
        (
            "check-pkg",
            " && needs.sign-pkg.result == 'success'",
            "",
            vec![no_test.clone()],
        ),
        ("check-pkg", CHECK_PKG_LINE, "", vec![no_status, no_test]),
        (
            "attest",
            " && needs.sums.result == 'success'",
            "",
            vec![no_sums],
        ),
    ];
    for (job, replace, with, want) in cases {
        let changed = in_job(&text, job, replace, with);
        assert_eq!(
            after_problems_of(&changed),
            want,
            "{job}: {replace:?} -> {with:?}"
        );
    }
}

/// The walk of the needs follows any depth, reads a `needs` written as a scalar, ends on a cycle,
/// and asks for the result of a need only when no other need of the job waits for it.
#[test]
fn the_walk_of_the_needs_follows_any_depth_and_ends_on_a_cycle() {
    let text = "jobs:
  macos-arm64:
    runs-on: macos-15
  a:
    needs: macos-arm64
    if: ${{ !cancelled() && needs.macos-arm64.result == 'success' }}
  b:
    needs: [a]
  c:
    needs: [a, b]
    if: ${{ !cancelled() && needs.b.result == 'success' }}
  x:
    needs: [y]
  y:
    needs: [x]
";
    let doc = doc_of(text);
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    assert_eq!(after_macos(jobs), ["a", "b", "c"]);
    let [no_status, no_test] = old_shape_problems("b", "a");
    assert_eq!(after_macos_problems(jobs), [no_status, no_test]);
}
