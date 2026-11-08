//! `channels rpm repo write --suite <s> --bucket <b> --distribution-id <id> --hostname <h> --next
//! <dir> (--rpms <dir> --version <V> | --no-pool) [--last] [--output <file>] [--aws <program>]
//! [--curl <program>]`: the signed `repodata` and the packages into the rpm tree.
//!
//! The order is what keeps a reader consistent when the write stops anywhere:
//!
//! 1. the packages (none with `--no-pool`: the promotion of a release from `testing` to `stable`
//!    writes the index only, the pool holds the packages already), each with `If-None-Match: *`. A
//!    `412` means the file exists, and the publish role may not read the pool, so the file is
//!    fetched from the public host (`https://<hostname>/rpm/pool/...`) and kept only when its
//!    bytes are ours: a package is never overwritten, and the index must not list a file that is
//!    not this one;
//! 2. for each architecture, the `primary`, `filelists` and `other` files, named by their content
//!    (`If-None-Match`; a `412` means the same bytes are there);
//! 3. `repomd.xml`, then `repomd.xml.asc`, each with `If-Match` on the ETag `rpm repo read` saw
//!    (`If-None-Match` for an object that did not exist). The `If-Match` of `repomd.xml` is the
//!    commit point; a reader that sees the new `repomd.xml` with the old signature fails its check
//!    and tries again, and the next writer signs again.
//!
//! A `412` on `repomd.xml` or its signature means someone else wrote in between: `conflict=true` is
//! appended to `--output` and the verb ends green, so the next attempt reads again; with `--last` it
//! fails instead. Before anything is written, the new `repodata` must be complete, each file must be
//! the one `repomd.xml` lists, and its `primary.xml` must list exactly the packages to upload (by
//! file name and digest). Then `/rpm/<suite>/<arch>/repodata/*` is invalidated, for each
//! architecture.

use std::path::{Path, PathBuf};

use super::repodata::{self, listed};
use super::state::{self, ABSENT, ARCHS, ETAGS_FILE, REPOMD, REPOMD_ASC, Suite};
use crate::Error;
use crate::channels::apt::{Failure, classify, strings, why};
use crate::channels::aws::Aws;
use crate::channels::check::Host;
use crate::channels::upload::{is_bucket, is_distribution_id};
use crate::fetch::Sha256;
use crate::package::read;
use crate::release::append_output;

/// What `rpm repo write` is asked.
pub struct Request<'a> {
    pub suite: Suite,
    pub bucket: &'a str,
    pub distribution_id: &'a str,
    pub hostname: &'a str,
    pub next: &'a Path,
    /// The packages to put in the pool, or `None` to write the `repodata` only.
    pub pool: Option<Pool<'a>>,
    pub last: bool,
    pub curl: &'a Path,
}

/// The packages of a release: the directory they are in and their version.
#[derive(Clone, Copy)]
pub struct Pool<'a> {
    pub rpms: &'a Path,
    pub version: &'a str,
}

/// What one conditional put came to.
enum Put {
    Done,
    Lost,
}

/// The condition of a put: `If-Match` on an ETag that was read, or `If-None-Match` for an object
/// that does not exist.
fn condition(etag: Option<&str>) -> [String; 2] {
    match etag {
        Some(etag) if etag != ABSENT => ["--if-match".to_owned(), etag.to_owned()],
        _ => ["--if-none-match".to_owned(), "*".to_owned()],
    }
}

fn put(
    aws: &dyn Aws,
    bucket: &str,
    key: &str,
    body: &Path,
    cond: &[String; 2],
) -> Result<Put, Error> {
    let body = body.display().to_string();
    let mut args = strings(&[
        "s3api",
        "put-object",
        "--bucket",
        bucket,
        "--key",
        key,
        "--body",
        &body,
    ]);
    args.extend(cond.iter().cloned());
    let answer = aws.run(&args)?;
    if answer.ok {
        return Ok(Put::Done);
    }
    match classify(&answer.stderr) {
        Failure::Precondition => Ok(Put::Lost),
        _ => Err(Error::Failed(format!(
            "put-object {key} failed: {}",
            why(&answer)
        ))),
    }
}

/// The new `repodata` is complete, each file is what its `repomd.xml` lists, and the `primary.xml`
/// of each architecture lists the packages to upload by file name and digest. The files of each
/// architecture, content-named ones first, then `repomd.xml` and its signature.
fn check_next(
    r: &Request,
    packages: &[(String, PathBuf)],
) -> Result<Vec<(&'static str, Vec<String>)>, Error> {
    let mut all = Vec::new();
    let mut primaries = Vec::new();
    for arch in ARCHS {
        let dir = r.next.join(r.suite.name()).join(arch).join("repodata");
        let missing = |file: &str| {
            Error::Failed(format!(
                "{} is missing from the new repodata: it was not built or not signed",
                dir.join(file).display()
            ))
        };
        let repomd = std::fs::read_to_string(dir.join(REPOMD)).map_err(|_| missing(REPOMD))?;
        if !dir.join(REPOMD_ASC).is_file() {
            return Err(missing(REPOMD_ASC));
        }
        let mut names = Vec::new();
        for (kind, name, sum) in listed(&repomd)? {
            let bytes = read(&dir.join(&name)).map_err(|_| missing(&name))?;
            if Sha256::of(&bytes).to_string() != sum {
                return Err(Error::Failed(format!(
                    "{name} does not have the digest its repomd.xml lists: the repodata was changed after it was built"
                )));
            }
            if kind == "primary" {
                primaries.push(String::from_utf8_lossy(&repodata::gunzip(&bytes)?).into_owned());
            }
            names.push(name);
        }
        names.extend([REPOMD.to_owned(), REPOMD_ASC.to_owned()]);
        all.push((arch, names));
    }
    for (name, path) in packages {
        let digest = Sha256::of(&read(path)?).to_string();
        let href = format!("<location href=\"../../{}/{name}\"/>", state::POOL_DIR);
        let listed_with_digest = primaries.iter().any(|xml| {
            xml.split("</package>").any(|chunk| {
                chunk.contains(&href)
                    && chunk.contains(&format!("pkgid=\"YES\">{digest}</checksum>"))
            })
        });
        if !listed_with_digest {
            return Err(Error::Failed(format!(
                "the new repodata does not list {name} with the digest of the file to upload ({digest})"
            )));
        }
    }
    Ok(all)
}

/// Uploads one package to the pool, once. A `412` passes only if the stored file is ours.
fn upload_package(
    aws: &dyn Aws,
    host: &dyn Host,
    r: &Request,
    name: &str,
    path: &Path,
    printed: &mut Vec<String>,
) -> Result<(), Error> {
    let key = state::pool_key(name)?;
    if matches!(put(aws, r.bucket, &key, path, &condition(None))?, Put::Done) {
        printed.push(format!("uploaded {key}"));
        return Ok(());
    }
    // Kept as it is, but only if it is the same file: a different one under this name is a release
    // whose bytes changed (a build that is not reproducible, or a version published twice).
    let there = r.next.join("pool-check.rpm");
    let url = format!("https://{}/{key}", r.hostname);
    let got = host.run(
        r.curl,
        &strings(&[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--output",
            &there.display().to_string(),
            &url,
        ]),
    )?;
    if !got.ok {
        return Err(Error::Failed(format!(
            "{key} exists already and could not be fetched from {url} to compare it with the file to publish: {}",
            got.stderr.trim()
        )));
    }
    let have = Sha256::of(&read(&there)?);
    let _ = std::fs::remove_file(&there);
    let want = Sha256::of(&read(path)?);
    if have != want {
        return Err(Error::Failed(format!(
            "{key} exists already with different bytes (sha256 {have} there, {want} here): a package is never overwritten, and a different file under the same name is a failure"
        )));
    }
    printed.push(format!(
        "{key} exists already, with the same bytes; it is kept as it is"
    ));
    Ok(())
}

/// The `rpm repo write` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a new `repodata` that is incomplete or does not match the packages, a
/// put that fails for a reason other than a lost race (and a lost race with `--last`), a package at
/// a name with other bytes, or an invalidation that fails.
pub fn write(
    aws: &dyn Aws,
    host: &dyn Host,
    r: &Request,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    if !is_bucket(r.bucket) {
        return Err(Error::Usage(format!(
            "--bucket {:?} is not a bucket name",
            r.bucket
        )));
    }
    if !is_distribution_id(r.distribution_id) {
        return Err(Error::Usage(format!(
            "--distribution-id {:?} is not a distribution id",
            r.distribution_id
        )));
    }
    if r.hostname.is_empty()
        || !r
            .hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(Error::Usage(format!(
            "--hostname {:?} is not a host name",
            r.hostname
        )));
    }
    let packages = match r.pool {
        Some(pool) => super::package_files(pool.rpms, pool.version)?,
        None => Vec::new(),
    };
    let files = check_next(r, &packages)?;
    let text = std::fs::read_to_string(r.next.join(ETAGS_FILE)).map_err(|_| {
        Error::Failed(format!(
            "{} is missing from {}",
            ETAGS_FILE,
            r.next.display()
        ))
    })?;
    let etags = state::parse_etags(&text)?;
    let mut printed = Vec::new();
    for (name, path) in &packages {
        upload_package(aws, host, r, name, path, &mut printed)?;
    }
    for (arch, names) in &files {
        let dir = r.next.join(r.suite.name()).join(arch).join("repodata");
        for name in names {
            let key = state::repodata_key(r.suite, arch, name);
            // A content-named file has no ETag: a `412` means the same bytes are there.
            let replaced = name.starts_with("repomd");
            let cond = condition(state::etag_of(&etags, &state::path(arch, name)));
            match put(aws, r.bucket, &key, &dir.join(name), &cond)? {
                Put::Done => printed.push(format!("wrote {key}")),
                Put::Lost if replaced => {
                    if r.last {
                        return Err(Error::Failed(format!(
                            "{key} changed while the repodata was built, and this was the last attempt: nothing more is written"
                        )));
                    }
                    if let Some(output) = output {
                        append_output(output, &["conflict=true".to_owned()])?;
                    }
                    printed.push(format!("::notice title=rpm repodata::{key} changed while the repodata was built; the next attempt reads it again"));
                    return Ok(printed);
                }
                Put::Lost => printed.push(format!("{key} exists already; it is kept as it is")),
            }
        }
    }
    let paths: Vec<String> = ARCHS
        .iter()
        .map(|arch| format!("/rpm/{}/{arch}/repodata/*", r.suite.name()))
        .collect();
    let mut invalidation = strings(&[
        "cloudfront",
        "create-invalidation",
        "--distribution-id",
        r.distribution_id,
        "--paths",
    ]);
    invalidation.extend(paths.iter().cloned());
    let invalidated = aws.run(&invalidation)?;
    if !invalidated.ok {
        return Err(Error::Failed(format!(
            "the invalidation of {} failed: {}",
            paths.join(" "),
            why(&invalidated)
        )));
    }
    printed.push(format!(
        "invalidated {}; the repository is at https://{}/rpm/{}/<arch>/repodata/repomd.xml",
        paths.join(" "),
        r.hostname,
        r.suite.name()
    ));
    if let Some(output) = output {
        append_output(output, &["conflict=false".to_owned()])?;
    }
    Ok(printed)
}

#[cfg(test)]
mod tests;
