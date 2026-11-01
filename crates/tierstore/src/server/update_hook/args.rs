//! The command line of the hook: `tierstore update-hook <step> [<artifact>] [--params <file>]`.
//!
//! The updater runs `<executable> <hook_args> <step> [args]`. This module parses what follows
//! the verb: the step, the staged release directory (`prepare` only) and the file of the
//! `[hook.params]` table, which the five steps need and `restore` (the boot entry, run before
//! any update) does not take. Flags only: no value comes from the environment.
//!
//! A build with the `hook-conformance` feature, which only this crate's own tests turn on, takes
//! six more flags that say where the host is ([`HostArgs`]): `--root`, `--root-uid`, `--nft`,
//! `--systemctl`, `--public-port` and `--poll-interval-ms`. They let a test run the real binary
//! against a scratch root, fake programs and its own ports. A release has none of them: there the
//! host is always `/`, root's, `/usr/sbin/nft`, `/usr/bin/systemctl`, port 9300 and a poll a
//! second, and the words are refused like any other unexpected argument.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use super::declaration::PUBLIC_PORT;
use super::machine::{NFT_PROGRAM, SYSTEMCTL_PROGRAM};
use super::plain::is_plain;

/// One of the five steps of the contract, or the boot entry `restore`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Check the host can take the update and make it ready; nothing a client sees changes.
    Prepare,
    /// The visible change.
    Cutover,
    /// Observe that the new version serves.
    Verify,
    /// Undo what `prepare` and `cutover` did, as far as they can be undone.
    Rollback,
    /// Remove what only the update needed.
    Cleanup,
    /// The boot entry: apply the rules and start the slot the record names.
    Restore,
}

impl Step {
    /// Every step, in the order of the contract's table, then the boot entry.
    pub const ALL: [Step; 6] = [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
        Step::Restore,
    ];

    /// The word the updater puts on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Step::Prepare => "prepare",
            Step::Cutover => "cutover",
            Step::Verify => "verify",
            Step::Rollback => "rollback",
            Step::Cleanup => "cleanup",
            Step::Restore => "restore",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Step {
    type Err = ArgsError;

    fn from_str(word: &str) -> Result<Step, ArgsError> {
        Step::ALL
            .into_iter()
            .find(|step| step.name() == word)
            .ok_or_else(|| ArgsError::UnknownStep(word.to_owned()))
    }
}

/// Why a command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    /// Nothing follows the verb.
    #[error("no step given")]
    MissingStep,
    /// The first word is not one of the steps.
    #[error("unknown step {0:?}")]
    UnknownStep(String),
    /// `prepare` without the staged release directory.
    #[error("prepare needs the staged release directory")]
    MissingArtifact,
    /// An argument the step does not take.
    #[error("unexpected argument {0:?}")]
    Unexpected(OsString),
    /// `--params` is the last word.
    #[error("--params needs a file")]
    MissingParamsFile,
    /// `--params` twice.
    #[error("--params given twice")]
    DuplicateParams,
    /// A step other than `restore` without `--params`.
    #[error("--params <file> is required")]
    MissingParams,
    /// A flag of the host is the last word.
    #[error("{0} needs a value")]
    MissingValue(&'static str),
    /// A flag of the host given twice.
    #[error("{0} given twice")]
    DuplicateFlag(&'static str),
    /// `--root`, `--nft` or `--systemctl` is not an absolute UTF-8 path of plain components.
    #[error("{0} must be an absolute UTF-8 path of plain components")]
    NotPlain(&'static str),
    /// `--root-uid`, `--public-port` or `--poll-interval-ms` is not a number in its range.
    #[error("{0} must be a whole number in its range")]
    NotNumber(&'static str),
}

/// The root the host's files are read under.
pub const ROOT_FLAG: &str = "--root";
/// The user that must own the declaration and a staged release's binary.
pub const ROOT_UID_FLAG: &str = "--root-uid";
/// The `nft` program.
pub const NFT_FLAG: &str = "--nft";
/// The `systemctl` program. A release reads it in no list: it takes none of these flags.
#[cfg_attr(not(feature = "hook-conformance"), allow(dead_code))]
pub const SYSTEMCTL_FLAG: &str = "--systemctl";
/// The public port.
pub const PUBLIC_PORT_FLAG: &str = "--public-port";
/// The time between two looks at an instance, in milliseconds.
pub const POLL_FLAG: &str = "--poll-interval-ms";
/// The flags of the host this build takes: all six in a conformance build, none in a release.
#[cfg(feature = "hook-conformance")]
const HOST_FLAGS: [&str; 6] = [
    ROOT_FLAG,
    ROOT_UID_FLAG,
    NFT_FLAG,
    SYSTEMCTL_FLAG,
    PUBLIC_PORT_FLAG,
    POLL_FLAG,
];
#[cfg(not(feature = "hook-conformance"))]
const HOST_FLAGS: [&str; 0] = [];
/// The user that must own the declaration and a staged release's binary unless `--root-uid`
/// says otherwise: root, since both are the updater's.
pub const ROOT_UID: u32 = 0;

/// Where the hook finds the host it acts on. A release always has the defaults: the root is `/`,
/// the declaration and a staged release's binary must be root's (uid 0), the programs are
/// `/usr/sbin/nft` and `/usr/bin/systemctl`, the public port is 9300 and a poll waits a second.
/// A conformance build lets a test set each, to run the real binary against a scratch host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostArgs {
    /// `--root`: the root the declaration, `etc/group`, `etc/passwd`, `proc` and the state
    /// directory are read under.
    pub root: PathBuf,
    /// `--root-uid`: the user that must own the declaration and the binary of a staged release.
    pub root_uid: u32,
    /// `--nft`: the `nft` program.
    pub nft: PathBuf,
    /// `--systemctl`: the `systemctl` program.
    pub systemctl: PathBuf,
    /// `--public-port`: the port clients call, which the redirect sends to the serving slot.
    pub public_port: u16,
    /// `--poll-interval-ms`: what a step waits between two looks at an instance; `None` is the
    /// steps' own interval, a second.
    pub poll: Option<Duration>,
}

impl Default for HostArgs {
    fn default() -> HostArgs {
        HostArgs {
            root: PathBuf::from("/"),
            root_uid: ROOT_UID,
            nft: PathBuf::from(NFT_PROGRAM),
            systemctl: PathBuf::from(SYSTEMCTL_PROGRAM),
            public_port: PUBLIC_PORT,
            poll: None,
        }
    }
}

/// `value` as a number of type `T`; `NotNumber(flag)` when it is not one, or out of range.
fn number<T: FromStr>(flag: &'static str, value: &OsString) -> Result<T, ArgsError> {
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or(ArgsError::NotNumber(flag))
}

impl HostArgs {
    /// Sets the value of `flag`, one of [`HOST_FLAGS`].
    fn set(&mut self, flag: &'static str, value: OsString) -> Result<(), ArgsError> {
        match flag {
            ROOT_UID_FLAG => self.root_uid = number(flag, &value)?,
            PUBLIC_PORT_FLAG => {
                self.public_port = number(flag, &value)?;
                if self.public_port == 0 {
                    return Err(ArgsError::NotNumber(flag));
                }
            }
            POLL_FLAG => self.poll = Some(Duration::from_millis(number(flag, &value)?)),
            _ => {
                // A path that is not UTF-8 is refused rather than spelt over: the programs are
                // run by a name that is text.
                let Some(text) = value.to_str() else {
                    return Err(ArgsError::NotPlain(flag));
                };
                let path = PathBuf::from(text);
                if !is_plain(&path) {
                    return Err(ArgsError::NotPlain(flag));
                }
                match flag {
                    ROOT_FLAG => self.root = path,
                    NFT_FLAG => self.nft = path,
                    _ => self.systemctl = path,
                }
            }
        }
        Ok(())
    }
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The step.
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The file of the `[hook.params]` table: given to the five steps, not to `restore`.
    pub params: Option<PathBuf>,
    /// Where the host is: the defaults unless a flag says otherwise.
    pub host: HostArgs,
}

impl Invocation {
    /// Parses the arguments that follow the verb, the step first.
    ///
    /// # Errors
    /// [`ArgsError`] for an unknown step, a missing or extra argument, or a missing `--params`.
    pub fn parse<I>(args: I) -> Result<Invocation, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args.into_iter();
        let (mut step, mut artifact, mut params) = (None, None, None);
        let (mut host, mut given) = (HostArgs::default(), Vec::new());
        while let Some(arg) = args.next() {
            if let Some(flag) = HOST_FLAGS.into_iter().find(|flag| arg == *flag) {
                let value = args.next().ok_or(ArgsError::MissingValue(flag))?;
                if given.contains(&flag) {
                    return Err(ArgsError::DuplicateFlag(flag));
                }
                given.push(flag);
                host.set(flag, value)?;
                continue;
            }
            let Some(step) = step else {
                step = Some(arg.to_string_lossy().parse::<Step>()?);
                continue;
            };
            if arg == "--params" && step != Step::Restore {
                let file = args.next().ok_or(ArgsError::MissingParamsFile)?;
                if params.replace(PathBuf::from(file)).is_some() {
                    return Err(ArgsError::DuplicateParams);
                }
            } else if step == Step::Prepare && artifact.is_none() {
                artifact = Some(PathBuf::from(arg));
            } else {
                return Err(ArgsError::Unexpected(arg));
            }
        }
        let step = step.ok_or(ArgsError::MissingStep)?;
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        if step != Step::Restore && params.is_none() {
            return Err(ArgsError::MissingParams);
        }
        Ok(Invocation {
            step,
            artifact,
            params,
            host,
        })
    }
}
