//! `cargo xtask <verb>`: the CI and release steps of this repository, as Rust with tests, so a
//! workflow step is one command.
//!
//! - [`fetch`]: download a pinned tool, check its SHA-256, install it.
//! - [`package`]: one binary and its SBOM as release assets, with their `SHA256SUMS` lines.
//! - [`sums`]: assemble `SHA256SUMS` and check the files and their attestations against it.
//! - [`build_commit`]: a built binary names the commit it was built from.
//! - [`release`]: find the successful build of a commit, check its files, publish them.
//! - [`advisories`]: keep the security advisories in one issue.
//!
//! The workflows that call them are in `.github/workflows`; `tests/workflows.rs` holds each
//! of them to the rules of this repository.

pub mod advisories;
pub mod build_commit;
pub mod bundles;
pub mod fetch;
pub mod package;
pub mod release;
pub mod sums;

#[cfg(test)]
mod testdir;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not a commit: {0:?} (expected 12 to 40 lowercase hex digits)")]
    NotACommit(String),
    #[error("--version is {version:?}; expected it to end +{}", &commit[..12])]
    WrongCommit { version: String, commit: String },
    #[error("control: {version:?} also passed for {}", &other[..12])]
    ControlPassed { version: String, other: String },
    #[error("{what}: {why}")]
    Spawn { what: String, why: String },
    #[error("{0}")]
    Failed(String),
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    /// Flags that do not fit together (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// The error for a program that could not be started, or a file not read: `what`
    /// and the system's reason.
    pub(crate) fn spawn(what: String) -> impl FnOnce(std::io::Error) -> Error {
        move |e| Error::Spawn {
            what,
            why: e.to_string(),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "cargo xtask",
    about = "The CI and release steps of this repository"
)]
struct Cli {
    /// The repository root (default: the workspace this xtask was built from).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// A binary's `--version` names the commit it was built from.
    BuildCommit {
        /// The built binary (run on the machine that built it).
        binary: PathBuf,
        /// The commit, 12 to 40 lowercase hex digits.
        commit: String,
    },
    /// Keep the advisories `cargo deny` reports in one GitHub issue.
    Advisories(advisories::Args),
    /// Download a pinned file, check its SHA-256 and install it (mode 0755).
    Fetch {
        /// Where to download it from (HTTPS; redirects must stay on HTTPS).
        #[arg(long)]
        url: String,
        /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
        #[arg(long)]
        sha256: fetch::Sha256,
        /// The file to install; its directory is created when missing.
        #[arg(long)]
        out: PathBuf,
        /// How the download is packed (default: it is the file).
        #[arg(long, value_enum)]
        extract: Option<fetch::Format>,
        /// The file to take out of a tar (`--extract tar.gz` or `tar.xz`): its path inside
        /// the archive.
        #[arg(long)]
        member: Option<PathBuf>,
    },
    /// Package one binary and its SBOM as release assets in `--out`.
    Package {
        /// The binary; its file name is the asset's name.
        #[arg(long)]
        binary: PathBuf,
        /// Its CycloneDX SBOM.
        #[arg(long)]
        sbom: PathBuf,
        /// The platform it was built for.
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (on Actions, `GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The directory the assets and `SHA256SUMS.<platform>` are written to.
        #[arg(long)]
        out: PathBuf,
    },
    /// `SHA256SUMS`: assemble it from the platforms' parts.
    #[command(subcommand)]
    Sums(SumsVerb),
    /// Check a directory of assets against `SHA256SUMS`, or their attestations.
    #[command(subcommand)]
    VerifyAssets(VerifyAssets),
    /// A GitHub release of the exact files a push to main built and signed.
    #[command(subcommand)]
    Release(ReleaseVerb),
}

#[derive(Subcommand)]
enum SumsVerb {
    /// Merge the `SHA256SUMS.<platform>` files of `dir` into `dir/SHA256SUMS`.
    Assemble {
        dir: PathBuf,
        /// Refuse a result that is not byte for byte this file (the published sums).
        #[arg(long)]
        expect: Option<PathBuf>,
        /// Append the result to this file (the job summary).
        #[arg(long)]
        summary: Option<PathBuf>,
        /// Append `sha256=<hex>` of the sums to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        digest_output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum VerifyAssets {
    /// `dir/SHA256SUMS` lists every file of `dir`, and each matches.
    Sums { dir: PathBuf },
    /// Every listed file has the attestations a deployment checks.
    Attestations {
        dir: PathBuf,
        /// The repository, `<owner>/<repo>`.
        #[arg(long)]
        repo: String,
        /// The workflow that signed them, `<owner>/<repo>/<path>[@<ref>]`.
        #[arg(long)]
        workflow_ref: String,
        /// The branch they were signed on, `refs/heads/<branch>`.
        #[arg(long, default_value = "refs/heads/main")]
        source_ref: String,
        /// The commit the files were built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// A `trusted_root.jsonl` (`gh attestation trusted-root`) to verify the bundles with.
        #[arg(long)]
        trusted_root: Option<PathBuf>,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

#[derive(Subcommand)]
enum ReleaseVerb {
    /// The successful build run of a commit on main; refuses a commit whose tests failed.
    Locate {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        commit: String,
        /// Append `run-id=<id>` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
    /// Whether a push to main is released and as which tag; appends the answer to `--output`.
    Plan {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        commit: String,
        /// The `VERSION` file: a version above the highest release tag is the next tag.
        #[arg(long)]
        version_file: Option<PathBuf>,
        /// The value of the repository variable `RELEASE_FREEZE`; `true` stops the release.
        #[arg(long, default_value = "")]
        freeze: String,
        /// Append `release=`, `tag=`, `commit=` and `note=` (or `reason=`) to this file
        /// (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
    /// The same answer for a run started by hand, with its own tag and commit.
    Manual {
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        #[arg(long)]
        commit: String,
        /// A paragraph for the release notes.
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// `dir` holds exactly the assets of the commit, each tarball with its bundle.
    Check {
        dir: PathBuf,
        #[arg(long)]
        commit: String,
    },
    /// Create the release and publish it.
    Publish {
        dir: PathBuf,
        #[arg(long)]
        repo: String,
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        #[arg(long)]
        commit: String,
        /// The build run the files came from.
        #[arg(long)]
        run_id: u64,
        /// Where the notes are written (must be outside `dir`).
        #[arg(long)]
        notes_file: PathBuf,
        /// A paragraph added to the notes; repeat for more.
        #[arg(long)]
        note: Vec<String>,
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

/// The directory this crate is in: `crates/xtask`'s grandparent, the repository root.
fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// `git -C root args`: its stdout.
pub(crate) fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(Error::spawn("git".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(out.stdout)
}

/// The `cargo` program: the one that ran this xtask.
fn cargo_program() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn run(cli: Cli) -> Result<Vec<String>, Error> {
    let root = cli.root.unwrap_or_else(default_root);
    match cli.verb {
        Verb::BuildCommit { binary, commit } => Ok(vec![build_commit::check(&binary, &commit)?]),
        Verb::Advisories(args) => advisories::run(&args, &root, &cargo_program()),
        Verb::Fetch {
            url,
            sha256,
            out,
            extract,
            member,
        } => {
            let unpack = fetch::Unpack::new(extract, member).map_err(Error::Usage)?;
            let roots = fetch::host_roots();
            Ok(vec![fetch::fetch(&url, &sha256, &unpack, &out, &roots)?])
        }
        Verb::Package {
            binary,
            sbom,
            platform,
            commit,
            out,
        } => package::package(&binary, &sbom, platform, &commit, &out),
        Verb::Sums(SumsVerb::Assemble {
            dir,
            expect,
            summary,
            digest_output,
        }) => sums::assemble(
            &dir,
            expect.as_deref(),
            summary.as_deref(),
            digest_output.as_deref(),
        ),
        Verb::VerifyAssets(VerifyAssets::Sums { dir }) => {
            Ok(vec![sums::verify_sums(&dir, fetch::Sha256::of)?])
        }
        Verb::VerifyAssets(VerifyAssets::Attestations {
            dir,
            repo,
            workflow_ref,
            source_ref,
            commit,
            trusted_root,
            gh,
        }) => {
            let scratch = root.join("target").join("verify-assets-scratch");
            std::fs::create_dir_all(&scratch)
                .map_err(Error::spawn(format!("create {}", scratch.display())))?;
            let attestations = sums::Attestations {
                repo: &repo,
                workflow_ref: &workflow_ref,
                source_ref: &source_ref,
                commit: &commit,
                trusted_root: trusted_root.as_deref(),
                gh: &gh,
                scratch: &scratch,
            };
            sums::verify_attestations(&dir, &attestations)
        }
        Verb::Release(ReleaseVerb::Locate {
            repo,
            commit,
            output,
            gh,
        }) => release::locate(&release::RealGh(gh), &repo, &commit, output.as_deref()),
        Verb::Release(ReleaseVerb::Plan {
            repo,
            commit,
            version_file,
            freeze,
            output,
            gh,
        }) => {
            let version = version_file
                .as_deref()
                .map(release::read_version)
                .transpose()?
                .flatten();
            release::plan(
                &release::RealGh(gh),
                &release::Request {
                    repo: &repo,
                    commit: &commit,
                    version: version.as_deref(),
                    freeze: &freeze,
                    output: output.as_deref(),
                },
            )
        }
        Verb::Release(ReleaseVerb::Manual {
            tag,
            commit,
            note,
            output,
        }) => release::manual(&tag, &commit, &note, output.as_deref()),
        Verb::Release(ReleaseVerb::Check { dir, commit }) => release::check(&dir, &commit),
        Verb::Release(ReleaseVerb::Publish {
            dir,
            repo,
            tag,
            commit,
            run_id,
            notes_file,
            note,
            gh,
        }) => release::publish(
            &release::RealGh(gh),
            &dir,
            &repo,
            &tag,
            &commit,
            run_id,
            &notes_file,
            &note
                .into_iter()
                .filter(|n| !n.trim().is_empty())
                .collect::<Vec<_>>(),
        ),
    }
}

/// The entry point of the `xtask` binary.
pub fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask: {e}");
            match e {
                Error::Usage(_) => ExitCode::from(2),
                _ => ExitCode::FAILURE,
            }
        }
    }
}
