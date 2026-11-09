//! The rpm channel: `tierstore-<version>-1.{x86_64,aarch64}.rpm` into the rpm tree of the repository
//! the organisation's products share, `packages.komira.ai/rpm`. `.github/workflows/release.yml`
//! runs these verbs in three jobs for each suite, split so that no job holds both the signing key
//! and the cloud role, as the apt channel's are; `docs/design/distribution.md` (sections 5 and 8) is
//! the design.
//!
//! Every verb of `repo` but `sign` takes `--suite stable|testing` (default `stable`): a release is
//! published into `testing` first, and `promote` lists the same packages in `stable` once the checks
//! passed. The packages are signed once, in the job that holds the key, and the signed bytes are the
//! unsigned, attested ones plus one entry in the signature header ([`layout`], [`sign`]).
//!
//! - `channels rpm sign --rpms <dir> --out <dir> --version <V> --sums <file> --work <dir>
//!   --fingerprint <fpr> --key-file <f> --passphrase-file <f> --signed-at <epoch>` ([`sign`]): each
//!   package signed, verified, and listed with its new digest in `SHA256SUMS.signed`.
//! - `channels rpm check-signed --dir <dir> --version <V> --sums <file>` ([`sign`]): each package is
//!   signed, undamaged, and the attested package once its signature is taken out. Reads no key.
//! - `channels rpm repo read --bucket <b> --out <dir>` ([`read`]): the current `repodata` and the
//!   ETag of `repomd.xml` and `repomd.xml.asc`, from S3 (never through the CDN, which caches the
//!   index and would hand back a stale ETag). The cloud role, no key.
//! - `channels rpm repo index --current <dir> --out <dir> --rpms <dir> --version <V> --date <epoch>`
//!   ([`index`]): the signed packages merged into that `repodata`, every other product's packages
//!   kept as they are. Reads no cloud and no key.
//! - `channels rpm promote --testing <dir> --stable <dir> --out <dir> --version <V> --sums <file>
//!   --signed-sums <file> --date <epoch>` ([`promote`]): the entries of a release `testing` lists,
//!   merged into the `repodata` of `stable`; reads no cloud and no key.
//! - `channels rpm repo sign --dir <dir> --work <dir> --fingerprint <fpr> --key-file <f>
//!   --passphrase-file <f>` ([`repo_sign`]): `repomd.xml.asc` of each architecture, verified. The key.
//! - `channels rpm repo write --bucket <b> --distribution-id <id> --hostname <h> --next <dir> --rpms
//!   <dir> --version <V>` ([`write`]): the packages into the pool with `If-None-Match`, the
//!   `repodata` after them, `repomd.xml` and its signature with `If-Match` on the ETags that were
//!   read (`--no-pool` instead of `--rpms` and `--version`: the `repodata` only). The cloud role.
//!
//! The programs that talk to the outside (`aws`, `gpg`, `curl`) are flags, so a test hands a
//! stand-in ([`super::aws::Aws`], [`super::apt::sign::Gpg`], [`super::check::Host`]).

pub mod index;
pub mod layout;
pub mod promote;
pub mod read;
pub mod repo_sign;
pub mod repodata;
pub mod sign;
pub mod state;
pub mod write;

#[cfg(test)]
pub(crate) mod testing;

use std::path::{Path, PathBuf};

use clap::Subcommand;

pub use state::Suite;

use super::apt::sign::RealGpg;
use super::aws::RealAws;
use super::check::RealHost;
use crate::Error;
use crate::package::Platform;
use crate::release::hostkeeper_package::is_plain_version;
use crate::release::rpm::{architecture, file_name};

/// The file names of the packages of `version`, one per RPM architecture, sorted.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`.
pub fn package_names(version: &str) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    let mut names: Vec<String> = Platform::ALL
        .iter()
        .filter_map(|p| architecture(*p))
        .map(|arch| file_name(version, arch))
        .collect();
    names.sort();
    Ok(names)
}

/// The package files of `version` in `dir`, `(name, path)`, one per RPM architecture, sorted by
/// name.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, or a package that is not there.
pub fn package_files(dir: &Path, version: &str) -> Result<Vec<(String, PathBuf)>, Error> {
    let files: Vec<(String, PathBuf)> = package_names(version)?
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            (name, path)
        })
        .collect();
    match files.iter().find(|(_, path)| !path.is_file()) {
        Some((name, _)) => Err(Error::Failed(format!("{name} is not in {}", dir.display()))),
        None => Ok(files),
    }
}

/// The `channels rpm` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// Each package of the release signed with the archive subkey, the unsigned bytes otherwise.
    Sign {
        /// The release's files (the `release-assets` artifact).
        #[arg(long)]
        rpms: PathBuf,
        /// Where the signed packages and `SHA256SUMS.signed` are written.
        #[arg(long)]
        out: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The release's `SHA256SUMS`: each package is held to its digest.
        #[arg(long)]
        sums: PathBuf,
        /// Where the private keyring is made; it is removed at the end.
        #[arg(long)]
        work: PathBuf,
        /// The signing subkey's fingerprint, or several separated by commas (the first signs).
        #[arg(long)]
        fingerprint: String,
        /// The armored private key; removed at the end.
        #[arg(long)]
        key_file: PathBuf,
        /// The file holding the key's passphrase; removed at the end.
        #[arg(long)]
        passphrase_file: PathBuf,
        /// The time of the signature, seconds since the epoch: the same value gives the same bytes.
        #[arg(long)]
        signed_at: u64,
        /// The `gpg` program.
        #[arg(long, default_value = "gpg")]
        gpg: PathBuf,
        /// Append `signed=true` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Each package is signed and, once its signature is taken out, the attested package.
    CheckSigned {
        /// The signed packages.
        #[arg(long)]
        dir: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The release's `SHA256SUMS`.
        #[arg(long)]
        sums: PathBuf,
        /// The `SHA256SUMS.signed` that `sign` wrote; the digests of the signed files are held to it.
        #[arg(long)]
        signed_sums: Option<PathBuf>,
        /// The fingerprint of the key the signature must be by.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// The entries of a release that `testing` lists, merged into the `repodata` of `stable`.
    Promote {
        /// The directory `rpm repo read --suite testing` wrote.
        #[arg(long)]
        testing: PathBuf,
        /// The directory `rpm repo read --suite stable` wrote.
        #[arg(long)]
        stable: PathBuf,
        /// Where the new unsigned `repodata` of `stable` and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The release's `SHA256SUMS`.
        #[arg(long)]
        sums: PathBuf,
        /// The `SHA256SUMS.signed` of `sign`: the entries promoted are held to its digests.
        #[arg(long)]
        signed_sums: PathBuf,
        /// The revision of the new `repomd.xml`, seconds since the epoch.
        #[arg(long)]
        date: u64,
        /// How many versions of a package and architecture the repository keeps.
        #[arg(long, default_value_t = index::KEEP)]
        keep: usize,
    },
    /// The repository's `repodata`: read it, merge the packages, sign it, write it.
    Repo {
        #[command(subcommand)]
        verb: RepoVerb,
    },
}

/// The `channels rpm repo` verbs.
#[derive(Subcommand)]
pub enum RepoVerb {
    /// The current `repodata` and the ETag of each replaced object, from S3.
    Read {
        /// The suite of the repository: `stable`, or `testing`, where a release is published
        /// before its install checks and promoted from.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The repository's bucket.
        #[arg(long)]
        bucket: String,
        /// Where the copy and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// The signed packages merged into the `repodata` that was read.
    Index {
        /// The suite of the repository.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The directory `rpm repo read` wrote.
        #[arg(long)]
        current: PathBuf,
        /// Where the new unsigned `repodata` and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The signed packages (`rpm sign --out`).
        #[arg(long)]
        rpms: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The revision of the new `repomd.xml`, seconds since the epoch.
        #[arg(long)]
        date: u64,
        /// How many versions of a package and architecture the repository keeps.
        #[arg(long, default_value_t = index::KEEP)]
        keep: usize,
    },
    /// `repomd.xml.asc` of each architecture, verified.
    Sign {
        /// `<next>/<suite>` of the new `repodata`.
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
    /// The packages and the signed `repodata` into the repository.
    Write {
        /// The suite of the repository.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The repository's bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate `/rpm/<suite>/<arch>/repodata/*` in.
        #[arg(long)]
        distribution_id: String,
        /// The repository's host name: for the closing message, and where an existing package is
        /// fetched from to compare it.
        #[arg(long)]
        hostname: String,
        /// The directory `rpm repo index` wrote, signed by `rpm repo sign`.
        #[arg(long)]
        next: PathBuf,
        /// The signed packages of the release.
        #[arg(long, required_unless_present = "no_pool", conflicts_with = "no_pool")]
        rpms: Option<PathBuf>,
        /// `<major>.<minor>.<patch>`.
        #[arg(long, required_unless_present = "no_pool", conflicts_with = "no_pool")]
        version: Option<String>,
        /// Write the `repodata` only: the packages are in the pool already (a promotion).
        #[arg(long)]
        no_pool: bool,
        /// This is the last attempt: a lost race fails instead of asking for another.
        #[arg(long)]
        last: bool,
        /// Append `conflict=true` or `conflict=false` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
        /// The `curl` program.
        #[arg(long, default_value = "curl")]
        curl: PathBuf,
    },
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Sign {
            rpms,
            out,
            version,
            sums,
            work,
            fingerprint,
            key_file,
            passphrase_file,
            signed_at,
            gpg,
            output,
        } => sign::sign(
            &RealGpg(gpg),
            &sign::Request {
                rpms: &rpms,
                out: &out,
                version: &version,
                sums: &sums,
                work: &work,
                fingerprints: &fingerprint,
                key_file: &key_file,
                passphrase_file: &passphrase_file,
                signed_at,
            },
            output.as_deref(),
        ),
        Verb::CheckSigned {
            dir,
            version,
            sums,
            signed_sums,
            fingerprint,
        } => sign::check_signed(&sign::Check {
            dir: &dir,
            version: &version,
            sums: &sums,
            signed_sums: signed_sums.as_deref(),
            fingerprint: fingerprint.as_deref(),
        }),
        Verb::Promote {
            testing,
            stable,
            out,
            version,
            sums,
            signed_sums,
            date,
            keep,
        } => promote::promote(&promote::Request {
            testing: &testing,
            stable: &stable,
            out: &out,
            version: &version,
            sums: &sums,
            signed_sums: &signed_sums,
            date,
            keep,
        }),
        Verb::Repo { verb } => run_repo(verb),
    }
}

fn run_repo(verb: RepoVerb) -> Result<Vec<String>, Error> {
    match verb {
        RepoVerb::Read {
            suite,
            bucket,
            out,
            aws,
        } => read::read(&RealAws(aws), suite, &bucket, &out),
        RepoVerb::Index {
            suite,
            current,
            out,
            rpms,
            version,
            date,
            keep,
        } => index::build(&index::Request {
            suite,
            current: &current,
            out: &out,
            rpms: &rpms,
            version: &version,
            date,
            keep,
        }),
        RepoVerb::Sign {
            dir,
            work,
            fingerprint,
            key_file,
            passphrase_file,
            gpg,
            output,
        } => repo_sign::sign(
            &RealGpg(gpg),
            &repo_sign::Request {
                dir: &dir,
                work: &work,
                fingerprints: &fingerprint,
                key_file: &key_file,
                passphrase_file: &passphrase_file,
            },
            output.as_deref(),
        ),
        RepoVerb::Write {
            suite,
            bucket,
            distribution_id,
            hostname,
            next,
            rpms,
            version,
            no_pool,
            last,
            output,
            aws,
            curl,
        } => {
            let pool = match (&rpms, &version, no_pool) {
                (Some(rpms), Some(version), false) => Some(write::Pool { rpms, version }),
                (None, None, true) => None,
                _ => {
                    return Err(Error::Usage(
                        "give --rpms and --version, or --no-pool and neither".to_owned(),
                    ));
                }
            };
            write::write(
                &RealAws(aws),
                &RealHost,
                &write::Request {
                    suite,
                    bucket: &bucket,
                    distribution_id: &distribution_id,
                    hostname: &hostname,
                    next: &next,
                    pool,
                    last,
                    curl: &curl,
                },
                output.as_deref(),
            )
        }
    }
}

#[cfg(test)]
mod tests;
