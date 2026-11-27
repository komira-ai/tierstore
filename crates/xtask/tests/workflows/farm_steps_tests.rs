//! The two rules of [`super::farm_steps`]: no step runs buck2's build itself, and every step of a
//! workflow that reaches the farm has a time limit of its own under its job's. Each rule is shown
//! to refuse its defect, in the committed workflows and in small workflows written here, by the
//! exact problems the lint reports.

use super::farm_lane::CHECK_RUN;
use super::farm_steps::{BUCK2_FARM, FARM_CONNECT, buck2_build, problems, reaches_farm};
use super::lint::lint;
use super::yaml::load;
use super::{text_of, workflows};

/// The problems [`problems`] reports for the workflow `text`.
fn problems_of(text: &str) -> Vec<String> {
    problems(&load(text).expect("the workflow parses"))
}

/// A workflow of one job (`timeout-minutes: 30`) with `steps`, each a YAML step at the indent of
/// a step.
fn one_job(steps: &str) -> String {
    format!("jobs:\n  a:\n    timeout-minutes: 30\n    steps:\n{steps}")
}

/// A pinned use of `action`.
fn pinned(action: &str) -> String {
    format!("{action}@0123456789abcdef0123456789abcdef01234567")
}

/// macos-farm.yml as committed, with its one occurrence of `from` replaced by `to`, linted.
fn macos_farm_with(from: &str, to: &str) -> Vec<String> {
    let text = text_of("macos-farm.yml");
    assert_eq!(
        text.matches(from).count(),
        1,
        "{from:?} once in macos-farm.yml"
    );
    lint("macos-farm.yml", &text.replace(from, to))
}

/// The label of macos-farm.yml's farm step in a problem.
const MAC_STEP: &str =
    "job package, step 5 (The macOS release files, built and checked on a farm Mac)";

#[test]
fn the_workflows_that_reach_the_farm_are_farm_yml_and_macos_farm_yml() {
    // Catches: a farm workflow the bound rule does not see (so its steps could go unbounded), or
    // one it sees that does not reach the farm (a tailnet join alone is not the build farm).
    let reaching: Vec<String> = workflows()
        .into_iter()
        .filter(|(_, text)| reaches_farm(&load(text).expect("parses")))
        .map(|(file, _)| file)
        .collect();
    assert_eq!(reaching, ["farm.yml", "macos-farm.yml"]);
}

#[test]
fn a_farm_step_without_its_own_bound_under_the_jobs_is_refused() {
    // Catches: a step left to the job's limit, a bound equal to or over the job's (the job's own
    // limit would end it first), a zero bound, and one that is no whole number (an expression,
    // a fraction), which this lint cannot hold under the job's.
    let bound = "        timeout-minutes: 75\n";
    let none = format!("{MAC_STEP}: no whole-number timeout-minutes of its own");
    for (to, problem) in [
        (String::new(), none.clone()),
        (
            "        timeout-minutes: 90\n".to_owned(),
            format!("{MAC_STEP}: timeout-minutes 90 is not from 1 and under the job's 90"),
        ),
        (
            "        timeout-minutes: 120\n".to_owned(),
            format!("{MAC_STEP}: timeout-minutes 120 is not from 1 and under the job's 90"),
        ),
        (
            "        timeout-minutes: 0\n".to_owned(),
            format!("{MAC_STEP}: timeout-minutes 0 is not from 1 and under the job's 90"),
        ),
        (
            "        timeout-minutes: ${{ 75 }}\n".to_owned(),
            none.clone(),
        ),
        ("        timeout-minutes: 7.5\n".to_owned(), none.clone()),
        ("        timeout-minutes: ''\n".to_owned(), none.clone()),
    ] {
        assert_eq!(macos_farm_with(bound, &to), [problem], "{to:?}");
    }
    assert_eq!(
        macos_farm_with(bound, "        timeout-minutes: 89\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_farm_job_without_a_whole_number_bound_is_refused() {
    // Catches: a job with no limit of its own (GitHub's six hours), under which no step bound is
    // held to anything.
    assert_eq!(
        macos_farm_with("    timeout-minutes: 90\n", ""),
        ["job package: reaches the farm with no whole-number timeout-minutes"]
    );
}

#[test]
fn each_way_of_reaching_the_farm_holds_every_step_to_a_bound() {
    // Catches: a farm workflow seen by only one of its marks (farm-connect, buck2-farm, the Mac
    // farm verb), a job that calls another workflow (it has no steps) held to steps, and a step
    // with no name, action or run named by nothing.
    let unbounded = "      - run: echo hi\n";
    assert_eq!(problems_of(&one_job(unbounded)), Vec::<String>::new());
    for mark in [
        format!("      - uses: {}\n", pinned(FARM_CONNECT)),
        format!("      - uses: {}\n", pinned(BUCK2_FARM)),
        "      - run: cargo xtask mac-farm --address x\n".to_owned(),
    ] {
        let text = format!(
            "{}  call:\n    uses: ./.github/workflows/x.yml\n",
            one_job(&format!("{unbounded}{mark}      - shell: bash\n"))
        );
        let first = mark.trim_start_matches("      - ").trim_end();
        let first = first.split_once(": ").expect("a key").1;
        assert_eq!(
            problems_of(&text),
            [
                "job a, step 1 (echo hi): no whole-number timeout-minutes of its own".to_owned(),
                format!("job a, step 2 ({first}): no whole-number timeout-minutes of its own"),
                "job a, step 3: no whole-number timeout-minutes of its own".to_owned(),
            ],
            "{mark}"
        );
    }
    let lookalike = format!("      - uses: {}\n", pinned(&format!("{FARM_CONNECT}-v2")));
    assert!(!reaches_farm(&load(&one_job(&lookalike)).expect("parses")));
}

#[test]
fn a_step_that_runs_buck2_build_itself_is_refused_in_any_workflow() {
    // Catches: a buck2 build, test, bxl or run outside buck2-farm (which no watchdog watches),
    // however buck2 is named or the command is joined, in a workflow that does not otherwise
    // reach the farm too.
    let text = one_job(
        "      - name: build\n        timeout-minutes: 10\n        run: cd x && \"$RUNNER_TEMP/bin/buck2\" build //:ci\n",
    );
    assert_eq!(
        problems_of(&text),
        [format!(
            "job a, step 1 (build): runs `buck2 build` itself; a buck2 build goes through {BUCK2_FARM}, whose watchdog ends a run hung on a dead connection to the farm"
        )]
    );
}

#[test]
fn buck2_is_found_by_its_name_and_its_first_word_that_is_not_a_flag() {
    // Catches: buck2 found only as a bare word, behind a separator or a flag of its own missed,
    // and the steps that name buck2 without building (the stop, the audit, the check, the
    // install) or a word that only contains its name refused.
    for (run, sub) in [
        ("buck2 build //:ci", "build"),
        ("./buck2 test //x:y", "test"),
        ("'$RUNNER_TEMP/bin/buck2' bxl //a:b.bxl:main", "bxl"),
        ("cd x && buck2 --isolation-dir=y run //t", "run"),
        ("echo hi; buck2 build x", "build"),
        ("(buck2 build x)", "build"),
        ("`buck2 test x`", "test"),
        ("true | buck2 build x", "build"),
    ] {
        assert_eq!(buck2_build(run), Some(sub), "{run}");
    }
    for run in [
        "\"$RUNNER_TEMP/bin/buck2\" kill",
        "buck2 audit config k --style json",
        CHECK_RUN,
        "cargo xtask fetch --out \"$RUNNER_TEMP/bin/buck2\" --extract zst",
        "buck2",
        "buck2 --version",
        "echo buck2build",
        "mybuck2 build x",
        "",
    ] {
        assert_eq!(buck2_build(run), None, "{run}");
    }
}
