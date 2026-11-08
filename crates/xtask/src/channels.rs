//! `cargo xtask channels`: a release reaching the public channels after the GitHub release exists.
//! `.github/workflows/release.yml` runs these verbs in its `testing-upload`, `promote-latest` and `tap` jobs and the install checks;
//! `docs/design/distribution.md` is the design.
//!
//! - `channels gate --label <job> --setting NAME=VALUE... [--output <file>]` ([`gate`]): whether
//!   a job has the settings it needs. None set: `enabled=false` and a notice, the job ends green
//!   and writes nothing. All set: `enabled=true`. Some set: red, naming the missing ones.
//! - `channels upload <dir> --tag <tag> --commit <sha> --bucket <name> --work <dir>` ([`upload`]):
//!   puts the release files (the tap-named tarballs, the Debian packages, their bundles and a
//!   `SHA256SUMS` of exactly those) to `releases/<version>/` of the site bucket once each, then
//!   (unless `--no-latest`) replaces `releases/latest.json`, never with an older version, and
//!   invalidates it.
//! - `channels latest <dir> --tag <tag> ...` ([`upload::latest`]): the pointer alone. It reads the
//!   files of `releases/<version>/` back, holds them to the release's `SHA256SUMS`, and then
//!   replaces `releases/latest.json`.
//! - `channels decide ...` ([`decide`]): whether the release is promoted from testing to the
//!   public channels, from the results of the jobs before it.
//! - `channels check-apt|check-brew|check-install-sh ...` ([`check`]): the release installed from
//!   the testing suite, the rendered formula and `install.sh`, as a user does.
//! - `channels apt read|index|sign|write ...` ([`apt`]): the Debian packages into the apt
//!   repository the organisation's products share, in the three jobs of `release.yml`.
//! - `channels soak --hours <n> --reached-at <secs> --version <X.Y.Z+hex> ...` ([`soak`]): the
//!   farm stays healthy on the release for N hours before the public channels follow.
//! - `channels formula --sums <file> --version <X.Y.Z> --repo <owner/name> --out <file>`
//!   ([`formula`]): `Formula/tierstore.rb` as the Homebrew tap's guard takes it, from the
//!   release's `SHA256SUMS` alone.
//! - `channels tap --version <X.Y.Z> --formula-file <file> --repo <owner/name> --tap-repo
//!   <owner/name>` ([`tap`]): the pull request `bump/tierstore-v<X.Y.Z>` in the tap.
//!
//! - `channels assume --role-arn <arn> --session-name <name> --region <r> --env-file <file>`
//!   ([`assume`]): the job's AWS credentials from its OIDC token, into the env file; the step
//!   `release.yml` runs where an action would otherwise do it.
//!
//! The programs that talk to the outside (`aws`, `gh`) are flags, so a test hands a stand-in
//! ([`aws::Aws`], [`crate::release::Gh`]).

pub mod apt;
pub mod assume;
pub mod aws;
pub mod check;
pub mod decide;
pub mod formula;
pub mod gate;
pub mod soak;
pub mod tap;
pub mod upload;

use std::path::PathBuf;

use clap::Subcommand;

use crate::Error;
use crate::release::hostkeeper_package::is_plain_version;

/// `<major>.<minor>.<patch>` as numbers; none for anything else, or a number too large.
pub(crate) fn numbers(version: &str) -> Option<(u64, u64, u64)> {
    if !is_plain_version(version) {
        return None;
    }
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// The `channels` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// Whether a channel job has its settings: none set is a green no-op, some set is red.
    Gate {
        /// The job, as the notice names it.
        #[arg(long)]
        label: String,
        /// One setting, `NAME=VALUE`; only whether VALUE is empty is read. The workflow passes
        /// `NAME=${VAR:+set}`, so a secret never reaches a command line.
        #[arg(long = "setting", required = true)]
        settings: Vec<String>,
        /// What an earlier job must have said, `NAME=VALUE` with VALUE its output: unless every
        /// one is `true` the job is off and green, and its settings are not judged.
        #[arg(long = "require")]
        requires: Vec<String>,
        /// Append `enabled=true` or `enabled=false` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The farm stays healthy on a release for N hours before the public channels follow.
    Soak {
        /// How many hours to soak, 0 to 5; 0 ends at once.
        #[arg(long)]
        hours: u32,
        /// Seconds since the epoch at which the farm was read converged on the version.
        #[arg(long)]
        reached_at: u64,
        /// The version the farm must stay on, as the coordinator names it, `X.Y.Z+<12 hex>`.
        #[arg(long)]
        version: String,
        /// Start a clock at this many seconds since the epoch that only the soak's own waits
        /// move, instead of the machine's: for a test, which then waits for nothing.
        #[arg(long)]
        now: Option<u64>,
        /// Seconds between two reads.
        #[arg(long, default_value_t = soak::INTERVAL_SECS)]
        interval_secs: u64,
        /// How many reads in a row may get no answer.
        #[arg(long, default_value_t = soak::MAX_SILENT_READS)]
        max_silent_reads: u32,
        /// The program that reads the convergence and prints the JSON of `hostkeeper ctl releases
        /// converge`. Needed for a soak of more than 0 hours.
        #[arg(long)]
        read_command: Option<PathBuf>,
        /// One argument of the reading program; repeat for more.
        #[arg(long = "read-arg", allow_hyphen_values = true)]
        read_args: Vec<String>,
    },
    /// The job's AWS credentials from its OIDC token, appended to the env file (`GITHUB_ENV`).
    Assume {
        /// The role to assume.
        #[arg(long)]
        role_arn: String,
        /// The session name AWS records.
        #[arg(long)]
        session_name: String,
        /// The region the later steps use.
        #[arg(long)]
        region: String,
        /// Where the `AWS_*` lines are appended.
        #[arg(long)]
        env_file: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// The release files to `releases/<version>/` of the site bucket, then `releases/latest.json`.
    Upload {
        /// The release's files (the `release-assets` artifact).
        dir: PathBuf,
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The site bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate `/releases/latest.json` in; empty: none.
        #[arg(long, default_value = "")]
        distribution_id: String,
        /// Where `SHA256SUMS`, `latest.json` and what is read back are staged; created when
        /// missing.
        #[arg(long)]
        work: PathBuf,
        /// How many times `latest.json` is read and decided again after a concurrent write.
        #[arg(long, default_value_t = upload::ATTEMPTS)]
        attempts: u32,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
        /// Do not replace `latest.json`: the release is published under its version only.
        #[arg(long)]
        no_latest: bool,
    },
    /// `releases/latest.json` replaced by this release, after its files are read back from the
    /// bucket and held to `SHA256SUMS`.
    Latest {
        /// The release's files (the `release-assets` artifact).
        dir: PathBuf,
        /// `v<major>.<minor>.<patch>`.
        #[arg(long)]
        tag: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The site bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate `/releases/latest.json` in; empty: none.
        #[arg(long, default_value = "")]
        distribution_id: String,
        /// Where `SHA256SUMS`, `latest.json` and what is read back are staged; created when
        /// missing.
        #[arg(long)]
        work: PathBuf,
        /// How many times `latest.json` is read and decided again after a concurrent write.
        #[arg(long, default_value_t = upload::ATTEMPTS)]
        attempts: u32,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// Whether the release in testing goes on to the public channels.
    Decide {
        /// The result of one stage, `NAME=RESULT` (`needs.<job>.result`); every stage is given.
        #[arg(long = "result", required = true)]
        results: Vec<String>,
        /// The output `enabled` of `farm-check`: `true` when the lab is configured.
        #[arg(long, default_value = "")]
        farm_enabled: String,
        /// Append `promote=true` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The release installed from the suite `testing` in a clean Ubuntu system.
    CheckApt {
        /// The Debian architecture of this machine, `amd64` or `arm64`.
        #[arg(long)]
        arch: String,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The apt repository.
        #[arg(long, default_value = "https://packages.komira.ai/apt")]
        apt_url: String,
        /// The system to check; `/` in the container.
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// The `apt-get` program.
        #[arg(long, default_value = "apt-get")]
        apt_get: PathBuf,
        /// The `dpkg-query` program.
        #[arg(long, default_value = "dpkg-query")]
        dpkg_query: PathBuf,
        /// The `dpkg` program.
        #[arg(long, default_value = "dpkg")]
        dpkg: PathBuf,
        /// The `curl` program.
        #[arg(long, default_value = "curl")]
        curl: PathBuf,
    },
    /// The rendered formula installed from a local tap.
    CheckBrew {
        /// The formula `channels formula` rendered.
        #[arg(long)]
        formula: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The local tap to make, `<owner>/<name>`.
        #[arg(long, default_value = "local/tierstore")]
        tap: String,
        /// The `brew` program.
        #[arg(long, default_value = "brew")]
        brew: PathBuf,
    },
    /// The version installed with `install.sh` from the site.
    CheckInstallSh {
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The commit the release was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// The site.
        #[arg(long, default_value = "https://tierstore.dev")]
        url: String,
        /// A scratch directory: the script is saved in it and installs into `bin` below it.
        #[arg(long)]
        work: PathBuf,
        /// The `curl` program.
        #[arg(long, default_value = "curl")]
        curl: PathBuf,
        /// The `sh` program.
        #[arg(long, default_value = "sh")]
        sh: PathBuf,
    },
    /// The apt repository: read the index, merge the packages, sign it, write it.
    Apt {
        #[command(subcommand)]
        verb: apt::Verb,
    },
    /// `Formula/tierstore.rb` from the release's `SHA256SUMS`.
    Formula {
        /// The release's `SHA256SUMS`.
        #[arg(long)]
        sums: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The released repository, `<owner>/<name>`; the formula's urls point into its releases.
        #[arg(long)]
        repo: String,
        /// The formula to write; it may not exist.
        #[arg(long)]
        out: PathBuf,
    },
    /// The pull request that puts the formula of a release in the tap, as the release bot.
    Tap {
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The formula `channels formula` wrote.
        #[arg(long)]
        formula_file: PathBuf,
        /// The released repository, `<owner>/<name>`.
        #[arg(long)]
        repo: String,
        /// The tap, `<owner>/<name>`, of the same owner.
        #[arg(long)]
        tap_repo: String,
        /// The tap's default branch.
        #[arg(long, default_value = "main")]
        base: String,
        /// The `gh` program; the bot's token is in its environment, as `gh` reads it.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Gate {
            label,
            settings,
            requires,
            output,
        } => gate::gate(&label, &settings, &requires, output.as_deref()),
        Verb::Soak {
            hours,
            reached_at,
            version,
            now,
            interval_secs,
            max_silent_reads,
            read_command,
            read_args,
        } => {
            let request = soak::Request {
                hours,
                reached_at,
                version: &version,
                interval_secs,
                max_silent_reads,
            };
            let reader = read_command.map(|program| soak::RealReader {
                program,
                args: read_args,
            });
            let reader = reader.as_ref().map(|r| r as &dyn soak::Reader);
            match now {
                Some(start) => soak::soak(&request, &mut soak::FixedClock(start), reader),
                None => soak::soak(&request, &mut soak::SystemClock, reader),
            }
        }
        Verb::Assume {
            role_arn,
            session_name,
            region,
            env_file,
            aws,
        } => assume::assume(
            &aws::RealAws(aws),
            &mut std::io::stdout().lock(),
            &assume::oidc_from_env,
            &assume::Request {
                role_arn: &role_arn,
                session_name: &session_name,
                region: &region,
                env_file: &env_file,
                https_only: true,
            },
        ),
        Verb::Upload {
            dir,
            tag,
            commit,
            bucket,
            distribution_id,
            work,
            attempts,
            aws,
            no_latest,
        } => upload::upload(
            &aws::RealAws(aws),
            &upload::Request {
                dir: &dir,
                tag: &tag,
                commit: &commit,
                bucket: &bucket,
                distribution_id: &distribution_id,
                work: &work,
                attempts,
                latest: !no_latest,
            },
        ),
        Verb::Latest {
            dir,
            tag,
            commit,
            bucket,
            distribution_id,
            work,
            attempts,
            aws,
        } => upload::latest(
            &aws::RealAws(aws),
            &upload::Request {
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
        Verb::Decide {
            results,
            farm_enabled,
            output,
        } => decide::decide(&results, &farm_enabled, output.as_deref()),
        Verb::CheckApt {
            arch,
            version,
            commit,
            apt_url,
            root,
            apt_get,
            dpkg_query,
            dpkg,
            curl,
        } => check::check_apt(
            &check::RealHost,
            &check::Apt {
                root: &root,
                arch: &arch,
                version: &version,
                commit: &commit,
                apt_url: &apt_url,
                apt_get: &apt_get,
                dpkg_query: &dpkg_query,
                dpkg: &dpkg,
                curl: &curl,
            },
        ),
        Verb::CheckBrew {
            formula,
            version,
            commit,
            tap,
            brew,
        } => check::check_brew(
            &check::RealHost,
            &check::Brew {
                formula: &formula,
                version: &version,
                commit: &commit,
                tap: &tap,
                brew: &brew,
            },
        ),
        Verb::CheckInstallSh {
            version,
            commit,
            url,
            work,
            curl,
            sh,
        } => check::check_install_sh(
            &check::RealHost,
            &check::InstallSh {
                version: &version,
                commit: &commit,
                url: &url,
                work: &work,
                curl: &curl,
                sh: &sh,
            },
        ),
        Verb::Apt { verb } => apt::run(verb),
        Verb::Formula {
            sums,
            version,
            repo,
            out,
        } => formula::formula(&sums, &version, &repo, &out),
        Verb::Tap {
            version,
            formula_file,
            repo,
            tap_repo,
            base,
            gh,
        } => tap::tap(
            &crate::release::RealGh(gh),
            &tap::Request {
                version: &version,
                formula_file: &formula_file,
                repo: &repo,
                tap_repo: &tap_repo,
                base: &base,
            },
        ),
    }
}
