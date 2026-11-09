//! `channels upload <dir> --tag <tag> --commit <sha> --bucket <name> --work <dir> [--distribution-id
//! <id>] [--attempts <n>] [--aws <program>]`: the release's files on tierstore.dev, as
//! `docs/design/distribution.md` sections 3 and 8 have them.
//!
//! `<dir>` is the release's files (the `release-assets` artifact). The files of
//! `releases/<version>/` are the tap-named tarballs, the Debian packages, the bundle of each of
//! them, and a `SHA256SUMS` of exactly these, which is written to `--work`. Each is put with one
//! `aws s3api put-object ... --if-none-match '*'`, a single-part upload, the only kind the role
//! may make. Before any is put, each file's digest must be the one the release's `SHA256SUMS`
//! lists.
//!
//! A file that exists answers `412`. The stored file is then read back, and the answer is a
//! success only if its digest is the local file's: a re-run of the job passes, and a different
//! file at a version's key is never written over (the role cannot, and this refuses to pretend).
//!
//! With `--no-latest` the verb ends there: the release is on tierstore.dev under its version and no
//! pointer names it. `channels latest` (`latest` below) is the later step that does: it checks that
//! the bucket holds, byte for byte, the files the release's `SHA256SUMS` lists, and then replaces the
//! pointer as described next. The two together are the one `upload` with the pointer, in two jobs
//! with a test of the release between them.
//!
//! `releases/latest.json` comes last, so a run that stopped halfway leaves a version without a
//! pointer and never a pointer to missing files. It is read with its ETag and written with
//! `--if-match` on that ETag, only when its version is lower than this release's (none yet: with
//! `--if-none-match '*'`); a `412` reads it again and decides again, up to `--attempts` times. A
//! re-run of an older release writes nothing. Then `/releases/latest.json` is invalidated, if
//! `--distribution-id` is given and `latest.json` is at this release's version.

use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::Digest;

use super::aws::{Answer, Aws};
use super::numbers;
use crate::Error;
use crate::build_commit::full_commit;
use crate::bundles::SUFFIX;
use crate::fetch::Sha256;
use crate::release::distribution_assets;
use crate::release::hostkeeper_package::is_plain_version;
use crate::sums::{self, SUMS};

/// How often `latest.json` is read and decided again after a concurrent write, by default.
pub const ATTEMPTS: u32 = 3;

/// The cache header of a file of a version: it never changes under its URL.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The cache header of `latest.json`: a release is seen within a minute.
pub const LATEST_CACHE: &str = "public, max-age=60";

/// The key of the pointer to the newest release.
pub const LATEST_KEY: &str = "releases/latest.json";

/// The name of `latest.json` in `--work`.
const LATEST_FILE: &str = "latest.json";

/// What `upload` is asked.
pub struct Request<'a> {
    pub dir: &'a Path,
    pub tag: &'a str,
    pub commit: &'a str,
    pub bucket: &'a str,
    pub distribution_id: &'a str,
    pub work: &'a Path,
    pub attempts: u32,
    /// Replace `latest.json` after the files are in (false for `--no-latest`).
    pub latest: bool,
}

/// One file to put: its name in `releases/<version>/`, where it is, and what it is.
struct Entry {
    name: String,
    path: PathBuf,
    sha256: Sha256,
    size: u64,
}

/// The digest and size of the file at `path`, read once.
fn digest_file(path: &Path) -> Result<(Sha256, u64), Error> {
    let what = || format!("read {}", path.display());
    let mut file = std::fs::File::open(path).map_err(Error::spawn(what()))?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    let mut size = 0_u64;
    loop {
        let n = file.read(&mut buffer).map_err(Error::spawn(what()))?;
        if n == 0 {
            return Ok((Sha256(hash.finalize().into()), size));
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
}

/// Is `name` a bucket name (lower case letters, digits, `-` and `.`)?
pub(crate) fn is_bucket(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Is `id` a CloudFront distribution id (upper case letters and digits)?
pub(crate) fn is_distribution_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The version of `tag`, after the flags are held to their forms.
fn check_request(r: &Request) -> Result<String, Error> {
    let version = r
        .tag
        .strip_prefix('v')
        .filter(|v| is_plain_version(v))
        .ok_or_else(|| {
            Error::Usage(format!("--tag {:?} is not v<major>.<minor>.<patch>", r.tag))
        })?;
    full_commit("--commit", r.commit)?;
    if !is_bucket(r.bucket) {
        return Err(Error::Usage(format!(
            "--bucket {:?} is not a bucket name",
            r.bucket
        )));
    }
    if !r.distribution_id.is_empty() && !is_distribution_id(r.distribution_id) {
        return Err(Error::Usage(format!(
            "--distribution-id {:?} is not a distribution id",
            r.distribution_id
        )));
    }
    if r.attempts == 0 {
        return Err(Error::Usage(
            "--attempts is 0; it must be 1 or more".to_owned(),
        ));
    }
    Ok(version.to_owned())
}

/// The files of `releases/<version>/` in `dir`, sorted by name, each held to the digest the
/// release's `SHA256SUMS` lists (the bundles are not listed there: they are made after the sums).
fn collect(dir: &Path, version: &str) -> Result<Vec<Entry>, Error> {
    let listed = sums::read_sums(&dir.join(SUMS))?;
    let mut problems = Vec::new();
    let mut entries = Vec::new();
    let mut names = distribution_assets(version);
    names.extend(names.clone().iter().map(|n| format!("{n}{SUFFIX}")));
    names.sort();
    for name in names {
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

/// The `Content-Type` a file of `name` is served as.
fn content_type(name: &str) -> &'static str {
    if name == SUMS {
        "text/plain; charset=utf-8"
    } else if name.ends_with(SUFFIX) {
        "application/json"
    } else if name.ends_with(".deb") {
        "application/vnd.debian.binary-package"
    } else if name.ends_with(".rpm") {
        "application/x-rpm"
    } else {
        "application/gzip"
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Whether `stderr` is S3's answer to a write whose condition did not hold.
fn precondition_failed(stderr: &str) -> bool {
    stderr.contains("PreconditionFailed") || stderr.contains("(412)")
}

fn refusal(what: &str, answer: &Answer) -> Error {
    Error::Failed(format!("{what}: {}", answer.stderr.trim_end()))
}

/// Puts `path` at `key` once. A `412` passes only if the stored file's digest is `sha256`.
fn put_once(
    aws: &dyn Aws,
    r: &Request,
    key: &str,
    path: &Path,
    sha256: &Sha256,
) -> Result<String, Error> {
    let name = key.rsplit('/').next().unwrap_or(key);
    let path_text = path.display().to_string();
    let put = aws.run(&strings(&[
        "s3api",
        "put-object",
        "--bucket",
        r.bucket,
        "--key",
        key,
        "--body",
        &path_text,
        "--if-none-match",
        "*",
        "--content-type",
        content_type(name),
        "--cache-control",
        IMMUTABLE,
    ]))?;
    if put.ok {
        return Ok(format!("{key}: written"));
    }
    if !precondition_failed(&put.stderr) {
        return Err(refusal(&format!("put {key}"), &put));
    }
    let existing = r.work.join("existing");
    let existing_text = existing.display().to_string();
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        key,
        &existing_text,
    ]))?;
    if !got.ok {
        return Err(refusal(
            &format!("{key} exists, and reading it back failed"),
            &got,
        ));
    }
    let (stored, _) = digest_file(&existing)?;
    if stored == *sha256 {
        return Ok(format!("{key}: already there with the same bytes"));
    }
    Err(Error::Failed(format!(
        "{key} exists with other bytes (sha256 {stored}, this release's file is {sha256}): a file of a version is never replaced"
    )))
}

/// `SHA256SUMS` of `entries`.
fn sums_text(entries: &[Entry]) -> String {
    sums::render(
        &entries
            .iter()
            .map(|e| sums::Line {
                sha256: e.sha256,
                name: e.name.clone(),
            })
            .collect::<Vec<_>>(),
    )
}

/// The text of `latest.json` for this release.
fn latest_text(version: &str, tag: &str, commit: &str, entries: &[Entry]) -> String {
    let files: serde_json::Map<String, Value> = entries
        .iter()
        .map(|e| {
            (
                e.name.clone(),
                json!({"sha256": e.sha256.to_string(), "size": e.size}),
            )
        })
        .collect();
    let latest = json!({"version": version, "tag": tag, "commit": commit, "files": files});
    format!("{latest}\n")
}

/// `latest.json` as stored: its ETag, its version and the version's numbers.
type Stored = (String, String, (u64, u64, u64));

/// `latest.json` as stored, or none when the bucket has no such key.
fn read_latest(aws: &dyn Aws, r: &Request) -> Result<Option<Stored>, Error> {
    let current = r.work.join("latest.current");
    let current_text = current.display().to_string();
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        LATEST_KEY,
        &current_text,
    ]))?;
    if !got.ok {
        if got.stderr.contains("NoSuchKey") {
            return Ok(None);
        }
        return Err(refusal(&format!("read {LATEST_KEY}"), &got));
    }
    let said: Value = serde_json::from_str(&got.stdout)
        .map_err(|e| Error::Failed(format!("read {LATEST_KEY}: aws did not print JSON: {e}")))?;
    let etag = said["ETag"]
        .as_str()
        .ok_or_else(|| Error::Failed(format!("read {LATEST_KEY}: aws printed no ETag")))?;
    let body = std::fs::read(&current).map_err(Error::spawn(format!("read {current_text}")))?;
    let stored: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::Failed(format!("{LATEST_KEY} in the bucket is not JSON: {e}")))?;
    let (version, theirs) = stored["version"]
        .as_str()
        .and_then(|v| Some((v, numbers(v)?)))
        .ok_or_else(|| {
            Error::Failed(format!(
                "{LATEST_KEY} in the bucket has no <major>.<minor>.<patch> version"
            ))
        })?;
    Ok(Some((etag.to_owned(), version.to_owned(), theirs)))
}

/// Invalidates `/releases/latest.json`, when a distribution is given.
fn invalidate(aws: &dyn Aws, r: &Request) -> Result<Option<String>, Error> {
    if r.distribution_id.is_empty() {
        return Ok(None);
    }
    let made = aws.run(&strings(&[
        "cloudfront",
        "create-invalidation",
        "--distribution-id",
        r.distribution_id,
        "--paths",
        "/releases/latest.json",
    ]))?;
    if !made.ok {
        return Err(refusal("invalidate /releases/latest.json", &made));
    }
    Ok(Some("/releases/latest.json: invalidated".to_owned()))
}

/// Replaces `latest.json` when its version is lower than `version`.
fn replace_latest(
    aws: &dyn Aws,
    r: &Request,
    version: &str,
    ours: (u64, u64, u64),
) -> Result<Vec<String>, Error> {
    let latest = r.work.join(LATEST_FILE);
    let latest_text = latest.display().to_string();
    for _ in 0..r.attempts {
        let stored = read_latest(aws, r)?;
        let mut condition = strings(&["--if-none-match", "*"]);
        if let Some((etag, stored_version, theirs)) = &stored {
            match theirs.cmp(&ours) {
                Ordering::Greater => {
                    return Ok(vec![format!(
                        "{LATEST_KEY}: at {stored_version}, newer than {version}: left as it is"
                    )]);
                }
                Ordering::Equal => {
                    let mut said = vec![format!("{LATEST_KEY}: already at {version}")];
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
            LATEST_KEY,
            "--body",
            &latest_text,
            "--content-type",
            "application/json",
            "--cache-control",
            LATEST_CACHE,
        ]);
        args.extend(condition);
        let put = aws.run(&args)?;
        if put.ok {
            let mut said = vec![format!("{LATEST_KEY}: now {version}")];
            said.extend(invalidate(aws, r)?);
            return Ok(said);
        }
        if !precondition_failed(&put.stderr) {
            return Err(refusal(&format!("put {LATEST_KEY}"), &put));
        }
    }
    Err(Error::Failed(format!(
        "{LATEST_KEY} was written by another release {} times in a row; run the job again",
        r.attempts
    )))
}

/// The `upload` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a file missing or not the one `SHA256SUMS` lists, a refusal by AWS, a
/// file of the version that exists with other bytes, or `latest.json` changed under every
/// attempt.
pub fn upload(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    let version = check_request(r)?;
    let entries = collect(r.dir, &version)?;
    std::fs::create_dir_all(r.work)
        .map_err(Error::spawn(format!("create {}", r.work.display())))?;
    let sums_file = r.work.join(SUMS);
    std::fs::write(&sums_file, sums_text(&entries))
        .map_err(Error::spawn(format!("write {}", sums_file.display())))?;
    let latest_file = r.work.join(LATEST_FILE);
    std::fs::write(
        &latest_file,
        latest_text(&version, r.tag, r.commit, &entries),
    )
    .map_err(Error::spawn(format!("write {}", latest_file.display())))?;
    let mut said = Vec::new();
    for entry in &entries {
        let key = format!("releases/{version}/{}", entry.name);
        said.push(put_once(aws, r, &key, &entry.path, &entry.sha256)?);
    }
    let (sums_digest, _) = digest_file(&sums_file)?;
    let key = format!("releases/{version}/{SUMS}");
    said.push(put_once(aws, r, &key, &sums_file, &sums_digest)?);
    if r.latest {
        said.extend(point_latest(aws, r, &version)?);
    }
    Ok(said)
}

/// Replaces `latest.json` with this release, when it is newer than the one there.
fn point_latest(aws: &dyn Aws, r: &Request, version: &str) -> Result<Vec<String>, Error> {
    // `is_plain_version` held in `check_request`; a number too large for a u64 is the only miss.
    let ours = numbers(version)
        .ok_or_else(|| Error::Usage(format!("--tag {:?} holds a number too large", r.tag)))?;
    replace_latest(aws, r, version, ours)
}

/// Reads `releases/<version>/<name>` back into `--work` and holds it to `sha256`.
fn check_published(
    aws: &dyn Aws,
    r: &Request,
    version: &str,
    name: &str,
    sha256: &Sha256,
) -> Result<String, Error> {
    let key = format!("releases/{version}/{name}");
    let held = r.work.join("published");
    let held_text = held.display().to_string();
    let got = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        r.bucket,
        "--key",
        &key,
        &held_text,
    ]))?;
    if !got.ok {
        return Err(refusal(&format!("{key} could not be read back"), &got));
    }
    let (stored, _) = digest_file(&held)?;
    if stored != *sha256 {
        return Err(Error::Failed(format!(
            "{key} holds sha256 {stored}, but this release's {name} is {sha256}"
        )));
    }
    Ok(format!("{key}: sha256 {stored}"))
}

/// The `latest` verb: the pointer to this release, after the bucket is read back.
///
/// Every file of `releases/<version>/` and its `SHA256SUMS` are read from the bucket and held to
/// the digests of the release's `SHA256SUMS` (the files that were published are exactly the ones
/// that were checked, nothing is uploaded again), and only then is `latest.json` replaced, never
/// with an older version.
///
/// # Errors
/// A flag out of its form, a file of the release that is missing or not the one `SHA256SUMS`
/// lists, a file of the version that is not in the bucket or holds other bytes, a refusal by AWS,
/// or `latest.json` changed under every attempt.
pub fn latest(aws: &dyn Aws, r: &Request) -> Result<Vec<String>, Error> {
    let version = check_request(r)?;
    let entries = collect(r.dir, &version)?;
    std::fs::create_dir_all(r.work)
        .map_err(Error::spawn(format!("create {}", r.work.display())))?;
    let sums_file = r.work.join(SUMS);
    std::fs::write(&sums_file, sums_text(&entries))
        .map_err(Error::spawn(format!("write {}", sums_file.display())))?;
    let latest_file = r.work.join(LATEST_FILE);
    std::fs::write(
        &latest_file,
        latest_text(&version, r.tag, r.commit, &entries),
    )
    .map_err(Error::spawn(format!("write {}", latest_file.display())))?;
    let mut said = Vec::new();
    for entry in &entries {
        said.push(check_published(
            aws,
            r,
            &version,
            &entry.name,
            &entry.sha256,
        )?);
    }
    let (sums_digest, _) = digest_file(&sums_file)?;
    said.push(check_published(aws, r, &version, SUMS, &sums_digest)?);
    said.extend(point_latest(aws, r, &version)?);
    Ok(said)
}

#[cfg(test)]
mod tests;
