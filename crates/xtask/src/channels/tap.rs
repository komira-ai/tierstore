//! `channels tap --version <X.Y.Z> --formula-file <file> --repo <owner/tierstore> --tap-repo
//! <owner/name> [--base main] [--gh <program>]`: the pull request that puts a release's formula
//! in the Homebrew tap, as the release bot (its token is in the environment of `gh`).
//!
//! The tap admits such a pull request only through its guard (`.github/guard/guard.js` of
//! `homebrew-tap`): the author is the bot, the branch is `bump/tierstore-v<version>` in the tap
//! itself, the one changed path is `Formula/tierstore.rb` (added or modified), the whole file is
//! the template for that name and version, the version is higher than the formula on the base, and
//! the tag `v<version>` exists in the released repository. This verb makes exactly that and
//! refuses, before it writes anything, what it can see the guard refusing, so a mistake is red here
//! and not a pull request that sits.
//!
//! Everything goes through the GitHub API (`gh api`), so there is no clone and nothing on disk:
//! the branch is a ref at the base's tip, the formula is one commit on it (the contents API), and
//! the pull request is opened with `gh api` too. In order:
//!
//! 1. the file is the formula of this version ([`formula::check`]) and the tag exists;
//! 2. the formula on the base: a higher version is refused (a tap never moves down), the same
//!    version means the tap has this release and nothing is opened;
//! 3. an open bump pull request for this version or a newer one means there is nothing to open (an
//!    open one for this version without auto-merge gets it asked for);
//! 4. otherwise the branch is made, or reused when an earlier run stopped after making it and it
//!    holds these bytes, the pull request is opened, and auto-merge (squash) is asked for. GitHub
//!    refuses auto-merge on a tap with no required checks: that is a notice, not a failure;
//! 5. open bump pull requests for older versions are closed, with their branches.
//!
//! A run again is therefore safe: whatever the earlier run finished is found and not made twice.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::formula::{self, NAME, check, live_version};
use super::numbers;
use crate::Error;
use crate::release::Gh;

/// The one path the pull request changes.
pub const PATH: &str = "Formula/tierstore.rb";

/// What `tap` is asked.
pub struct Request<'a> {
    pub version: &'a str,
    pub formula_file: &'a Path,
    pub repo: &'a str,
    pub tap_repo: &'a str,
    pub base: &'a str,
}

/// An open pull request of the tap that bumps the formula.
struct Bump {
    number: u64,
    url: String,
    version: String,
    numbers: (u64, u64, u64),
    auto_merge: bool,
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// The text of the file the contents API answered with.
fn decode(file: &Value) -> Result<String, Error> {
    let encoded: String = file["content"]
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|e| Error::Failed(format!("{PATH} from GitHub is not base64: {e}")))?;
    String::from_utf8(bytes).map_err(|_| Error::Failed(format!("{PATH} from GitHub is not text")))
}

/// The open pull requests of the tap that are bumps of this formula.
fn open_bumps(gh: &dyn Gh, tap_repo: &str) -> Result<Vec<Bump>, Error> {
    let listed = gh.api(&format!("repos/{tap_repo}/pulls?state=open&per_page=100"))?;
    let prefix = format!("bump/{NAME}-v");
    let mut found = Vec::new();
    for pr in listed
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let branch = pr["head"]["ref"].as_str().unwrap_or_default();
        let Some((version, parsed)) = branch
            .strip_prefix(&prefix)
            .and_then(|v| Some((v, numbers(v)?)))
        else {
            continue;
        };
        let (Some(number), Some(url)) = (pr["number"].as_u64(), pr["html_url"].as_str()) else {
            return Err(Error::Failed(format!(
                "the pull request of {branch} has no number or url"
            )));
        };
        found.push(Bump {
            number,
            url: url.to_owned(),
            version: version.to_owned(),
            numbers: parsed,
            auto_merge: !pr["auto_merge"].is_null(),
        });
    }
    Ok(found)
}

/// Asks for auto-merge (squash) on `pr`; a refusal is a notice.
fn ask_auto_merge(gh: &dyn Gh, tap_repo: &str, pr: &str) -> String {
    let asked = gh.run(&strings(&[
        "pr", "merge", pr, "-R", tap_repo, "--auto", "--squash",
    ]));
    match asked {
        Ok(_) => format!("auto-merge (squash) is on for {pr}"),
        Err(why) => format!(
            "::notice title=The tap pull request is open without auto-merge::{pr} stays open: GitHub refused auto-merge on it ({why}). A tap without required checks cannot auto-merge; squash-merge it by hand."
        ),
    }
}

/// Closes the pull requests in `older` with their branches; a failure is a notice.
fn close_older(gh: &dyn Gh, tap_repo: &str, older: &[&Bump], newer: &str) -> Vec<String> {
    let mut said = Vec::new();
    for pr in older {
        let comment = format!("Superseded by the bump to {newer}.");
        let closed = gh.run(&[
            "pr".to_owned(),
            "close".to_owned(),
            pr.number.to_string(),
            "-R".to_owned(),
            tap_repo.to_owned(),
            "--delete-branch".to_owned(),
            "--comment".to_owned(),
            comment,
        ]);
        said.push(match closed {
            Ok(_) => format!("closed {}, superseded by {newer}", pr.url),
            Err(why) => format!(
                "::notice title=An older tap pull request stays open::{} could not be closed ({why}); close it by hand.",
                pr.url
            ),
        });
    }
    said
}

/// Makes the branch of the bump with the formula on it, from the base's tip.
fn push_branch(
    gh: &dyn Gh,
    r: &Request,
    branch: &str,
    text: &str,
    blob: Option<&str>,
) -> Result<(), Error> {
    let tap = r.tap_repo;
    let tip = gh
        .api(&format!("repos/{tap}/git/ref/heads/{}", r.base))?
        .and_then(|v| v["object"]["sha"].as_str().map(str::to_owned))
        .ok_or_else(|| Error::Failed(format!("{tap} has no branch {}", r.base)))?;
    gh.run(&[
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        format!("repos/{tap}/git/refs"),
        "-f".to_owned(),
        format!("ref=refs/heads/{branch}"),
        "-f".to_owned(),
        format!("sha={tip}"),
    ])?;
    let mut args = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "PUT".to_owned(),
        format!("repos/{tap}/contents/{PATH}"),
        "-f".to_owned(),
        format!("message={NAME} {}", r.version),
        "-f".to_owned(),
        format!("content={}", STANDARD.encode(text)),
        "-f".to_owned(),
        format!("branch={branch}"),
    ];
    if let Some(blob) = blob {
        args.push("-f".to_owned());
        args.push(format!("sha={blob}"));
    }
    gh.run(&args)?;
    Ok(())
}

/// Opens the pull request of `branch`: its url.
fn open_pr(gh: &dyn Gh, r: &Request, branch: &str) -> Result<String, Error> {
    let title = format!("{NAME} {}", r.version);
    let body = format!(
        "{PATH} for the release v{v} of {repo}, rendered by `channels formula` from the SHA256SUMS the release pipeline verified.\n\nRelease: https://github.com/{repo}/releases/tag/v{v}\n\nOpened by the release pipeline of {repo} after it published the release. The only change is {PATH}.",
        v = r.version,
        repo = r.repo,
    );
    let made = gh.run(&[
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        format!("repos/{}/pulls", r.tap_repo),
        "-f".to_owned(),
        format!("title={title}"),
        "-f".to_owned(),
        format!("head={branch}"),
        "-f".to_owned(),
        format!("base={}", r.base),
        "-f".to_owned(),
        format!("body={body}"),
        "--jq".to_owned(),
        ".html_url".to_owned(),
    ])?;
    let url = made.trim();
    if url.starts_with("https://") {
        Ok(url.to_owned())
    } else {
        Err(Error::Failed(format!(
            "opening the pull request printed {url:?}, not its url"
        )))
    }
}

/// The flags held to their forms.
fn check_request(r: &Request) -> Result<(u64, u64, u64), Error> {
    let Some(ours) = numbers(r.version) else {
        return Err(Error::Usage(format!(
            "--version {:?} is not <major>.<minor>.<patch>",
            r.version
        )));
    };
    if !formula::is_repo(r.repo) {
        return Err(Error::Usage(format!(
            "--repo {:?} is not <owner>/{NAME}",
            r.repo
        )));
    }
    let owner = r.repo.split('/').next().unwrap_or_default();
    let tap_ok = r
        .tap_repo
        .split_once('/')
        .is_some_and(|(o, name)| o == owner && !name.is_empty() && !name.contains('/'));
    if !tap_ok {
        return Err(Error::Usage(format!(
            "--tap-repo {:?} is not <{owner}>/<name>: the tap's guard builds the urls from the tap's owner",
            r.tap_repo
        )));
    }
    if r.base.is_empty() || r.base.contains(char::is_whitespace) {
        return Err(Error::Usage(format!("--base {:?} is not a branch", r.base)));
    }
    Ok(ours)
}

/// The `tap` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a formula that is not this version's, a missing tag, a tap that is
/// ahead of this release, a branch that holds other bytes, or a refusal by GitHub.
pub fn tap(gh: &dyn Gh, r: &Request) -> Result<Vec<String>, Error> {
    let ours = check_request(r)?;
    let (version, tap_repo) = (r.version, r.tap_repo);
    let text = std::fs::read_to_string(r.formula_file)
        .map_err(Error::spawn(format!("read {}", r.formula_file.display())))?;
    check(&text, r.repo, version)
        .map_err(|why| Error::Failed(format!("{}: {why}", r.formula_file.display())))?;
    let tag = format!("v{version}");
    if gh
        .api(&format!("repos/{}/git/ref/tags/{tag}", r.repo))?
        .is_none()
    {
        return Err(Error::Failed(format!(
            "the tag {tag} does not exist in {}: the tap's guard would refuse the pull request",
            r.repo
        )));
    }
    let live = gh.api(&format!("repos/{tap_repo}/contents/{PATH}?ref={}", r.base))?;
    let mut blob = None;
    if let Some(file) = &live {
        let live_text = decode(file)?;
        let live_version = live_version(&live_text, r.repo)
            .map_err(|why| Error::Failed(format!("{PATH} on {} of {tap_repo}: {why}", r.base)))?;
        let theirs = numbers(&live_version).ok_or_else(|| {
            Error::Failed(format!(
                "{PATH} on {} of {tap_repo}: a number too large",
                r.base
            ))
        })?;
        if theirs > ours {
            return Err(Error::Failed(format!(
                "refusing to open a pull request for {tag}: {tap_repo} already has v{live_version} (a tap never moves down)"
            )));
        }
        if theirs == ours {
            return Ok(vec![format!(
                "{PATH} on {} of {tap_repo} is already at {tag}: nothing to open",
                r.base
            )]);
        }
        blob = file["sha"].as_str().map(str::to_owned);
    }
    let bumps = open_bumps(gh, tap_repo)?;
    let older: Vec<&Bump> = bumps.iter().filter(|b| b.numbers < ours).collect();
    let mut said = Vec::new();
    if let Some(covering) = bumps.iter().find(|b| b.numbers >= ours) {
        if covering.numbers == ours && !covering.auto_merge {
            said.push(ask_auto_merge(gh, tap_repo, &covering.url));
        }
        said.push(format!(
            "a bump pull request for {tag} or newer is open ({}, {}): nothing to open",
            covering.url, covering.version
        ));
        said.extend(close_older(gh, tap_repo, &older, &tag));
        return Ok(said);
    }
    let branch = format!("bump/{NAME}-v{version}");
    match gh.api(&format!("repos/{tap_repo}/contents/{PATH}?ref={branch}"))? {
        Some(held) if decode(&held)? != text => {
            return Err(Error::Failed(format!(
                "the branch {branch} exists in {tap_repo} with another {PATH} than this release's; delete the branch by hand"
            )));
        }
        Some(_) => {}
        None => push_branch(gh, r, &branch, &text, blob.as_deref())?,
    }
    let url = open_pr(gh, r, &branch)?;
    said.push(format!("opened {url}: {NAME} {version}"));
    said.push(ask_auto_merge(gh, tap_repo, &url));
    said.extend(close_older(gh, tap_repo, &older, &tag));
    Ok(said)
}

#[cfg(test)]
mod tests;
