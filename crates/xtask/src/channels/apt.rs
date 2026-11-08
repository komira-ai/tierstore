//! The apt channel: `tierstore_<version>_{amd64,arm64}.deb` into the apt repository the
//! organisation's products share, `packages.komira.ai`. `.github/workflows/release.yml` runs these
//! verbs in three jobs, split so that no job holds both the signing key and the cloud role;
//! `docs/design/distribution.md` (sections 5 and 8) is the design. The verbs are ported from
//! buildrules' `tools/channels/apt.zig` and `tools/deploy/apt_publish.zig`, which publish into the
//! same repository, and write the same text.
//!
//! - `channels apt read --bucket <b> --out <dir>` ([`read`]): the current index and the ETag of
//!   each of its objects, from S3 (never through the CDN, which caches the index and would hand
//!   back a stale ETag). Job `apt-deploy-read`: the cloud role, no key.
//! - `channels apt index --current <dir> --out <dir> --debs <dir> --version <V> ...`
//!   ([`build`]): the packages merged into that index, every other product's entries kept as they
//!   are. Reads no cloud and no key.
//! - `channels apt sign --dir <dir> --work <dir> --fingerprint <fpr> --key-file <f>
//!   --passphrase-file <f>` ([`sign`]): `InRelease` and `Release.gpg`, verified. Job `apt-index`:
//!   the key, no cloud role.
//! - `channels apt write --bucket <b> --distribution-id <id> --hostname <h> --next <dir> --debs
//!   <dir> --version <V>` ([`write`]): the packages into the pool with `If-None-Match`, the index
//!   with `If-Match` on the ETags that were read. Job `apt-deploy-write`: the cloud role, no key.
//!
//! The programs that talk to the outside (`aws`, `gpg`) are flags, so a test hands a stand-in
//! ([`super::aws::Aws`], [`sign::Gpg`]).

pub mod build;
pub mod control;
pub mod deb;
pub mod index;
pub mod read;
pub mod sign;
pub mod state;
pub mod write;

#[cfg(test)]
pub(crate) mod testing;

use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::aws::{Answer, RealAws};
use crate::Error;
use crate::package::Platform;
use crate::release::deb::{architecture, file_name};
use crate::release::hostkeeper_package::is_plain_version;

/// The `channels apt` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// The current index and the ETag of each of its objects, from S3.
    Read {
        /// The apt repository's bucket.
        #[arg(long)]
        bucket: String,
        /// Where the copy and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// The packages merged into the index that was read.
    Index {
        /// The directory `apt read` wrote.
        #[arg(long)]
        current: PathBuf,
        /// Where the new unsigned index and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The release's files (the `release-assets` artifact).
        #[arg(long)]
        debs: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The `Origin` of a first `Release`; an existing one keeps its own.
        #[arg(long)]
        origin: String,
        /// The `Label` of a first `Release`.
        #[arg(long)]
        label: String,
        /// The `Description` of a first `Release`.
        #[arg(long)]
        description: String,
        /// The `Date` of the `Release`, seconds since the epoch.
        #[arg(long)]
        date: u64,
        /// How many versions of a package and architecture the index keeps.
        #[arg(long, default_value_t = build::KEEP)]
        keep: usize,
    },
    /// `InRelease` and `Release.gpg`, verified.
    Sign {
        /// `dists/stable` of the new index.
        #[arg(long)]
        dir: PathBuf,
        /// Where the private keyring is made; it is removed at the end.
        #[arg(long)]
        work: PathBuf,
        /// The signing subkey's fingerprint, or several separated by commas (a key rotation).
        #[arg(long)]
        fingerprint: String,
        /// The armored private key; removed at the end.
        #[arg(long)]
        key_file: PathBuf,
        /// The file holding the key's passphrase; removed at the end.
        #[arg(long)]
        passphrase_file: PathBuf,
        /// The `gpg` program.
        #[arg(long, default_value = "gpg")]
        gpg: PathBuf,
        /// Append `signed=true` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The packages and the signed index into the repository.
    Write {
        /// The apt repository's bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate `/apt/dists/*` in.
        #[arg(long)]
        distribution_id: String,
        /// The repository's host name, for the closing message.
        #[arg(long)]
        hostname: String,
        /// The directory `apt index` wrote, signed by `apt sign`.
        #[arg(long)]
        next: PathBuf,
        /// The release's files (the `release-assets` artifact).
        #[arg(long)]
        debs: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// This is the last attempt: a lost race fails instead of asking for another.
        #[arg(long)]
        last: bool,
        /// Append `conflict=true` or `conflict=false` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Read { bucket, out, aws } => read::read(&RealAws(aws), &bucket, &out),
        Verb::Index {
            current,
            out,
            debs,
            version,
            origin,
            label,
            description,
            date,
            keep,
        } => build::build(&build::Request {
            current: &current,
            out: &out,
            debs: &debs,
            version: &version,
            origin: &origin,
            label: &label,
            description: &description,
            date,
            keep,
        }),
        Verb::Sign {
            dir,
            work,
            fingerprint,
            key_file,
            passphrase_file,
            gpg,
            output,
        } => sign::sign(
            &sign::RealGpg(gpg),
            &sign::Request {
                dir: &dir,
                work: &work,
                fingerprints: &fingerprint,
                key_file: &key_file,
                passphrase_file: &passphrase_file,
            },
            output.as_deref(),
        ),
        Verb::Write {
            bucket,
            distribution_id,
            hostname,
            next,
            debs,
            version,
            last,
            output,
            aws,
        } => write::write(
            &RealAws(aws),
            &write::Request {
                bucket: &bucket,
                distribution_id: &distribution_id,
                hostname: &hostname,
                next: &next,
                debs: &debs,
                version: &version,
                last,
            },
            output.as_deref(),
        ),
    }
}

/// What an `aws` call that failed was, from its standard error.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The object is not there (`NoSuchKey`).
    NotFound,
    /// S3 answers `AccessDenied` for a key that does not exist when the caller may not list the
    /// bucket, as the publish role may not.
    Denied,
    /// A conditional write lost: the version it named is not the current one (`412`), or another
    /// write to the same key is in flight (`409`).
    Precondition,
    /// Anything else.
    Other,
}

/// What a failed `aws` call was.
pub fn classify(stderr: &str) -> Failure {
    let has = |words: &[&str]| words.iter().any(|w| stderr.contains(w));
    if has(&["PreconditionFailed", "ConditionalRequestConflict", "(412)"]) {
        Failure::Precondition
    } else if has(&["NoSuchKey", "(404)", "Not Found"]) {
        Failure::NotFound
    } else if has(&["AccessDenied", "(403)", "Forbidden"]) {
        Failure::Denied
    } else {
        Failure::Other
    }
}

/// `aws <args>` as the verbs ask it.
pub(crate) fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// The words of a failed call for a message.
pub(crate) fn why(answer: &Answer) -> String {
    answer.stderr.trim().to_owned()
}

/// The package files of `version` in `dir`, `(name, path)`, one per Debian architecture, sorted by
/// name.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, or a package that is not there.
pub fn package_files(dir: &Path, version: &str) -> Result<Vec<(String, PathBuf)>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    let mut files: Vec<(String, PathBuf)> = Platform::ALL
        .iter()
        .filter_map(|p| architecture(*p))
        .map(|arch| {
            let name = file_name(version, arch);
            let path = dir.join(&name);
            (name, path)
        })
        .collect();
    files.sort();
    match files.iter().find(|(_, path)| !path.is_file()) {
        Some((name, _)) => Err(Error::Failed(format!("{name} is not in {}", dir.display()))),
        None => Ok(files),
    }
}

#[cfg(test)]
mod tests;
