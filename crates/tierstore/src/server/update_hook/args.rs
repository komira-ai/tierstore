//! The command line of the hook: `tierstore update-hook <step> [<artifact>] --params <file>`.
//!
//! hostkeeper runs `<command> <step> --params <file>`, the command being the manifest's
//! `["bin/tierstore", "update-hook"]`. This module parses what follows the verb: the step, the
//! staged release directory (`prepare` only) and the params file, which every step takes: the
//! five steps of an update, `uninstall`, and `restore`, the boot step the manifest names
//! (`boot = "restore"`). Flags only: no value comes from the environment.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// One step of the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Check the host can take the update and record it; nothing a client sees changes.
    Prepare,
    /// The visible change: the new release serves.
    Cutover,
    /// Observe that the new release serves.
    Verify,
    /// Undo what `prepare` and `cutover` did, before the commit.
    Rollback,
    /// Retire what only the update needed; the commit.
    Cleanup,
    /// Stop the service for good; its data stays.
    Uninstall,
    /// The boot step: start the instance the record names, and only that one.
    Restore,
}

impl Step {
    /// Every step: the five of an update in the order of the contract's table, then `uninstall`
    /// and the boot step.
    pub const ALL: [Step; 7] = [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
        Step::Uninstall,
        Step::Restore,
    ];

    /// The word hostkeeper puts on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Step::Prepare => "prepare",
            Step::Cutover => "cutover",
            Step::Verify => "verify",
            Step::Rollback => "rollback",
            Step::Cleanup => "cleanup",
            Step::Uninstall => "uninstall",
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
    /// No `--params`.
    #[error("--params <file> is required")]
    MissingParams,
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// The step.
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The params file hostkeeper wrote for this run.
    pub params: PathBuf,
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
        let step = args
            .next()
            .ok_or(ArgsError::MissingStep)?
            .to_string_lossy()
            .parse::<Step>()?;
        let (mut artifact, mut params) = (None, None);
        while let Some(arg) = args.next() {
            if arg == "--params" {
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
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        Ok(Invocation {
            step,
            artifact,
            params: params.ok_or(ArgsError::MissingParams)?,
        })
    }
}
