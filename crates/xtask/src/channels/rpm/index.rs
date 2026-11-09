//! `channels rpm repo index --suite <s> --current <dir> --out <dir> --rpms <dir> --version <V>
//! --date <epoch> [--keep <n>]`: the packages merged into the `repodata` `rpm repo read` copied to
//! `<current>`.
//!
//! The repository is shared with other products. Their packages are kept as the text they were
//! written as, and the `repodata` of each architecture is rebuilt with the signed package of the
//! release added ([`super::repodata::merge`]): a version older than the newest listed for tierstore
//! is refused, and so is the same version with other bytes. Everything is computed before anything
//! is written; the new unsigned `repodata` goes to `<out>/<suite>/<arch>/repodata/` and the ETags
//! it was built from to `<out>/etags.txt`. `--rpms` holds the signed packages, `rpm sign`'s output.

use std::path::Path;

use super::repodata::{self, Package};
use super::state::{self, ARCHS, ETAGS_FILE, REPOMD, Suite};
use crate::Error;
use crate::fetch::Sha256;
use crate::package::read;

/// How many versions of a package and architecture the repository keeps, by default.
pub const KEEP: usize = 25;

/// What `rpm repo index` is asked.
pub struct Request<'a> {
    pub suite: Suite,
    pub current: &'a Path,
    pub out: &'a Path,
    pub rpms: &'a Path,
    pub version: &'a str,
    pub date: u64,
    pub keep: usize,
}

/// The packages the `repodata` of `arch` in `dir` lists (what `rpm repo read` wrote); none when
/// there is no `repomd.xml` yet.
///
/// # Errors
/// A `repomd.xml` that lists no `primary`, `filelists` or `other`, a file that is not there, is
/// not gzip or is not the file `repomd.xml` lists, or metadata that is not read.
pub(super) fn read_repo(dir: &Path, suite: Suite, arch: &str) -> Result<Vec<Package>, Error> {
    let repodata = dir.join(suite.name()).join(arch).join("repodata");
    let Ok(text) = std::fs::read_to_string(repodata.join(REPOMD)) else {
        return Ok(Vec::new());
    };
    let mut xml = Vec::new();
    for kind in ["primary", "filelists", "other"] {
        let (_, name, sum) = repodata::listed(&text)?
            .into_iter()
            .find(|(k, _, _)| k == kind)
            .ok_or_else(|| {
                Error::Failed(format!("{} lists no {kind}", state::path(arch, REPOMD)))
            })?;
        let gz = read(&repodata.join(&name))?;
        if Sha256::of(&gz).to_string() != sum {
            return Err(Error::Failed(format!(
                "{name} does not have the digest its repomd.xml lists"
            )));
        }
        xml.push(
            String::from_utf8(repodata::gunzip(&gz)?)
                .map_err(|_| Error::Failed(format!("{name} is not UTF-8")))?,
        );
    }
    repodata::parse(&xml[0], &xml[1], &xml[2])
}

/// Writes the unsigned `repodata` of `packages` for `arch` to `out`.
pub(super) fn write_repo(
    out: &Path,
    suite: Suite,
    arch: &str,
    packages: &[Package],
    date: u64,
) -> Result<(), Error> {
    let dir = out.join(suite.name()).join(arch).join("repodata");
    std::fs::create_dir_all(&dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    for (name, bytes) in repodata::files(packages, date) {
        std::fs::write(dir.join(&name), bytes).map_err(Error::spawn(format!("write {name}")))?;
    }
    Ok(())
}

/// The ETags `rpm repo read` kept in `dir`.
pub(super) fn etags_of(dir: &Path, hint: &str) -> Result<String, Error> {
    let path = dir.join(ETAGS_FILE);
    std::fs::read_to_string(&path)
        .map_err(|_| Error::Failed(format!("{} is missing: run `{hint}` first", path.display())))
}

/// The `rpm repo index` verb: the lines to print.
///
/// # Errors
/// No `etags.txt` in `current`, a package that is missing or that says other than its name says,
/// a repository that cannot be read or merged, or a file that cannot be written.
pub fn build(r: &Request) -> Result<Vec<String>, Error> {
    let etags = etags_of(r.current, "rpm repo read")?;
    let mut added: Vec<Package> = Vec::new();
    for (name, path) in super::package_files(r.rpms, r.version)? {
        added.push(repodata::package_of(&name, &read(&path)?)?);
    }
    let (mut new, mut dropped, mut listed) = (0, 0, 0);
    let mut next = Vec::new();
    for arch in ARCHS {
        let mine: Vec<Package> = added.iter().filter(|p| p.arch == arch).cloned().collect();
        let merged = repodata::merge(read_repo(r.current, r.suite, arch)?, &mine, r.keep)?;
        (new, dropped, listed) = (
            new + merged.added,
            dropped + merged.dropped,
            listed + merged.packages.len(),
        );
        next.push((arch, merged.packages));
    }
    for (arch, packages) in &next {
        write_repo(r.out, r.suite, arch, packages, r.date)?;
    }
    // The new repodata was built from exactly the objects whose ETags these are.
    let path = r.out.join(ETAGS_FILE);
    std::fs::write(&path, etags).map_err(Error::spawn(format!("write {}", path.display())))?;
    Ok(vec![format!(
        "rpm repo index: added {new}, dropped {dropped}, listed {listed}"
    )])
}

#[cfg(test)]
mod tests;
