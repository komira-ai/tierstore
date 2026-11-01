//! `release plan` and `release manual`: which tag a push to `main` is released as, or whether
//! it is released at all. `release.yml` runs one of them first and feeds its outputs
//! (`release`, `tag`, `commit`, `note`, or `release=false` and a `reason`) to the jobs that
//! check and publish. `release version` ([`version`]) prints the version part of the same
//! computation (`X.Y.Z`), which `artifacts.yml` gives the build before it builds, so the binary
//! reports the tag it is released under.
//!
//! - `release plan --repo <owner/repo> --commit <sha> [--version-file <path>] [--freeze <value>]
//!   [--output <file>] [--gh <program>]` ([`plan`]): no release when `--freeze` is `true`
//!   (the repository variable `RELEASE_FREEZE`); when the commit does not have a successful
//!   push run of both [`BUILD_WORKFLOW`] and [`CI_WORKFLOW`] yet (release.yml starts when
//!   either finishes, and the one that finishes last releases); when a release already covers
//!   the commit; or when everything since the highest release tag is under [`DOCS_PATHS`].
//!   Otherwise the tag is the patch after the highest `v<major>.<minor>.<patch>` tag, unless
//!   the `VERSION` file names a higher version, which is then the tag ([`next_tag`]). A
//!   `VERSION` at or below the highest tag is a floor, not a pin: it does not stop the bump,
//!   so a file nobody updated never makes a push fail on a tag that exists. The change is
//!   measured from the highest tag, not from the push, so the code of a push whose run was
//!   replaced or failed goes out with the next release.
//! - `release manual --tag <tag> --commit <sha> [--note <text>] [--output <file>]`
//!   ([`manual`]): the same outputs for a run started by hand, which names its own tag and
//!   commit and is not held by the freeze.

use std::path::Path;

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

/// The version of the first release.
const FIRST: Version = (0, 1, 0);

type Version = (u64, u64, u64);

/// What `release plan` is asked.
pub struct Request<'a> {
    pub repo: &'a str,
    pub commit: &'a str,
    /// The text of the `VERSION` file, if there is one.
    pub version: Option<&'a str>,
    /// The value of `RELEASE_FREEZE`: `true` freezes.
    pub freeze: &'a str,
    /// Where `key=value` lines are appended (on Actions, `GITHUB_OUTPUT`).
    pub output: Option<&'a Path>,
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

/// Compare `tag` with `commit`.
fn changes_since(gh: &dyn Gh, repo: &str, tag: &str, commit: &str) -> Result<Changes, Error> {
    let compared = gh
        .api(&format!("repos/{repo}/compare/{tag}...{commit}"))?
        .ok_or_else(|| Error::Failed(format!("GitHub does not know {tag}...{commit}")))?;
    match compared["status"].as_str() {
        Some("ahead") => {}
        Some("identical" | "behind") => return Ok(Changes::Released),
        other => {
            return Err(Error::Failed(format!(
                "{tag} is not an ancestor of {commit} (compare says {other:?})"
            )));
        }
    }
    let files: Vec<String> = compared["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["filename"].as_str().map(str::to_owned))
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
/// binary and the plan names the tag by, so the two cannot differ unless a tag appears between.
pub fn version(
    gh: &dyn Gh,
    repo: &str,
    version_file: Option<&str>,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let latest = latest_tag(gh, repo)?;
    let tag = next_tag(latest.as_deref(), version_file)?;
    answer(output, vec![format!("version={}", &tag[1..])])
}

/// The `release plan` verb: the lines to print.
pub fn plan(gh: &dyn Gh, request: &Request) -> Result<Vec<String>, Error> {
    let Request {
        repo,
        commit,
        version,
        freeze,
        output,
    } = *request;
    full_commit("--commit", commit)?;
    if freeze == "true" {
        return skip(output, "RELEASE_FREEZE is true".to_owned());
    }
    for workflow in [BUILD_WORKFLOW, CI_WORKFLOW] {
        if push_run(gh, repo, workflow, commit)?.is_none() {
            return skip(
                output,
                format!(
                    "{commit} has no successful push run of {workflow} on {MAIN} yet; the run that finishes last releases"
                ),
            );
        }
    }
    let latest = latest_tag(gh, repo)?;
    let note = match &latest {
        None => format!("The push run of {commit} on {MAIN}."),
        Some(tag) => match changes_since(gh, repo, tag, commit)? {
            Changes::Released => {
                return skip(output, format!("{tag} already covers {commit}"));
            }
            Changes::Docs => {
                return skip(
                    output,
                    format!("only docs changed since {tag}: nothing in a release differs"),
                );
            }
            Changes::Code(note) => note,
        },
    };
    let tag = next_tag(latest.as_deref(), version)?;
    release(output, &tag, commit, &note)
}

/// The `release manual` verb: the lines to print.
pub fn manual(
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
    release(output, tag, commit, note)
}

#[cfg(test)]
mod tests;
