//! The command line of the hook: `tierstore update-hook <step> [<artifact>] [--params <file>]`.
//!
//! The updater runs `<executable> <hook_args> <step> [args]`. This module parses what follows
//! the verb: the step, the staged release directory (`prepare` only) and the file of the
//! `[hook.params]` table, which the five steps need and `restore` (the boot entry, run before
//! any update) does not take. Flags only: no value comes from the environment.
//!
//! Four more flags say where the host is ([`HostArgs`]): `--root`, `--root-uid`, `--nft` and
//! `--systemctl`. A host gives none of them and gets the defaults; a test gives a scratch root
//! and fake programs. They may come before or after the step, as `hook_args` or after it.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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
    /// `--root`, `--nft` or `--systemctl` is not an absolute path of plain components.
    #[error("{0} must be an absolute path of plain components")]
    NotPlain(&'static str),
    /// `--root-uid` is not a user id.
    #[error("--root-uid must be a user id, a whole number")]
    BadUid,
}

/// The root the host's files are read under.
pub const ROOT_FLAG: &str = "--root";
/// The user that must own the declaration and a staged release's binary.
pub const ROOT_UID_FLAG: &str = "--root-uid";
/// The `nft` program.
pub const NFT_FLAG: &str = "--nft";
/// The `systemctl` program.
pub const SYSTEMCTL_FLAG: &str = "--systemctl";
const HOST_FLAGS: [&str; 4] = [ROOT_FLAG, ROOT_UID_FLAG, NFT_FLAG, SYSTEMCTL_FLAG];
/// The user that must own the declaration and a staged release's binary unless `--root-uid`
/// says otherwise: root, since both are the updater's.
pub const ROOT_UID: u32 = 0;

/// Where the hook finds the host it acts on. A host gives none of the flags: the root is `/`, the
/// declaration and a staged release's binary must be root's (uid 0), and the programs are
/// `/usr/sbin/nft` and `/usr/bin/systemctl`. A conformance test gives a scratch root, its own
/// user id and fake programs, and runs the real binary there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostArgs {
    /// `--root`: the root the declaration, `etc/group` and `proc` are read under.
    pub root: PathBuf,
    /// `--root-uid`: the user that must own the declaration and the binary of a staged release.
    pub root_uid: u32,
    /// `--nft`: the `nft` program.
    pub nft: PathBuf,
    /// `--systemctl`: the `systemctl` program.
    pub systemctl: PathBuf,
}

impl Default for HostArgs {
    fn default() -> HostArgs {
        HostArgs {
            root: PathBuf::from("/"),
            root_uid: ROOT_UID,
            nft: PathBuf::from(NFT_PROGRAM),
            systemctl: PathBuf::from(SYSTEMCTL_PROGRAM),
        }
    }
}

impl HostArgs {
    /// Sets the value of `flag`, one of [`HOST_FLAGS`].
    fn set(&mut self, flag: &'static str, value: OsString) -> Result<(), ArgsError> {
        if flag == ROOT_UID_FLAG {
            self.root_uid = value
                .to_str()
                .and_then(|text| text.parse().ok())
                .ok_or(ArgsError::BadUid)?;
            return Ok(());
        }
        let path = PathBuf::from(value);
        if !is_plain(&path) {
            return Err(ArgsError::NotPlain(flag));
        }
        match flag {
            ROOT_FLAG => self.root = path,
            NFT_FLAG => self.nft = path,
            _ => self.systemctl = path,
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
