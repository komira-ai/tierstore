//! `channels rpm repo read --suite <s> --bucket <b> --out <dir> [--aws <program>]`: the current
//! `repodata` of the suite, read from S3.
//!
//! For each architecture, `repomd.xml` and `repomd.xml.asc`, then the files `repomd.xml` lists
//! (`primary`, `filelists` and `other`), each checked against the digest `repomd.xml` gives. The
//! copy is written under `<out>/<suite>/<arch>/repodata/` and the ETag of the two replaced objects
//! of each architecture (`-` for one that does not exist yet) to `<out>/etags.txt`: the index a
//! later job builds is conditional on exactly these. Nothing is read through the CDN, which caches
//! the index and would hand back a stale ETag.

use std::path::Path;

use super::repodata::listed;
use super::state::{self, ABSENT, ARCHS, ETAGS_FILE, REPOMD, REPOMD_ASC, Suite};
use crate::Error;
use crate::channels::apt::read::etag_of;
use crate::channels::apt::{Failure, classify, strings, why};
use crate::channels::aws::Aws;
use crate::channels::upload::is_bucket;
use crate::fetch::Sha256;

/// Reads one object to `dest`: its ETag, or `-` when there is no such object (`NoSuchKey`, or
/// `AccessDenied`: the publish role may not list the bucket, so S3 answers a missing key with
/// `AccessDenied`; a wrong grant shows at the first write instead).
fn fetch(aws: &dyn Aws, bucket: &str, key: &str, dest: &Path) -> Result<String, Error> {
    let dir = dest.parent().unwrap_or(dest);
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let dest_arg = dest.display().to_string();
    let answer = aws.run(&strings(&[
        "s3api",
        "get-object",
        "--bucket",
        bucket,
        "--key",
        key,
        &dest_arg,
    ]))?;
    if answer.ok {
        return etag_of(&answer.stdout)
            .ok_or_else(|| Error::Failed(format!("get-object {key}: no ETag in the answer")));
    }
    match classify(&answer.stderr) {
        Failure::NotFound | Failure::Denied => {
            // Nothing was written, or a part of an error page: either way there is no file.
            let _ = std::fs::remove_file(dest);
            Ok(ABSENT.to_owned())
        }
        _ => Err(Error::Failed(format!(
            "get-object {key} failed: {}",
            why(&answer)
        ))),
    }
}

/// The `rpm repo read` verb: the lines to print.
///
/// # Errors
/// A bucket that is not a name, an object that could not be read for a reason other than being
/// absent, a `repomd.xml` that names a file that does not exist or has another digest, or a file
/// that could not be written.
pub fn read(aws: &dyn Aws, suite: Suite, bucket: &str, out: &Path) -> Result<Vec<String>, Error> {
    if !is_bucket(bucket) {
        return Err(Error::Usage(format!(
            "--bucket {bucket:?} is not a bucket name"
        )));
    }
    let mut entries = Vec::new();
    let mut printed = Vec::new();
    for arch in ARCHS {
        let dir = out.join(suite.name()).join(arch).join("repodata");
        let mut etags = Vec::new();
        for file in [REPOMD, REPOMD_ASC] {
            let etag = fetch(
                aws,
                bucket,
                &state::repodata_key(suite, arch, file),
                &dir.join(file),
            )?;
            printed.push(format!(
                "read {}: {}",
                state::path(arch, file),
                if etag == ABSENT {
                    "does not exist yet"
                } else {
                    &etag
                }
            ));
            entries.push((state::path(arch, file), etag.clone()));
            etags.push(etag);
        }
        if etags[0] == ABSENT {
            continue;
        }
        let text = std::fs::read_to_string(dir.join(REPOMD))
            .map_err(|_| Error::Failed(format!("{} is not UTF-8", state::path(arch, REPOMD))))?;
        for (kind, name, sum) in listed(&text)? {
            let key = state::repodata_key(suite, arch, &name);
            if fetch(aws, bucket, &key, &dir.join(&name))? == ABSENT {
                return Err(Error::Failed(format!(
                    "{} lists the {kind} file {name}, which does not exist",
                    state::path(arch, REPOMD)
                )));
            }
            let bytes =
                std::fs::read(dir.join(&name)).map_err(Error::spawn(format!("read {name}")))?;
            if Sha256::of(&bytes).to_string() != sum {
                return Err(Error::Failed(format!(
                    "{name} does not have the digest {} lists",
                    state::path(arch, REPOMD)
                )));
            }
            printed.push(format!("read {}", state::path(arch, &name)));
        }
    }
    let etags = out.join(ETAGS_FILE);
    std::fs::write(&etags, state::render_etags(&entries))
        .map_err(Error::spawn(format!("write {}", etags.display())))?;
    Ok(printed)
}

#[cfg(test)]
mod tests;
