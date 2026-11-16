//! `channels pkg upload|promote`: the signed macOS installer of a release on packages.komira.ai,
//! in the layout hostkeeper's `macos-pkg` installer reads (the organisation's repository
//! descriptor, `formats["macos-pkg"]`): `pkg/tierstore/<X.Y.Z>/tierstore-<X.Y.Z>-macos-arm64.pkg`.
//!
//! - `channels pkg upload <dir> --tag <tag> --commit <sha> --bucket <name> --work <dir>
//!   [--aws <program>]` ([`upload()`]): the installer, the bundle of its provenance attestation and
//!   a `SHA256SUMS` of exactly those two files (as the pointer's names list them), into
//!   `pkg/tierstore/<version>/`, once each (`--if-none-match '*'`: a second put of the same bytes
//!   is accepted, other bytes at the same key fail). The installer is the release's, signed by
//!   `artifacts.yml` before it is attested; nothing here signs. The pointer is not touched.
//! - `channels pkg promote <dir> --tag <tag> --commit <sha> --bucket <name> --work <dir>
//!   [--distribution-id <id>] [--attempts <n>] [--aws <program>]` ([`promote`]): the two files
//!   read back from the bucket and held to the release's `SHA256SUMS`, then the pointer
//!   `pkg/tierstore/stable.json` (`version`, `tag`, `commit` and each file's `sha256` and `size`)
//!   written with `--if-match` on the ETag read (`--if-none-match '*'` when there is none), only
//!   when its version is lower than this release's; a newer one is left, an equal one only
//!   invalidated, and a lost race is read and decided again, up to `--attempts` times.
//!
//! `<dir>` is the release's files (the `release-assets` artifact). Both verbs hold each file to
//! the digest `SHA256SUMS` lists before anything is written.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Value, json};

use super::apt::read::etag_of;
use super::apt::{Failure, classify};
use super::aws::{Aws, RealAws};
use super::numbers;
use super::upload::{
    ATTEMPTS, Entry, Request, check_request, digest_file, precondition_failed, put_once, refusal,
    strings, sums_text,
};
use crate::Error;
use crate::bundles::SUFFIX;
use crate::release::macos_pkg_name;
use crate::sums::{self, SUMS};

/// The cache header of the pointer: a release is seen within a minute.
pub const POINTER_CACHE: &str = "public, max-age=60";

/// The key of the pointer to the stable installer.
pub const STABLE_KEY: &str = "pkg/tierstore/stable.json";

/// The name of the pointer in `--work`.
const POINTER_FILE: &str = "stable.json";

/// The verbs of `channels pkg`.
#[derive(Subcommand)]
pub enum Verb {
    /// The installer, its bundle and their `SHA256SUMS` into `pkg/tierstore/<version>/`, once each.
    Upload {
        /// The release's files (the `release-assets` artifact).
        dir: PathBuf,
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The bucket of the repository.
        #[arg(long)]
        bucket: String,
        /// Where the `SHA256SUMS` and what is read back are staged; created when missing.
        #[arg(long)]
        work: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// `pkg/tierstore/stable.json` moved to this release, after its files are read back.
    Promote {
        /// The release's files (the `release-assets` artifact).
        dir: PathBuf,
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The bucket of the repository.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate the pointer in; empty: none.
        #[arg(long, default_value = "")]
        distribution_id: String,
        /// Where the pointer and what is read back are staged; created when missing.
        #[arg(long)]
        work: PathBuf,
        /// How many times the pointer is read and decided again after a concurrent write.
        #[arg(long, default_value_t = ATTEMPTS)]
        attempts: u32,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
}

/// Runs a verb of `channels pkg`.
///
/// # Errors
/// Those of [`upload()`] and [`promote`].
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Upload {
            dir,
            tag,
            commit,
            bucket,
            work,
            aws,
        } => upload(
            &RealAws(aws),
            &Request {
                dir: &dir,
                tag: &tag,
                commit: &commit,
                bucket: &bucket,
                distribution_id: "",
                work: &work,
                attempts: ATTEMPTS,
                latest: false,
            },
        ),
        Verb::Promote {
            dir,
            tag,
            commit,
            bucket,
            distribution_id,
            work,
            attempts,
            aws,
        } => promote(
            &RealAws(aws),
            &Request {
                dir: &dir,
                tag: &tag,
                commit: &commit,
                bucket: &bucket,
                distribution_id: &distribution_id,
                work: &work,
                attempts,
                latest: true,
            },
        ),
    }
}

/// The directory of a version in the bucket, with its trailing `/`.
fn prefix(version: &str) -> String {
    format!("pkg/tierstore/{version}/")
}

/// The installer of `version` and its bundle in `dir`, each held to the digest the release's
/// `SHA256SUMS` lists for the installer (the bundle is made after the sums and is not listed).
fn collect(dir: &Path, version: &str) -> Result<Vec<Entry>, Error> {
    let listed = sums::read_sums(&dir.join(SUMS))?;
    let installer = macos_pkg_name(version);
    let mut problems = Vec::new();
    let mut entries = Vec::new();
    for name in [format!("{installer}{SUFFIX}"), installer] {
        let path = dir.join(&name);
        if !path.is_file() {
            problems.push(format!("{} is not a file", path.display()));
            continue;
        }
        let (sha256, size) = digest_file(&path)?;
        let line = listed.iter().find(|l| l.name == name);
        if let Some(line) = line
            && line.sha256 != sha256
        {
            problems.push(format!(
                "{name} is {sha256}, but {SUMS} lists {}",
                line.sha256
            ));
        }
        if line.is_none() && !name.ends_with(SUFFIX) {
            problems.push(format!("{SUMS} does not list {name}"));
        }
        entries.push(Entry {
            name,
            path,
            sha256,
            size,
        });
    }
    if problems.is_empty() {
        Ok(entries)
    } else {
        Err(Error::Refused(problems))
    }
}

/// `SHA256SUMS` of `entries`, written to `--work`: its path and its digest.
fn write_sums(work: &Path, entries: &[Entry]) -> Result<(PathBuf, crate::fetch::Sha256), Error> {
    std::fs::create_dir_all(work).map_err(Error::spawn(format!("create {}", work.display())))?;
    let path = work.join(SUMS);
    std::fs::write(&path, sums_text(entries))
        .map_err(Error::spawn(format!("write {}", path.display())))?;
    let (digest, _) = digest_file(&path)?;
    Ok((path, digest))
}

/// The `channels pkg upload` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a file missing or not the one `SHA256SUMS` lists, a refusal by AWS, or
/// a file of the version that exists with other bytes.
pub fn upload(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    let version = check_request(r)?;
    let entries = collect(r.dir, &version)?;
    let (sums_file, digest) = write_sums(r.work, &entries)?;
    let prefix = prefix(&version);
    let mut said = Vec::new();
    for entry in &entries {
        let key = format!("{prefix}{}", entry.name);
        said.push(put_once(aws, r, &key, &entry.path, &entry.sha256)?);
    }
    said.push(put_once(
        aws,
        r,
        &format!("{prefix}{SUMS}"),
        &sums_file,
        &digest,
    )?);
    Ok(said)
}

/// The text of the pointer for this release.
fn pointer_text(version: &str, tag: &str, commit: &str, entries: &[Entry]) -> String {
    let files: serde_json::Map<String, Value> = entries
        .iter()
        .map(|e| {
            (
                e.name.clone(),
                json!({"sha256": e.sha256.to_string(), "size": e.size}),
            )
        })
        .collect();
    let pointer = json!({"version": version, "tag": tag, "commit": commit, "files": files});
    format!("{pointer}\n")
}

/// Reads `key` back into `--work` and holds it to `sha256`.
fn check_published(
    aws: &dyn Aws,
    r: &Request,
    key: &str,
    sha256: &crate::fetch::Sha256,
) -> Result<String, Error> {
    let held = r.work.join("published");
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        key,
        &held.display().to_string(),
    ]))?;
    if !got.ok {
        return Err(refusal(&format!("{key} could not be read back"), &got));
    }
    let (stored, _) = digest_file(&held)?;
    if stored != *sha256 {
        return Err(Error::Failed(format!(
            "{key} holds sha256 {stored}, but this release's file is {sha256}"
        )));
    }
    Ok(format!("{key}: sha256 {stored}"))
}

/// The pointer as stored: its ETag, its version and the version's numbers.
type Stored = (String, String, (u64, u64, u64));

/// The pointer as stored: its ETag, its version and the version's numbers; none when there is no
/// such key (or the role may not read it: S3 says `AccessDenied` for a missing key then).
fn read_pointer(
    aws: &dyn Aws,
    r: &Request,
) -> Result<Option<Stored>, Error> {
    let current = r.work.join("pointer.current");
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        STABLE_KEY,
        &current.display().to_string(),
    ]))?;
    if !got.ok {
        return match classify(&got.stderr) {
            Failure::NotFound | Failure::Denied => Ok(None),
            _ => Err(refusal(&format!("read {STABLE_KEY}"), &got)),
        };
    }
    let etag = etag_of(&got.stdout)
        .ok_or_else(|| Error::Failed(format!("read {STABLE_KEY}: aws printed no ETag")))?;
    let body =
        std::fs::read(&current).map_err(Error::spawn(format!("read {}", current.display())))?;
    let stored: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::Failed(format!("{STABLE_KEY} in the bucket is not JSON: {e}")))?;
    let (version, theirs) = stored["version"]
        .as_str()
        .and_then(|v| Some((v, numbers(v)?)))
        .ok_or_else(|| {
            Error::Failed(format!(
                "{STABLE_KEY} in the bucket has no <major>.<minor>.<patch> version"
            ))
        })?;
    Ok(Some((etag, version.to_owned(), theirs)))
}

/// Invalidates the pointer, when a distribution is given.
fn invalidate(aws: &dyn Aws, r: &Request) -> Result<Option<String>, Error> {
    if r.distribution_id.is_empty() {
        return Ok(None);
    }
    let path = format!("/{STABLE_KEY}");
    let made = aws.run(&strings(&[
        "cloudfront",
        "create-invalidation",
        "--distribution-id",
        r.distribution_id,
        "--paths",
        &path,
    ]))?;
    if !made.ok {
        return Err(refusal(&format!("invalidate {path}"), &made));
    }
    Ok(Some(format!("{path}: invalidated")))
}

/// Replaces the pointer when its version is lower than `version`.
fn replace_pointer(
    aws: &dyn Aws,
    r: &Request,
    version: &str,
    ours: (u64, u64, u64),
    pointer: &Path,
) -> Result<Vec<String>, Error> {
    for _ in 0..r.attempts {
        let stored = read_pointer(aws, r)?;
        let mut condition = strings(&["--if-none-match", "*"]);
        if let Some((etag, stored_version, theirs)) = &stored {
            match theirs.cmp(&ours) {
                Ordering::Greater => {
                    return Ok(vec![format!(
                        "{STABLE_KEY}: at {stored_version}, newer than {version}: left as it is"
                    )]);
                }
                Ordering::Equal => {
                    let mut said = vec![format!("{STABLE_KEY}: already at {version}")];
                    said.extend(invalidate(aws, r)?);
                    return Ok(said);
                }
                Ordering::Less => condition = vec!["--if-match".to_owned(), etag.clone()],
            }
        }
        let mut args = strings(&[
            "s3api",
            "put-object",
            "--bucket",
            r.bucket,
            "--key",
            STABLE_KEY,
            "--body",
            &pointer.display().to_string(),
            "--content-type",
            "application/json",
            "--cache-control",
            POINTER_CACHE,
        ]);
        args.extend(condition);
        let put = aws.run(&args)?;
        if put.ok {
            let mut said = vec![format!("{STABLE_KEY}: now {version}")];
            said.extend(invalidate(aws, r)?);
            return Ok(said);
        }
        if !precondition_failed(&put.stderr) {
            return Err(refusal(&format!("put {STABLE_KEY}"), &put));
        }
    }
    Err(Error::Failed(format!(
        "{STABLE_KEY} was written by another release {} times in a row; run the job again",
        r.attempts
    )))
}

/// The `channels pkg promote` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a file missing or not the one `SHA256SUMS` lists, a file of the version
/// that is not in the bucket or holds other bytes, a refusal by AWS, or the pointer changed under
/// every attempt.
pub fn promote(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    let version = check_request(r)?;
    // `is_plain_version` held in `check_request`; a number too large for a u64 is the only miss.
    let ours = numbers(&version)
        .ok_or_else(|| Error::Usage(format!("--tag {:?} holds a number too large", r.tag)))?;
    let entries = collect(r.dir, &version)?;
    let (_, digest) = write_sums(r.work, &entries)?;
    let pointer = r.work.join(POINTER_FILE);
    std::fs::write(&pointer, pointer_text(&version, r.tag, r.commit, &entries))
        .map_err(Error::spawn(format!("write {}", pointer.display())))?;
    let prefix = prefix(&version);
    let mut said = Vec::new();
    for entry in &entries {
        said.push(check_published(
            aws,
            r,
            &format!("{prefix}{}", entry.name),
            &entry.sha256,
        )?);
    }
    said.push(check_published(
        aws,
        r,
        &format!("{prefix}{SUMS}"),
        &digest,
    )?);
    said.extend(replace_pointer(aws, r, &version, ours, &pointer)?);
    Ok(said)
}

#[cfg(test)]
mod tests;
