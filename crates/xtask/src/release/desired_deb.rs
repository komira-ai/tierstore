//! `release desired-deb --repo <owner/repo> --version <X.Y.Z> --dir <dir> --apt-url <url> [--output
//! <file>] [--gh <program>] [--curl <program>]`: what `release.yml` sets as the desired version of
//! tierstore on the hostkeeper coordinator for a release that is in the suite `testing`, or why it
//! sets nothing.
//!
//! The lab installs the Debian package of the release from the testing suite, so the version the
//! coordinator is given is the package's (`<X.Y.Z>`, what `apt-get install tierstore=<X.Y.Z>`
//! takes) and the digest it records pins the packages of both architectures. The suite's
//! `Packages` files are read from the repository as a client reads them (the CDN, `--apt-url`),
//! and the entry of each package is held to the digest the release's `SHA256SUMS` lists for the
//! file:
//!
//! - the release is a published GitHub release of `<owner/repo>` that has `SHA256SUMS`;
//! - `SHA256SUMS` lists `tierstore_<X.Y.Z>_amd64.deb` and `..._arm64.deb`;
//! - the `Packages` of `dists/testing/main/binary-<arch>` lists each, in the pool, with that digest.
//!
//! Every problem found is reported, one line each. When there is none, `--output` gets
//! `package-version=<X.Y.Z>` and `package-digest=`, the SHA-256 of the text `<sha256>  <name>\n`
//! of the two packages in file-name order.

use std::path::Path;

use super::{Gh, append_output};
use crate::Error;
use crate::channels::apt::index::parse_packages;
use crate::channels::apt::{control, package_names, state::POOL_DIR};
use crate::channels::check::Host;
use crate::fetch::Sha256;
use crate::sums::{self, SUMS};

/// What `release desired-deb` is asked.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub curl: &'a Path,
    pub repo: &'a str,
    pub version: &'a str,
    pub dir: &'a Path,
    pub apt_url: &'a str,
}

/// The `Packages` of the testing suite for an architecture.
fn packages_url(apt_url: &str, arch: &str) -> String {
    format!("{apt_url}/dists/testing/main/binary-{arch}/Packages")
}

/// The architecture a package file is for (`tierstore_<version>_<arch>.deb`).
fn arch_of(name: &str) -> &str {
    name.trim_end_matches(".deb")
        .rsplit('_')
        .next()
        .unwrap_or_default()
}

/// The `release desired-deb` verb: the lines to print.
///
/// # Errors
/// A `--version` that is not `<major>.<minor>.<patch>` or an `--apt-url` that is not an https URL
/// (usage errors); no published release of that version or one without `SHA256SUMS`; a download
/// that fails; or one line per package the sums do not list or the testing suite does not list with
/// the digest of the sums.
pub fn desired_deb(
    gh: &dyn Gh,
    host: &dyn Host,
    r: &Request,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    let Request {
        curl,
        repo,
        version,
        dir,
        apt_url,
    } = *r;
    let names = package_names(version)?;
    if !apt_url.starts_with("https://") || apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {apt_url:?} is not an https URL without a trailing /"
        )));
    }
    let tag = format!("v{version}");
    let (commit, assets) = super::desired::release(gh, repo, &tag)?;
    if !assets.iter().any(|(n, _)| n == SUMS) {
        return Err(Error::Refused(vec![format!(
            "release {tag} has no {SUMS}: there are no digests to hold the testing suite to"
        )]));
    }
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let into = dir.display().to_string();
    gh.run(&[
        "release".to_owned(),
        "download".to_owned(),
        tag.clone(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--dir".to_owned(),
        into,
        "--pattern".to_owned(),
        SUMS.to_owned(),
    ])?;
    let listed = sums::read_sums(&dir.join(SUMS))?;
    let mut problems = Vec::new();
    let mut lines = Vec::new();
    for name in &names {
        let Some(line) = listed.iter().find(|l| &l.name == name) else {
            problems.push(format!("{SUMS} of release {tag} lists no {name}"));
            continue;
        };
        let arch = arch_of(name);
        let file = dir.join(format!("Packages-{arch}"));
        let file_arg = file.display().to_string();
        let url = packages_url(apt_url, arch);
        let answer = host.run(
            curl,
            &[
                "--proto".to_owned(),
                "=https".to_owned(),
                "--tlsv1.2".to_owned(),
                "--fail".to_owned(),
                "--silent".to_owned(),
                "--show-error".to_owned(),
                "--retry".to_owned(),
                "5".to_owned(),
                "--retry-delay".to_owned(),
                "20".to_owned(),
                "--retry-all-errors".to_owned(),
                "--max-time".to_owned(),
                "60".to_owned(),
                "--output".to_owned(),
                file_arg,
                url.clone(),
            ],
        )?;
        if !answer.ok {
            problems.push(format!(
                "{url} could not be read: {}",
                answer.stderr.trim_end()
            ));
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(Error::spawn(format!("read {}", file.display())))?;
        let stanzas = parse_packages(&text)
            .map_err(|e| Error::Failed(format!("{url} is not a Packages file: {e}")))?;
        let filename = format!("{POOL_DIR}/{name}");
        let entry = stanzas.iter().find(|s| {
            control::parse(&s.text)
                .is_ok_and(|f| control::get(&f, "Filename") == Some(filename.as_str()))
        });
        match entry {
            None => problems.push(format!(
                "the testing suite lists no {name}: the release is not published there"
            )),
            Some(s) if s.sha256 != line.sha256.to_string() => problems.push(format!(
                "the testing suite lists {name} with sha256 {}, but {SUMS} lists {}",
                s.sha256, line.sha256
            )),
            Some(_) => lines.push(format!("{}  {name}\n", line.sha256)),
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let digest = Sha256::of(lines.concat().as_bytes());
    let out = [
        format!("package-version={version}"),
        format!("package-digest={digest}"),
    ];
    if let Some(output) = output {
        append_output(output, &out)?;
    }
    let mut printed = vec![format!(
        "release {tag} ({commit}): the testing suite lists tierstore {version} for each of {} architectures, each with the digest {SUMS} lists",
        names.len()
    )];
    printed.extend(out);
    Ok(printed)
}

#[cfg(test)]
mod tests;
