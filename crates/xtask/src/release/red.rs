//! `release red`: one open GitHub issue per stage whose run had a job fail.
//!
//! The slow suites of `beta.yml`, and the install checks, the lab and the promotion of
//! `release.yml`, never hold a deploy back. The last job of each of those workflows, named by
//! `--job`, runs this verb whatever the jobs before it did, and it keeps one open issue titled
//! `RELEASE-RED: tierstore <stage>` (a failure of the deploy path of `release.yml` is reported
//! there too):
//!
//! | jobs of the run | open issue | `--complete` | does |
//! |---|---|---|---|
//! | some failed, were cancelled or timed out | none | any | opens it, with a table of those jobs |
//! | some failed, were cancelled or timed out | yes | any | comments the same body on it |
//! | none | yes | true | comments `Green again at <commit>` and closes it |
//! | none | yes | false | leaves it open: the run did not do the whole stage (it released nothing) |
//! | none | none | any | nothing |
//!
//! A job is red when its conclusion is `failure`, `cancelled` or `timed_out`; success, skipped,
//! neutral and a job still running (no conclusion) are not. The job named by `--job` (the one
//! that runs this verb) is never counted. The verb exits 0 in every row, so the red job does
//! not fail the run a second time; it fails only on a bad argument or a GitHub error. The jobs
//! are read with one page (100) of `repos/<repo>/actions/runs/<run>/jobs`; a run with more is
//! refused rather than half read.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::advisories::{Call, Github, cell, check_repo, gh_api, open_issue, write};
use crate::{Error, build_commit};

/// The stage whose failure is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Stage {
    /// `beta.yml`: the linux-arm64 and macOS tests of the workspace.
    Beta,
    /// `release.yml`: any job of a release run, the install checks and the promotion among them.
    Release,
}

impl Stage {
    /// The word of the issue's title.
    fn name(self) -> &'static str {
        match self {
            Stage::Beta => "beta",
            Stage::Release => "release",
        }
    }
}

/// The arguments of `release red`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `github.repository`).
    #[arg(long)]
    pub repo: String,
    /// The stage that ran: `beta` or `release`.
    #[arg(long, value_enum)]
    pub stage: Stage,
    /// The id of the workflow run to read (on Actions, `github.run_id`).
    #[arg(long)]
    pub run: String,
    /// The name of the job that runs this verb, which is not counted.
    #[arg(long)]
    pub job: String,
    /// The commit the run is of, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// Whether every job of the stage ran (`true` or `false`): only then may a green run close
    /// the issue.
    #[arg(long, action = clap::ArgAction::Set)]
    pub complete: bool,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The program `gh` as the [`Github`] of the verb.
struct GhProgram<'a>(&'a Path);

impl Github for GhProgram<'_> {
    fn github(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        gh_api(self.0, call)
    }
}

/// `release red`: the lines to print when it finished.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    sync(&mut GhProgram(&args.gh), args)
}

/// Check the arguments, read the run's jobs and bring the stage's issue to match them.
pub fn sync(github: &mut impl Github, args: &Args) -> Result<Vec<String>, Error> {
    check_repo(&args.repo)?;
    if args.run.is_empty() || !args.run.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Usage(format!(
            "--run {:?} is not a run id",
            args.run
        )));
    }
    build_commit::full_commit("--commit", &args.commit)?;
    if args.job.is_empty() {
        return Err(Error::Usage("--job must not be empty".to_owned()));
    }
    let (repo, run, commit) = (&args.repo, &args.run, &args.commit);
    let red = red_jobs(github, repo, run, &args.job)?;
    let title = format!("RELEASE-RED: tierstore {}", args.stage.name());
    let open = open_issue(github, repo, &title)?;
    let run_url = format!("https://github.com/{repo}/actions/runs/{run}");
    match (red.is_empty(), open) {
        (false, None) => {
            let body = render(args.stage, commit, &run_url, &red);
            let made = write(
                github,
                Call::post(
                    format!("repos/{repo}/issues"),
                    json!({ "title": title, "body": body }),
                ),
            )?;
            let url = made["html_url"].as_str().unwrap_or_default();
            Ok(vec![format!("{title}: opened {url}")])
        }
        (false, Some(number)) => {
            let body = render(args.stage, commit, &run_url, &red);
            write(
                github,
                Call::post(
                    format!("repos/{repo}/issues/{number}/comments"),
                    json!({ "body": body }),
                ),
            )?;
            Ok(vec![format!("{title}: commented on issue #{number}")])
        }
        (true, Some(number)) if args.complete => {
            let issue = format!("repos/{repo}/issues/{number}");
            let note = format!("Green again at {commit}: {run_url}");
            write(
                github,
                Call::post(format!("{issue}/comments"), json!({ "body": note })),
            )?;
            let closed = json!({ "state": "closed", "state_reason": "completed" });
            write(github, Call::patch(issue, closed))?;
            Ok(vec![format!("{title}: green, closed issue #{number}")])
        }
        (true, Some(number)) => Ok(vec![format!(
            "{title}: incomplete run, issue #{number} left open"
        )]),
        (true, None) => Ok(vec![format!("{title}: green, nothing to do")]),
    }
}

/// A job of the run that is red: its name, its page and its conclusion.
struct Red {
    name: String,
    url: Option<String>,
    conclusion: String,
}

/// The red jobs of run `run`, other than `own`, in the order GitHub lists them.
fn red_jobs(github: &mut impl Github, repo: &str, run: &str, own: &str) -> Result<Vec<Red>, Error> {
    let path = format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100");
    let Some(found) = github.github(&Call::get(path))? else {
        return Err(Error::Failed(format!("{repo} has no run {run}")));
    };
    let Some(jobs) = found["jobs"].as_array() else {
        return Err(Error::Failed(format!(
            "run {run} of {repo}: the reply has no list of jobs"
        )));
    };
    // One page holds every job of the stage; a run with more is refused, not half read.
    if found["total_count"].as_u64() != Some(jobs.len() as u64) {
        return Err(Error::Failed(format!(
            "run {run} of {repo} has {} jobs; one page of {} was read",
            found["total_count"],
            jobs.len()
        )));
    }
    let mut red = vec![];
    for job in jobs {
        let Some(name) = job["name"].as_str() else {
            return Err(Error::Failed(format!(
                "run {run} of {repo}: a job with no name"
            )));
        };
        let conclusion = job["conclusion"].as_str().unwrap_or_default();
        if name != own && matches!(conclusion, "failure" | "cancelled" | "timed_out") {
            red.push(Red {
                name: name.to_owned(),
                url: job["html_url"].as_str().map(str::to_owned),
                conclusion: conclusion.to_owned(),
            });
        }
    }
    Ok(red)
}

/// The text of the issue or comment: the run and a table of its red jobs.
fn render(stage: Stage, commit: &str, run_url: &str, red: &[Red]) -> String {
    let mut body = format!(
        "RELEASE-RED: tierstore {} failed at {commit}.\n\nRun: {run_url}\n\n| job | conclusion |\n|---|---|\n",
        stage.name()
    );
    for job in red {
        let name = cell(&job.name);
        let link = match &job.url {
            Some(url) => format!("[{name}]({url})"),
            None => name,
        };
        body.push_str(&format!("| {link} | {} |\n", job.conclusion));
    }
    body
}

#[cfg(test)]
mod tests;
