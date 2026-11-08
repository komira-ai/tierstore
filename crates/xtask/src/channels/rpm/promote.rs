//! `channels rpm promote --testing <dir> --stable <dir> --out <dir> --version <V> --sums <file>
//! --signed-sums <file> --date <epoch> [--keep <n>]`: the entries of a release that `testing` lists,
//! merged into the `repodata` of `stable`.
//!
//! The release was published into `testing` first, its install checks ran against that, and now the
//! same bytes are made available in `stable`: nothing is built and no package is written (the pool
//! holds the files; `rpm repo write --no-pool` puts the `repodata` only). What is checked, before
//! anything is written:
//!
//! - `SHA256SUMS` lists each package of the version (the attested, unsigned one) and
//!   `SHA256SUMS.signed` lists the signed one, which `rpm sign` made from it after checking it
//!   against that line (a signing job runs again for the promotion, and gives the same bytes);
//! - `testing` lists each package of the version, from the pool file of its name, and with the
//!   digest `SHA256SUMS.signed` lists: the entry that is promoted is the one the release was
//!   verified against, not whatever the suite holds;
//! - `stable` has no newer version of the package (the channel does not move down), and no entry
//!   at this version with other bytes (the merge refuses both).
//!
//! `--testing` and `--stable` are what `rpm repo read --suite testing` and `--suite stable` wrote.
//! The new unsigned `repodata` of `stable` goes to `<out>/stable/<arch>/repodata/` and the ETags of
//! `stable` it was built from to `<out>/etags.txt`; `rpm repo sign` and `rpm repo write --suite
//! stable --no-pool` follow. Promoting a version that `stable` lists already changes no entry.

use std::path::Path;

use super::index::{etags_of, read_repo, write_repo};
use super::repodata::{self, Package};
use super::state::{ARCHS, ETAGS_FILE, Suite};
use crate::Error;
use crate::sums::{self, SUMS};

/// What `rpm promote` is asked.
pub struct Request<'a> {
    pub testing: &'a Path,
    pub stable: &'a Path,
    pub out: &'a Path,
    pub version: &'a str,
    pub sums: &'a Path,
    pub signed_sums: &'a Path,
    pub date: u64,
    pub keep: usize,
}

/// The `rpm promote` verb: the lines to print.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, no `etags.txt` in `--stable`, a sums file that
/// cannot be read or lists no such package, a package `testing` does not list or lists with a
/// digest other than the signed sums', a `stable` that is newer or lists the version with other
/// bytes, or a file that cannot be written.
pub fn promote(r: &Request) -> Result<Vec<String>, Error> {
    let names = super::package_names(r.version)?;
    let etags = etags_of(r.stable, "rpm repo read --suite stable")?;
    let attested = sums::read_sums(r.sums)?;
    let signed = sums::read_sums(r.signed_sums)?;
    let mut testing: Vec<Package> = Vec::new();
    for arch in ARCHS {
        testing.extend(read_repo(r.testing, Suite::Testing, arch)?);
    }
    let mut added = Vec::new();
    for name in &names {
        if !attested.iter().any(|l| &l.name == name) {
            return Err(Error::Failed(format!(
                "{SUMS} lists no {name}: nothing to promote"
            )));
        }
        let line = signed.iter().find(|l| &l.name == name).ok_or_else(|| {
            Error::Failed(format!(
                "the signed sums list no {name}: nothing to promote"
            ))
        })?;
        let href = format!("../../{}/{name}", super::state::POOL_DIR);
        let package = testing.iter().find(|p| p.href == href).ok_or_else(|| {
            Error::Failed(format!(
                "testing does not list {name}: the release was not published there"
            ))
        })?;
        if package.pkgid != line.sha256.to_string() {
            return Err(Error::Failed(format!(
                "testing lists {name} with sha256 {}, but the signed sums list {}: the bytes that were checked are not the ones in testing",
                package.pkgid, line.sha256
            )));
        }
        added.push(package.clone());
    }
    let (mut new, mut dropped, mut listed) = (0, 0, 0);
    let mut next = Vec::new();
    for arch in ARCHS {
        let mine: Vec<Package> = added.iter().filter(|p| p.arch == arch).cloned().collect();
        let merged = repodata::merge(read_repo(r.stable, Suite::Stable, arch)?, &mine, r.keep)?;
        (new, dropped, listed) = (
            new + merged.added,
            dropped + merged.dropped,
            listed + merged.packages.len(),
        );
        next.push((arch, merged.packages));
    }
    for (arch, packages) in &next {
        write_repo(r.out, Suite::Stable, arch, packages, r.date)?;
    }
    let path = r.out.join(ETAGS_FILE);
    std::fs::write(&path, etags).map_err(Error::spawn(format!("write {}", path.display())))?;
    Ok(vec![format!(
        "rpm promote: added {new}, dropped {dropped}, listed {listed}"
    )])
}

#[cfg(test)]
mod tests;
