//! The `needs` graph of `release.yml` ([`super::graph`]): the file passes, a job behind a job that
//! may be skipped must say it runs after a skipped job, and the conditions of the file, run
//! against the outcomes of a release, skip and run the jobs the design says.

use std::collections::BTreeMap;

use super::expr::{Context, holds};
use super::graph::{Info, ancestors, graph_problems, jobs};
use super::platforms_tests::in_job;
use super::text_of;
use super::yaml;

fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => graph_problems(&doc),
        Err(e) => vec![e],
    }
}

fn release() -> BTreeMap<String, Info> {
    jobs(&yaml::load(&text_of("release.yml")).expect("release.yml"))
}

const PREFIX: &str = "!cancelled() && !failure() && ";

/// `release.yml` as it was when the farm was skipped and the promotion with it: no status
/// function on `promote-latest`.
fn without_status(job: &str) -> String {
    in_job(&text_of("release.yml"), job, PREFIX, "")
}

#[test]
fn the_release_chain_of_release_yml_passes_the_graph_rule() {
    // Catches: a rule that refuses the file as it is, which the planted defects below would not
    // notice.
    assert_eq!(problems_of(&text_of("release.yml")), Vec::<String>::new());
}

#[test]
fn the_promotion_waits_behind_the_farm_the_soak_and_the_attempts() {
    // Catches: a graph walk that stops at the first level, which would hide the farm and the
    // soak (skipped when no hostkeeper is set) behind `promote-check` from the rule.
    let jobs = release();
    for job in [
        "promote-latest",
        "tap",
        "stable-apt-read",
        "stable-rpm-write-3",
    ] {
        let above = ancestors(&jobs, job);
        for needed in [
            "promote-check",
            "farm",
            "soak",
            "check-apt",
            "testing-apt-read-2",
        ] {
            assert!(above.contains(needed), "{job} does not wait for {needed}");
        }
        assert!(!above.contains("plan"), "{job}: the root is left out");
    }
    assert!(ancestors(&jobs, "plan").is_empty());
    assert!(!ancestors(&jobs, "farm").contains("soak"));
}

#[test]
fn a_job_behind_a_gated_job_without_a_status_function_is_named() {
    // Catches: the release that ended at `promote-check` with `promote=true`: `promote-latest`,
    // `tap` and every `stable-*` job skipped, because the farm and the soak were.
    for job in [
        "promote-latest",
        "tap",
        "stable-apt-read",
        "stable-apt-write",
        "stable-rpm-index",
        "stable-rpm-write-3",
        "testing-apt-index-2",
        "testing-rpm-read-3",
    ] {
        let problems = problems_of(&without_status(job));
        assert_eq!(problems.len(), 1, "{job}: {problems:?}");
        assert!(
            problems[0].starts_with(&format!("{job}: `if` has no status function")),
            "{problems:?}"
        );
    }
    let problems = problems_of(&without_status("promote-latest"));
    assert!(problems[0].contains("\"farm\""), "{problems:?}");
    assert!(problems[0].contains("\"soak\""), "{problems:?}");
}

#[test]
fn any_status_function_satisfies_the_rule_and_a_job_with_nothing_above_it_needs_none() {
    // Catches: a rule that insists on one spelling, or one that asks a status function of
    // `formula` and the other jobs that only wait for `publish`.
    let text = text_of("release.yml");
    for status in ["always() && ", "success() && ", "failure() || "] {
        let planted = in_job(&text, "tap", PREFIX, status);
        assert_eq!(problems_of(&planted), Vec::<String>::new(), "{status}");
    }
    for job in [
        "formula",
        "testing-upload",
        "testing-apt-read-2",
        "farm-check",
    ] {
        let ifs = jobs(&yaml::load(&text).unwrap())[job].has_status();
        assert_eq!(ifs, job == "farm-check", "{job}");
    }
}

#[test]
fn an_attempt_gated_on_a_conflict_must_be_named_by_the_jobs_after_it() {
    // Catches: a status function on `testing-apt-index-2` with no test of `testing-apt-read-2`,
    // which would run the second index when no second attempt was needed.
    let spoiled = in_job(
        &text_of("release.yml"),
        "testing-apt-index-2",
        " && needs.testing-apt-read-2.result == 'success'",
        "",
    );
    let problems = problems_of(&spoiled);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("testing-apt-index-2: `if` does not test needs.testing-apt-read-2"),
        "{problems:?}"
    );
}

/// How the jobs of `release.yml` end in one release, as the answers they give.
struct Release {
    /// `needs.<job>.outputs.<name>` of the jobs that ran.
    outputs: Vec<(&'static str, &'static str, &'static str)>,
}

/// The result of each job of `text` ('success' or 'skipped') in the release `release`, under
/// GitHub's rule for a job without a status function: it runs only if every job in its chain
/// succeeded.
fn simulate(text: &str, release: &Release) -> BTreeMap<String, &'static str> {
    let jobs = jobs(&yaml::load(text).expect("release.yml"));
    // Dependencies first: a job is settled once all it waits for is.
    let mut results: BTreeMap<String, &'static str> = BTreeMap::new();
    results.insert("plan".to_owned(), "success");
    while results.len() < jobs.len() {
        for (name, info) in &jobs {
            if results.contains_key(name) || !info.needs.iter().all(|n| results.contains_key(n)) {
                continue;
            }
            let mut names = BTreeMap::from([
                ("needs.plan.outputs.release".to_owned(), "true".to_owned()),
                ("github.ref".to_owned(), "refs/heads/main".to_owned()),
            ]);
            for need in &info.needs {
                names.insert(format!("needs.{need}.result"), results[need].to_owned());
            }
            for (job, key, value) in &release.outputs {
                if results.get(*job) == Some(&"success") {
                    names.insert(format!("needs.{job}.outputs.{key}"), (*value).to_owned());
                }
            }
            let context = Context {
                names,
                cancelled: false,
                failed: false,
            };
            let text = format!("${{{{ {} }}}}", info.condition);
            let holds = info.condition.is_empty() || holds(&text, &context).expect("an if");
            let chain_ok = ancestors(&jobs, name)
                .iter()
                .all(|a| results[a] == "success");
            let runs = holds && (info.has_status() || chain_ok);
            results.insert(name.clone(), if runs { "success" } else { "skipped" });
        }
    }
    results
}

fn ran(results: &BTreeMap<String, &'static str>, prefix: &str) -> Vec<String> {
    results
        .iter()
        .filter(|(n, r)| n.starts_with(prefix) && **r == "success")
        .map(|(n, _)| n.clone())
        .collect()
}

/// A release whose first attempts all land: no farm (no hostkeeper settings), `promote=true`.
fn no_farm() -> Release {
    Release {
        outputs: vec![
            ("testing-upload", "enabled", "true"),
            ("testing-apt-write", "conflict", "false"),
            ("testing-rpm-write", "conflict", "false"),
            ("farm-check", "enabled", "false"),
            ("promote-check", "promote", "true"),
            ("stable-apt-write", "conflict", "false"),
            ("stable-rpm-write", "conflict", "false"),
        ],
    }
}

#[test]
fn a_release_without_a_farm_is_promoted_through_every_channel_and_no_second_attempt_runs() {
    // Catches: the release of run 42537574985, where `promote-check` said `promote=true` and
    // `promote-latest`, `tap` and every `stable-*` job were skipped behind the skipped farm and
    // soak; and a status function that starts the second or third attempt of a tree whose
    // first write landed.
    let results = simulate(&text_of("release.yml"), &no_farm());
    assert_eq!(results["farm"], "skipped");
    assert_eq!(results["soak"], "skipped");
    assert_eq!(results["promote-check"], "success");
    for job in [
        "promote-latest",
        "tap",
        "stable-apt-read",
        "stable-apt-index",
        "stable-apt-write",
        "stable-rpm-read",
        "stable-rpm-index",
        "stable-rpm-write",
    ] {
        assert_eq!(results[job], "success", "{job}");
    }
    for job in results.keys() {
        let attempt = job.ends_with("-2") || job.ends_with("-3");
        assert_eq!(
            results[job] == "skipped",
            attempt || job == "farm" || job == "soak",
            "{job}"
        );
    }
}

#[test]
fn the_release_of_main_before_the_fix_is_skipped_behind_the_farm() {
    // Catches: a simulation that cannot show the defect, which would make the test above pass
    // for any file.
    let mut text = text_of("release.yml");
    for job in [
        "promote-latest",
        "tap",
        "stable-apt-read",
        "stable-rpm-write",
    ] {
        text = in_job(&text, job, PREFIX, "");
    }
    let results = simulate(&text, &no_farm());
    assert_eq!(results["promote-check"], "success");
    for job in [
        "promote-latest",
        "tap",
        "stable-apt-read",
        "stable-rpm-write",
    ] {
        assert_eq!(results[job], "skipped", "{job}");
    }
    assert_eq!(results["stable-apt-index"], "success");
}

#[test]
fn a_release_that_is_not_promoted_runs_no_stable_job() {
    // Catches: a status function that lets `promote-latest`, `tap` or a `stable-*` job run when
    // `promote-check` did not say `promote=true`.
    let mut release = no_farm();
    release.outputs.retain(|o| o.0 != "promote-check");
    release.outputs.push(("promote-check", "promote", "false"));
    let results = simulate(&text_of("release.yml"), &release);
    assert_eq!(results["promote-check"], "success");
    assert!(ran(&results, "stable-").is_empty(), "{results:?}");
    assert_eq!(results["promote-latest"], "skipped");
    assert_eq!(results["tap"], "skipped");
}

#[test]
fn a_write_that_lost_the_race_runs_the_next_attempt_and_no_later_one() {
    // Catches: an attempt chain that stops after the first conflict (the later attempts skipped
    // behind the skipped ones), one that runs the third attempt after the second landed, and a
    // promotion that is skipped after a retry.
    let mut release = no_farm();
    release
        .outputs
        .retain(|o| o.0 != "testing-apt-write" && o.0 != "stable-rpm-write");
    release.outputs.extend([
        ("testing-apt-write", "conflict", "true"),
        ("testing-apt-write-2", "conflict", "false"),
        ("stable-rpm-write", "conflict", "true"),
        ("stable-rpm-write-2", "conflict", "false"),
    ]);
    let results = simulate(&text_of("release.yml"), &release);
    assert_eq!(
        ran(&results, "testing-apt-"),
        [
            "testing-apt-index",
            "testing-apt-index-2",
            "testing-apt-read",
            "testing-apt-read-2",
            "testing-apt-write",
            "testing-apt-write-2",
        ]
    );
    assert_eq!(
        ran(&results, "stable-rpm-"),
        [
            "stable-rpm-index",
            "stable-rpm-index-2",
            "stable-rpm-read",
            "stable-rpm-read-2",
            "stable-rpm-write",
            "stable-rpm-write-2",
        ]
    );
    assert_eq!(results["check-apt"], "success");
    assert_eq!(results["promote-latest"], "success");
}

#[test]
fn three_conflicts_in_a_row_run_all_three_attempts() {
    // Catches: a third attempt that never runs behind a second that did.
    let mut release = no_farm();
    release.outputs.retain(|o| o.0 != "testing-rpm-write");
    release.outputs.extend([
        ("testing-rpm-write", "conflict", "true"),
        ("testing-rpm-write-2", "conflict", "true"),
        ("testing-rpm-write-3", "conflict", "false"),
    ]);
    let results = simulate(&text_of("release.yml"), &release);
    assert_eq!(ran(&results, "testing-rpm-").len(), 9);
    assert_eq!(results["check-rpm"], "success");
}

#[test]
fn an_index_with_a_status_function_and_no_test_of_its_read_runs_when_no_attempt_was_needed() {
    // Catches: the way to fix the defect that breaks the chains instead, shown by the rule above
    // and by the run of it here.
    let spoiled = in_job(
        &text_of("release.yml"),
        "testing-apt-index-2",
        " && needs.testing-apt-read-2.result == 'success'",
        "",
    );
    let results = simulate(&spoiled, &no_farm());
    assert_eq!(results["testing-apt-index-2"], "success");
    assert_eq!(results["testing-apt-read-2"], "skipped");
}
