//! `release publish`: the GitHub release of a checked directory of assets.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::{BINARY, array, check};
use crate::Error;
use crate::build_commit::full_commit;
use crate::sums::{SUMS, files_in};

/// What the release verbs ask of GitHub, so that a test can answer in place of it.
pub trait Gh {
    /// `gh api <path>`: the JSON it printed, or `None` when GitHub answered 404.
    fn api(&self, path: &str) -> Result<Option<Value>, Error>;
    /// `gh <args>`: what it printed on stdout.
    fn run(&self, args: &[String]) -> Result<String, Error>;
}

/// The `gh` program.
pub struct RealGh(pub PathBuf);

impl Gh for RealGh {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        let out = Command::new(&self.0)
            .args(["api", path])
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        let err = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            if err.contains("(HTTP 404)") {
                return Ok(None);
            }
            return Err(Error::Failed(format!("gh api {path}: {}", err.trim_end())));
        }
        serde_json::from_slice(&out.stdout)
            .map(Some)
            .map_err(|e| Error::Failed(format!("gh api {path}: not JSON: {e}")))
    }

    fn run(&self, args: &[String]) -> Result<String, Error> {
        let out = Command::new(&self.0)
            .args(args)
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Err(Error::Failed(format!(
            "gh {}: {}",
            args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )))
    }
}

/// Whether `tag` is `v<major>.<minor>.<patch>`, each a decimal number without a leading zero.
pub fn is_release_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

/// The notes of release `tag`: where the files came from and how to check them, then each
/// of `extra` as a paragraph.
pub fn notes(repo: &str, tag: &str, commit: &str, run_id: u64, extra: &[String]) -> String {
    let mut text = format!(
        "{BINARY} {tag}, built from commit {commit} by run {run_id} of \
         .github/workflows/artifacts.yml on `main`. The files are the ones that run built and \
         signed; this release rebuilt nothing.\n"
    );
    for paragraph in extra {
        text.push('\n');
        text.push_str(paragraph);
        text.push('\n');
    }
    text.push_str(&format!(
        "\nVerify a file before running it:\n\n```\n\
         gh attestation verify <file> --repo {repo} \\\n  \
         --signer-workflow {repo}/.github/workflows/artifacts.yml \\\n  \
         --source-ref refs/heads/main --source-digest {commit} \
         --deny-self-hosted-runners\n```\n\n\
         `{SUMS}` lists the SHA-256 of every file; each tarball also has its provenance bundle \
         (`<tarball>.sigstore.json`) for an offline check with `--bundle`.\n"
    ));
    text
}

/// The names of the draft release `tag` holds as assets, or none when no draft has that tag.
fn draft_assets(gh: &dyn Gh, repo: &str, tag: &str) -> Result<Option<Vec<String>>, Error> {
    let releases = gh.api(&format!("repos/{repo}/releases?per_page=100"))?;
    let found = array(&releases)
        .iter()
        .find(|r| r["tag_name"] == tag && r["draft"] == true);
    Ok(found.map(|r| {
        let mut names: Vec<String> = array(&r.get("assets").cloned())
            .iter()
            .filter_map(|a| a["name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }))
}

/// The `release publish` verb: the lines to print.
#[allow(clippy::too_many_arguments)]
pub fn publish(
    gh: &dyn Gh,
    dir: &Path,
    repo: &str,
    tag: &str,
    commit: &str,
    run_id: u64,
    notes_file: &Path,
    extra: &[String],
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !is_release_tag(tag) {
        return Err(Error::Usage(format!(
            "--tag {tag:?} is not v<major>.<minor>.<patch>"
        )));
    }
    let mut printed = check(dir, commit)?;
    let releases = gh.api(&format!("repos/{repo}/releases?per_page=100"))?;
    if array(&releases)
        .iter()
        .any(|r| r["tag_name"] == tag && r["draft"] == false)
    {
        return Err(Error::Failed(format!(
            "release {tag} exists: nothing is changed"
        )));
    }
    if gh
        .api(&format!("repos/{repo}/git/ref/tags/{tag}"))?
        .is_some()
    {
        return Err(Error::Failed(format!(
            "the tag {tag} exists already: pick another, or remove it by hand"
        )));
    }
    // A draft a failed run left holds no tag; it is replaced, never added to.
    if draft_assets(gh, repo, tag)?.is_some() {
        gh.run(&[
            "release".into(),
            "delete".into(),
            tag.into(),
            "--repo".into(),
            repo.into(),
            "--yes".into(),
        ])?;
        printed.push(format!("removed the draft {tag} an earlier run left"));
    }
    std::fs::write(notes_file, notes(repo, tag, commit, run_id, extra))
        .map_err(Error::spawn(format!("write {}", notes_file.display())))?;
    let files = files_in(dir)?;
    let mut create: Vec<String> = vec![
        "release".into(),
        "create".into(),
        tag.into(),
        "--repo".into(),
        repo.into(),
        "--target".into(),
        commit.into(),
        "--title".into(),
        tag.into(),
        "--notes-file".into(),
        notes_file.display().to_string(),
        "--draft".into(),
    ];
    create.extend(files.iter().map(|f| dir.join(f).display().to_string()));
    gh.run(&create)?;
    let held = draft_assets(gh, repo, tag)?;
    if held.as_deref() != Some(files.as_slice()) {
        gh.run(&[
            "release".into(),
            "delete".into(),
            tag.into(),
            "--repo".into(),
            repo.into(),
            "--yes".into(),
        ])?;
        return Err(Error::Failed(format!(
            "the draft {tag} holds {held:?}, not exactly {files:?}: it was deleted"
        )));
    }
    gh.run(&[
        "release".into(),
        "edit".into(),
        tag.into(),
        "--repo".into(),
        repo.into(),
        "--draft=false".into(),
    ])?;
    printed.push(format!(
        "published {tag} at {commit} with {} files",
        files.len()
    ));
    Ok(printed)
}
