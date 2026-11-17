//! The decisions of `release red`, against a fake GitHub: no process, no network.

use std::collections::BTreeMap;

use super::*;
use crate::advisories::Method;

const REPO: &str = "example/tierstore";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const JOBS: &str = "repos/example/tierstore/actions/runs/42/jobs?per_page=100";
const LIST: &str = "repos/example/tierstore/issues?state=open&per_page=100&page=1";
const RUN_URL: &str = "https://github.com/example/tierstore/actions/runs/42";
const ISSUE_URL: &str = "https://github.com/example/tierstore/issues/9";

/// GitHub as a script: a GET is answered by its exact path, every call is recorded.
struct Fake {
    /// Answer every write with nothing.
    silent: bool,
    gets: BTreeMap<String, Option<Value>>,
    calls: Vec<Call>,
}

impl Github for Fake {
    fn github(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        self.calls.push(call.clone());
        if call.method == Method::Get {
            return Ok(self.gets.get(&call.path).cloned().expect("a scripted GET"));
        }
        if self.silent {
            return Ok(None);
        }
        Ok(Some(json!({ "number": 9, "html_url": ISSUE_URL })))
    }
}

impl Fake {
    fn writes(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|c| c.method != Method::Get)
            .collect()
    }
}

fn job(name: &str, conclusion: Value) -> Value {
    json!({
        "name": name,
        "conclusion": conclusion,
        "html_url": format!("https://github.com/example/tierstore/actions/runs/42/job/{}", name.len()),
    })
}

fn run_of(jobs: Vec<Value>) -> Value {
    json!({ "total_count": jobs.len(), "jobs": jobs })
}

fn issue(number: u64, title: &str) -> Value {
    json!({ "number": number, "title": title })
}

fn fake(run: Value, issues: Vec<Value>) -> Fake {
    let mut gets = BTreeMap::new();
    gets.insert(JOBS.to_owned(), Some(run));
    gets.insert(LIST.to_owned(), Some(Value::Array(issues)));
    Fake {
        silent: false,
        gets,
        calls: vec![],
    }
}

fn args(stage: Stage, complete: bool) -> Args {
    Args {
        repo: REPO.to_owned(),
        stage,
        run: "42".to_owned(),
        job: "red".to_owned(),
        commit: COMMIT.to_owned(),
        complete,
        gh: PathBuf::from("gh"),
    }
}

fn beta() -> Args {
    args(Stage::Beta, true)
}

fn failure(r: Result<Vec<String>, Error>) -> Error {
    r.expect_err("a refusal")
}

fn failed_text(r: Result<Vec<String>, Error>) -> String {
    match failure(r) {
        Error::Failed(why) => why,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn usage_text(r: Result<Vec<String>, Error>) -> String {
    match failure(r) {
        Error::Usage(why) => why,
        other => panic!("expected a usage error, got {other:?}"),
    }
}

fn one_red() -> Value {
    run_of(vec![
        job("test", json!("success")),
        job("arm64", json!("failure")),
        job("red", json!("success")),
    ])
}

/// What the issue or comment says of the one red job `arm64` of beta.
fn arm64_body(stage: &str) -> String {
    format!(
        "RELEASE-RED: tierstore {stage} failed at {COMMIT}.\n\nRun: {RUN_URL}\n\n| job | conclusion |\n|---|---|\n| [arm64](https://github.com/example/tierstore/actions/runs/42/job/5) | failure |\n"
    )
}

#[test]
fn a_red_job_with_no_open_issue_opens_one_with_the_title_and_a_table() {
    let mut f = fake(one_red(), vec![issue(3, "something else")]);
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(
        out,
        [format!("RELEASE-RED: tierstore beta: opened {ISSUE_URL}")]
    );
    let writes = f.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, Method::Post);
    assert_eq!(writes[0].path, "repos/example/tierstore/issues");
    assert_eq!(
        writes[0].body,
        Some(json!({
            "title": "RELEASE-RED: tierstore beta",
            "body": arm64_body("beta"),
        }))
    );
}

#[test]
fn a_red_job_with_an_open_issue_comments_on_it() {
    let mut f = fake(one_red(), vec![issue(7, "RELEASE-RED: tierstore beta")]);
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(out, ["RELEASE-RED: tierstore beta: commented on issue #7"]);
    assert_eq!(
        f.writes(),
        [&Call::post(
            "repos/example/tierstore/issues/7/comments".to_owned(),
            json!({ "body": arm64_body("beta") }),
        )]
    );
}

#[test]
fn the_release_stage_has_its_own_title() {
    let mut f = fake(one_red(), vec![issue(7, "RELEASE-RED: tierstore beta")]);
    let out = sync(&mut f, &args(Stage::Release, true)).expect("sync");
    assert_eq!(
        out,
        [format!(
            "RELEASE-RED: tierstore release: opened {ISSUE_URL}"
        )]
    );
    let body = f.writes()[0].body.clone().expect("body");
    assert_eq!(body["title"], "RELEASE-RED: tierstore release");
    assert_eq!(body["body"], arm64_body("release"));
}

#[test]
fn a_green_complete_run_comments_and_closes_the_open_issue() {
    let green = run_of(vec![
        job("test", json!("success")),
        job("red", json!("skipped")),
    ]);
    let mut f = fake(green, vec![issue(7, "RELEASE-RED: tierstore beta")]);
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(out, ["RELEASE-RED: tierstore beta: green, closed issue #7"]);
    assert_eq!(
        f.writes(),
        [
            &Call::post(
                "repos/example/tierstore/issues/7/comments".to_owned(),
                json!({ "body": format!("Green again at {COMMIT}: {RUN_URL}") }),
            ),
            &Call::patch(
                "repos/example/tierstore/issues/7".to_owned(),
                json!({ "state": "closed", "state_reason": "completed" }),
            ),
        ]
    );
}

#[test]
fn a_green_run_with_no_open_issue_writes_nothing() {
    let green = run_of(vec![job("test", json!("success"))]);
    let mut f = fake(green, vec![]);
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(out, ["RELEASE-RED: tierstore beta: green, nothing to do"]);
    assert!(f.writes().is_empty());
}

#[test]
fn a_green_incomplete_run_leaves_the_open_issue_untouched() {
    let green = run_of(vec![job("test", json!("success"))]);
    let mut f = fake(green, vec![issue(7, "RELEASE-RED: tierstore release")]);
    let out = sync(&mut f, &args(Stage::Release, false)).expect("sync");
    assert_eq!(
        out,
        ["RELEASE-RED: tierstore release: incomplete run, issue #7 left open"]
    );
    assert!(f.writes().is_empty());
}

#[test]
fn a_red_incomplete_run_still_reports_its_red_jobs() {
    let mut f = fake(one_red(), vec![]);
    let out = sync(&mut f, &args(Stage::Beta, false)).expect("sync");
    assert_eq!(
        out,
        [format!("RELEASE-RED: tierstore beta: opened {ISSUE_URL}")]
    );
    assert_eq!(f.writes().len(), 1);
}

#[test]
fn the_job_that_runs_the_verb_is_not_red_even_when_it_failed() {
    let only_own = run_of(vec![
        job("test", json!("success")),
        job("red", json!("failure")),
    ]);
    let mut f = fake(only_own, vec![]);
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(out, ["RELEASE-RED: tierstore beta: green, nothing to do"]);
    assert!(f.writes().is_empty());
}

#[test]
fn only_failure_cancelled_and_timed_out_are_red_in_the_order_listed() {
    let jobs = run_of(vec![
        job("a", json!("success")),
        job("b", json!("timed_out")),
        job("c", json!("skipped")),
        job("d", json!("neutral")),
        job("e", Value::Null),
        job("f", json!("cancelled")),
        job("g", json!("failure")),
        json!({ "name": "h|x\ny", "conclusion": "failure" }),
    ]);
    let mut f = fake(jobs, vec![]);
    sync(&mut f, &beta()).expect("sync");
    let body = f.writes()[0].body.clone().expect("body");
    assert_eq!(
        body["body"],
        format!(
            "RELEASE-RED: tierstore beta failed at {COMMIT}.\n\nRun: {RUN_URL}\n\n| job | conclusion |\n|---|---|\n\
             | [b](https://github.com/example/tierstore/actions/runs/42/job/1) | timed_out |\n\
             | [f](https://github.com/example/tierstore/actions/runs/42/job/1) | cancelled |\n\
             | [g](https://github.com/example/tierstore/actions/runs/42/job/1) | failure |\n\
             | h\\|x y | failure |\n"
        )
    );
}

#[test]
fn a_run_that_does_not_exist_is_a_failure() {
    let mut f = fake(Value::Null, vec![]);
    f.gets.insert(JOBS.to_owned(), None);
    assert_eq!(
        failed_text(sync(&mut f, &beta())),
        "example/tierstore has no run 42"
    );
}

#[test]
fn a_reply_with_more_jobs_than_one_page_is_a_failure() {
    let reply = json!({ "total_count": 101, "jobs": [job("a", json!("success"))] });
    let mut f = fake(reply, vec![]);
    assert_eq!(
        failed_text(sync(&mut f, &beta())),
        "run 42 of example/tierstore has 101 jobs; one page of 1 was read"
    );
}

#[test]
fn a_reply_without_a_list_of_jobs_is_a_failure() {
    let mut f = fake(json!({ "total_count": 0, "jobs": "none" }), vec![]);
    assert_eq!(
        failed_text(sync(&mut f, &beta())),
        "run 42 of example/tierstore: the reply has no list of jobs"
    );
}

#[test]
fn a_job_without_a_name_is_a_failure() {
    let reply = run_of(vec![json!({ "conclusion": "failure" })]);
    let mut f = fake(reply, vec![]);
    assert_eq!(
        failed_text(sync(&mut f, &beta())),
        "run 42 of example/tierstore: a job with no name"
    );
}

#[test]
fn the_open_issue_is_found_on_a_later_page_and_a_pull_request_is_not_one() {
    let mut f = fake(one_red(), vec![]);
    let page1: Vec<Value> = (0..100).map(|n| issue(n + 100, "other")).collect();
    f.gets.insert(LIST.to_owned(), Some(Value::Array(page1)));
    f.gets.insert(
        LIST.replace("&page=1", "&page=2"),
        Some(json!([
            { "number": 5, "title": "RELEASE-RED: tierstore beta", "pull_request": {} },
            issue(8, "RELEASE-RED: tierstore beta"),
        ])),
    );
    let out = sync(&mut f, &beta()).expect("sync");
    assert_eq!(out, ["RELEASE-RED: tierstore beta: commented on issue #8"]);
    assert_eq!(
        f.writes()[0].path,
        "repos/example/tierstore/issues/8/comments"
    );
}

#[test]
fn an_unreadable_issue_list_is_a_failure() {
    let mut f = fake(one_red(), vec![]);
    f.gets.insert(LIST.to_owned(), None);
    assert!(failed_text(sync(&mut f, &beta())).ends_with("not found"));
    let mut f = fake(one_red(), vec![]);
    f.gets.insert(LIST.to_owned(), Some(json!({})));
    assert!(failed_text(sync(&mut f, &beta())).ends_with("not an array"));
    assert!(f.writes().is_empty());
}

#[test]
fn a_write_with_no_reply_is_a_failure() {
    let mut f = fake(one_red(), vec![]);
    f.silent = true;
    assert_eq!(
        failed_text(sync(&mut f, &beta())),
        "gh api repos/example/tierstore/issues: no reply"
    );
}

#[test]
fn a_bad_repository_run_commit_or_job_is_a_usage_error() {
    let mut a = beta();
    a.repo = "example".to_owned();
    let mut f = fake(one_red(), vec![]);
    assert_eq!(
        usage_text(sync(&mut f, &a)),
        "--repo \"example\" is not <owner>/<repo>"
    );
    for run in ["x", "", "4 2"] {
        let mut a = beta();
        a.run = run.to_owned();
        assert_eq!(
            usage_text(sync(&mut f, &a)),
            format!("--run {run:?} is not a run id")
        );
    }
    let mut a = beta();
    a.commit = "abc123".to_owned();
    assert_eq!(
        usage_text(sync(&mut f, &a)),
        "--commit \"abc123\" is not a full commit SHA (40 lowercase hex digits)"
    );
    let mut a = beta();
    a.job = String::new();
    assert_eq!(usage_text(sync(&mut f, &a)), "--job must not be empty");
    assert!(
        f.calls.is_empty(),
        "nothing is read before the arguments pass"
    );
}
