//! `release plan` and `release manual`: which commit of `main` is released and as which tag,
//! or whether one is released at all. `release.yml` runs one of them first and feeds its outputs
//! (`release`, `tag`, `commit`, `note`, or `release=false` and a `reason`) to the jobs that
//! check and publish. `release version` ([`version`]) prints the version part of the same
//! computation (`X.Y.Z`), which `artifacts.yml` writes into the crate (`release set-version`)
//! before it builds, so the binary reports the tag it is released under, and records on its run
//! as the name of an artifact, [`VERSION_ARTIFACT`]`<X.Y.Z>` (`--record`).
//!
//! - `release plan --repo <owner/repo> --commit <sha> [--version-file <path>] [--freeze <value>]
//!   [--output <file>] [--gh <program>]` ([`plan`]): `--commit` is the tip of `main`. No release
//!   when `--freeze` (the repository variable `RELEASE_FREEZE`) is anything but empty or
//!   `false` in any case, or when a release already covers the tip. Otherwise the tag is the
//!   patch after the highest `v<major>.<minor>.<patch>` tag, unless the `VERSION` file names a
//!   higher version, which is then the tag ([`next_tag`]). A `VERSION` at or below the highest
//!   tag is a floor, not a pin: it does not stop the bump, so a file nobody updated never makes a
//!   push fail on a tag that exists. The plan then walks the first-parent history of the tip,
//!   newest first, down to the highest tag (at most [`WALK_LIMIT`] commits), and releases the
//!   first commit that is ready: it has a successful push run of both [`BUILD_WORKFLOW`] and
//!   [`CI_WORKFLOW`], and its build run recorded the version of that tag, so its binary says
//!   the tag. A commit built under another version (a release was published between its build
//!   and now, see [`stamped`]) is passed over: the release of a newer commit carries its
//!   changes. No release when no commit is ready, or when everything since the highest release
//!   tag up to the ready commit is under [`DOCS_PATHS`]. The change is measured from the highest
//!   tag, not from the push, so the code of a push whose run was replaced, failed or passed over
//!   goes out with the next release.
//! - `release manual --repo <owner/repo> --tag <tag> --commit <sha> [--note <text>]
//!   [--output <file>] [--gh <program>]` ([`manual`]): the same outputs for a run started by
//!   hand, which names its own tag and commit and is not held by the freeze. It refuses a
//!   commit the highest release tag already covers, so no second tag is made for it. A change
//!   that renames a file is judged by the old path as well as the new.

use std::path::Path;

use serde_json::Value;

use super::publish::{Gh, is_release_tag};
use super::{BUILD_WORKFLOW, CI_WORKFLOW, MAIN, append_output, push_run};
use crate::Error;
use crate::build_commit::full_commit;

/// The paths whose change alone makes no release: a directory (`docs/`, in which `*` stands
/// for one directory name) or a file suffix (`**/*.md`). The product is one binary built from
/// the crates' sources, so these never reach it. Kept in one place; the guide lists them and
/// a test holds the two together.
pub const DOCS_PATHS: [&str; 4] = ["docs/", "**/*.md", "crates/*/docs/", "crates/*/site/"];

/// The most files a GitHub compare lists; a change of that many may have more, so it is code.
const FILES_LIMIT: usize = 300;

/// The most tag refs one page lists; a repository with that many may have more.
const TAG_REFS_LIMIT: usize = 100;

/// The most commit titles a release note lists.
const NOTE_TITLES: usize = 20;

/// The most commits of the first-parent history a plan judges, newest first (each is read from
/// GitHub on its own, so a long compare, which GitHub lists oldest first and cuts short, never
/// hides the newest). A commit further back was built before the twenty newer ones, so under a version no newer than theirs: when
/// none of them is ready, it is not either, or its release is carried by a newer commit's.
const WALK_LIMIT: usize = 20;

/// The start of the name of the artifact on which a build run of [`BUILD_WORKFLOW`] records the
/// version it built the binary under: `version-<X.Y.Z>` (`release version --record`, uploaded
/// by `artifacts.yml`'s `version` job). The plan reads the name; the file in it holds the same
/// version.
pub const VERSION_ARTIFACT: &str = "version-";

/// The version of the first release.
const FIRST: Version = (0, 1, 0);

type Version = (u64, u64, u64);

/// What `release plan` is asked.
pub struct Request<'a> {
    pub repo: &'a str,
    /// The tip of `main`: the plan walks its first-parent history down to the highest tag.
    pub commit: &'a str,
    /// The text of the `VERSION` file, if there is one.
    pub version: Option<&'a str>,
    /// The value of `RELEASE_FREEZE`: `true` freezes.
    pub freeze: &'a str,
    /// Where `key=value` lines are appended (on Actions, `GITHUB_OUTPUT`).
    pub output: Option<&'a Path>,
}

/// Whether `RELEASE_FREEZE` freezes releases: any value except an empty one (the variable is
/// not set) and `false`, in any case. A value that is not understood freezes, so that a typo
/// stops a release instead of letting one through.
fn frozen(value: &str) -> bool {
    let value = value.trim();
    !(value.is_empty() || value.eq_ignore_ascii_case("false"))
}

/// Whether `pattern` (one of [`DOCS_PATHS`]) covers `path`.
fn covers(pattern: &str, path: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("**/*") {
        return path.ends_with(suffix);
    }
    let want: Vec<&str> = pattern.trim_end_matches('/').split('/').collect();
    let have: Vec<&str> = path.split('/').collect();
    have.len() > want.len() && want.iter().zip(&have).all(|(w, h)| *w == "*" || w == h)
}

/// Whether `path` is under [`DOCS_PATHS`].
pub fn is_docs_path(path: &str) -> bool {
    DOCS_PATHS.iter().any(|p| covers(p, path))
}

/// Whether a change of exactly these files makes no release.
pub fn docs_only(files: &[String]) -> bool {
    files.iter().all(|f| is_docs_path(f))
}

/// `v<major>.<minor>.<patch>` as numbers, none for anything else (or a number too large).
fn parse_tag(tag: &str) -> Option<Version> {
    if !is_release_tag(tag) {
        return None;
    }
    let mut parts = tag[1..].split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn show((major, minor, patch): Version) -> String {
    format!("v{major}.{minor}.{patch}")
}

/// The tag of the next release: the patch after `latest` (a release tag), unless `version_file`
/// (the text of `VERSION`) names a higher version, which is the tag. A `VERSION` that is not
/// `<major>.<minor>.<patch>` (a leading `v` is allowed) is refused.
pub fn next_tag(latest: Option<&str>, version_file: Option<&str>) -> Result<String, Error> {
    let latest = latest
        .map(|t| parse_tag(t).ok_or_else(|| Error::Failed(format!("{t:?} is not a release tag"))))
        .transpose()?;
    let named = version_file
        .map(|text| {
            let text = text.trim();
            parse_tag(&format!("v{}", text.strip_prefix('v').unwrap_or(text))).ok_or_else(|| {
                Error::Failed(format!(
                    "VERSION is {text:?}; expected <major>.<minor>.<patch>"
                ))
            })
        })
        .transpose()?;
    let next = match (named, latest) {
        (Some(named), Some(latest)) if named > latest => named,
        (_, Some((major, minor, patch))) => (major, minor, patch + 1),
        (Some(named), None) => named,
        (None, None) => FIRST,
    };
    Ok(show(next))
}

/// The text of the `VERSION` file at `path`, none when there is no such file.
pub fn read_version(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::spawn(format!("read {}", path.display()))(e)),
    }
}

/// `text` on one line: every control character (a line break included) is a space.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
}

/// The release tags of the repository.
fn release_tags(gh: &dyn Gh, repo: &str) -> Result<Vec<String>, Error> {
    let refs = gh.api(&format!(
        "repos/{repo}/git/matching-refs/tags/v?per_page=100"
    ))?;
    let refs = super::array(&refs);
    if refs.len() >= TAG_REFS_LIMIT {
        return Err(Error::Failed(format!(
            "{} tags start with v: more than one page of tags, so the highest may be missing",
            refs.len()
        )));
    }
    Ok(refs
        .iter()
        .filter_map(|r| r["ref"].as_str()?.strip_prefix("refs/tags/"))
        .map(str::to_owned)
        .collect())
}

/// What the commits after the latest release tag are.
enum Changes {
    /// The release covers the commit (it is the tag's commit, or older).
    Released,
    /// Everything changed is under [`DOCS_PATHS`].
    Docs,
    /// Code: the note of the release.
    Code(String),
}

/// GitHub's compare of `tag` with `commit`.
fn compare(gh: &dyn Gh, repo: &str, tag: &str, commit: &str) -> Result<Value, Error> {
    gh.api(&format!("repos/{repo}/compare/{tag}...{commit}"))?
        .ok_or_else(|| Error::Failed(format!("GitHub does not know {tag}...{commit}")))
}

/// Whether `compared` (`tag...commit`) has commits after `tag`: false when the tag covers the
/// commit (it is the tag's commit, or older), an error when the tag is not on its history.
fn ahead(compared: &Value, tag: &str, commit: &str) -> Result<bool, Error> {
    match compared["status"].as_str() {
        Some("ahead") => Ok(true),
        Some("identical" | "behind") => Ok(false),
        other => Err(Error::Failed(format!(
            "{tag} is not an ancestor of {commit} (compare says {other:?})"
        ))),
    }
}

/// What `compared` (`tag...commit`) says the commits after `tag` are.
fn changes(compared: &Value, tag: &str, commit: &str) -> Result<Changes, Error> {
    if !ahead(compared, tag, commit)? {
        return Ok(Changes::Released);
    }
    let files: Vec<String> = compared["files"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|f| [&f["filename"], &f["previous_filename"]])
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect();
    if files.len() < FILES_LIMIT && docs_only(&files) {
        return Ok(Changes::Docs);
    }
    let titles: Vec<String> = compared["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["commit"]["message"].as_str())
        .map(|m| {
            one_line(m.lines().next().unwrap_or_default())
                .trim()
                .to_owned()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let shown = titles.len().min(NOTE_TITLES);
    if shown == 0 {
        return Ok(Changes::Code(format!(
            "The push run of {commit} on {MAIN}."
        )));
    }
    let more = match titles.len() - shown {
        0 => String::new(),
        n => format!("; and {n} more"),
    };
    Ok(Changes::Code(format!(
        "Changes since {tag}: {}{more}.",
        titles[..shown].join("; ")
    )))
}

/// The first parent of `commit`, from GitHub; none for a root commit. A commit GitHub does not
/// know is an error: every commit asked is on the history of the tip of `main`.
fn parent_of(gh: &dyn Gh, repo: &str, commit: &str) -> Result<Option<String>, Error> {
    let found = gh
        .api(&format!("repos/{repo}/commits/{commit}"))?
        .ok_or_else(|| Error::Failed(format!("GitHub does not know {commit}")))?;
    Ok(found["parents"][0]["sha"].as_str().map(str::to_owned))
}

/// The commit `compared` (`tag...tip`, ahead) says the tag is: the walk ends above it.
fn base_of(compared: &Value, tag: &str) -> Result<String, Error> {
    compared["merge_base_commit"]["sha"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Failed(format!("GitHub's compare names no commit for {tag}")))
}

/// The version the push run `run` of [`BUILD_WORKFLOW`] built its binary under, from the name
/// of its [`VERSION_ARTIFACT`] artifact; none when the run has none (it predates the record).
/// The highest when it has several: a re-run computes the version again, never lower, and the
/// files a release downloads are the latest attempt's.
fn stamped(gh: &dyn Gh, repo: &str, run: u64) -> Result<Option<String>, Error> {
    let listed = gh.api(&format!(
        "repos/{repo}/actions/runs/{run}/artifacts?per_page=100"
    ))?;
    let listed = listed.as_ref().map_or(&Value::Null, |v| &v["artifacts"]);
    Ok(listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str()?.strip_prefix(VERSION_ARTIFACT))
        .filter_map(|v| parse_tag(&format!("v{v}")))
        .max()
        .map(|v| show(v)[1..].to_owned()))
}

/// Why `commit` is not released as `tag`; none when it is ready: both workflows have a
/// successful push run for it, and its build run recorded the version of `tag`.
fn not_ready(gh: &dyn Gh, repo: &str, commit: &str, tag: &str) -> Result<Option<String>, Error> {
    let waiting = |workflow: &str| format!("no successful push run of {workflow} on {MAIN} yet");
    let Some(run) = push_run(gh, repo, BUILD_WORKFLOW, commit)? else {
        return Ok(Some(waiting(BUILD_WORKFLOW)));
    };
    if push_run(gh, repo, CI_WORKFLOW, commit)?.is_none() {
        return Ok(Some(waiting(CI_WORKFLOW)));
    }
    Ok(match stamped(gh, repo, run)? {
        Some(version) if version == tag[1..] => None,
        Some(version) => Some(format!(
            "built as {version}, not {tag}; a newer commit's release carries it"
        )),
        None => Some(format!(
            "run {run} records no {VERSION_ARTIFACT}<X.Y.Z> artifact, so the version its binary says is not known"
        )),
    })
}

/// Appends `lines` to `output`, if there is one, and returns the lines to print.
fn answer(output: Option<&Path>, lines: Vec<String>) -> Result<Vec<String>, Error> {
    if let Some(output) = output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}

/// No release, for `reason`.
fn skip(output: Option<&Path>, reason: String) -> Result<Vec<String>, Error> {
    let mut printed = vec![format!("no release: {reason}")];
    printed.extend(answer(
        output,
        vec!["release=false".to_owned(), format!("reason={reason}")],
    )?);
    Ok(printed)
}

/// A release of `commit` as `tag`.
fn release(
    output: Option<&Path>,
    tag: &str,
    commit: &str,
    note: &str,
) -> Result<Vec<String>, Error> {
    let lines = vec![
        "release=true".to_owned(),
        format!("tag={tag}"),
        format!("commit={commit}"),
        format!("note={}", one_line(note)),
    ];
    let mut printed = vec![format!("release {tag} of {commit}")];
    printed.extend(answer(output, lines)?);
    Ok(printed)
}

/// The highest release tag of the repository, if there is one.
fn latest_tag(gh: &dyn Gh, repo: &str) -> Result<Option<String>, Error> {
    let tags = release_tags(gh, repo)?;
    Ok(tags
        .into_iter()
        .filter_map(|t| parse_tag(&t).map(|v| (v, t)))
        .max()
        .map(|(_, t)| t))
}

/// The `release version` verb: the lines to print. The version the next release of the
/// repository has (`<major>.<minor>.<patch>`, no `v`): the one computation the build gives the
/// binary and the plan names the tag by. They differ when a release is published between the
/// build and the plan; the plan then passes the commit over, which it knows by the version
/// written to `record` (the file `artifacts.yml` uploads as [`VERSION_ARTIFACT`]`<X.Y.Z>`).
pub fn version(
    gh: &dyn Gh,
    repo: &str,
    version_file: Option<&str>,
    output: Option<&Path>,
    record: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let latest = latest_tag(gh, repo)?;
    let tag = next_tag(latest.as_deref(), version_file)?;
    if let Some(record) = record {
        std::fs::write(record, format!("{}\n", &tag[1..]))
            .map_err(Error::spawn(format!("write {}", record.display())))?;
    }
    answer(output, vec![format!("version={}", &tag[1..])])
}

/// The `release plan` verb: the lines to print.
pub fn plan(gh: &dyn Gh, request: &Request) -> Result<Vec<String>, Error> {
    let Request {
        repo,
        commit: tip,
        version,
        freeze,
        output,
    } = *request;
    full_commit("--commit", tip)?;
    if frozen(freeze) {
        return skip(output, "RELEASE_FREEZE is set".to_owned());
    }
    let latest = latest_tag(gh, repo)?;
    let tag = next_tag(latest.as_deref(), version)?;
    // The tag's commit, where the walk ends, and the tip's compare, kept for the tip's note.
    let (base, tip_compared) = match &latest {
        None => (None, None),
        Some(latest) => {
            let compared = compare(gh, repo, latest, tip)?;
            if !ahead(&compared, latest, tip)? {
                return skip(output, format!("{latest} already covers {tip}"));
            }
            (Some(base_of(&compared, latest)?), Some(compared))
        }
    };
    // The first-parent history, newest first, one commit at a time: a commit a merge brought in
    // was never the tip of main and is not on it.
    let mut passed = Vec::new();
    let mut commit = tip.to_owned();
    let mut cut = false;
    loop {
        if let Some(reason) = not_ready(gh, repo, &commit, &tag)? {
            passed.push(format!("{commit}: {reason}"));
            let Some(base) = &base else { break };
            match parent_of(gh, repo, &commit)? {
                Some(parent) if parent != *base => {
                    if passed.len() == WALK_LIMIT {
                        cut = true;
                        break;
                    }
                    commit = parent;
                    continue;
                }
                _ => break,
            }
        }
        let Some(latest) = &latest else {
            return release(
                output,
                &tag,
                &commit,
                &format!("The push run of {commit} on {MAIN}."),
            );
        };
        let compared = match tip_compared {
            Some(compared) if commit == tip => compared,
            _ => compare(gh, repo, latest, &commit)?,
        };
        return match changes(&compared, latest, &commit)? {
            Changes::Released => skip(output, format!("{latest} already covers {commit}")),
            Changes::Docs => skip(
                output,
                format!("only docs changed since {latest}: nothing in a release differs"),
            ),
            Changes::Code(note) => release(output, &tag, &commit, &note),
        };
    }
    let unjudged = if cut {
        "; older commits not judged"
    } else {
        ""
    };
    skip(
        output,
        format!(
            "no commit of {MAIN} is ready to be {tag}: {}{unjudged}",
            passed.join("; ")
        ),
    )
}

/// The `release manual` verb: the lines to print.
pub fn manual(
    gh: &dyn Gh,
    repo: &str,
    tag: &str,
    commit: &str,
    note: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !is_release_tag(tag) {
        return Err(Error::Usage(format!(
            "--tag {tag:?} is not v<major>.<minor>.<patch>"
        )));
    }
    // A commit the highest tag already covers has its release: a second tag name for it is not
    // made. (A commit GitHub does not know, or off the tag's history, is left to verify.)
    if let Some(latest) = latest_tag(gh, repo)? {
        let compared = gh.api(&format!("repos/{repo}/compare/{latest}...{commit}"))?;
        let status = compared.as_ref().and_then(|c| c["status"].as_str());
        if matches!(status, Some("identical" | "behind")) {
            return Err(Error::Failed(format!(
                "{latest} already covers {commit}: no second tag is made for a released commit"
            )));
        }
    }
    release(output, tag, commit, note)
}

#[cfg(test)]
mod tests;
