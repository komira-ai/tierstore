//! The command line of the hook: `tierstore update-hook <step> [<artifact>] [--params <file>]`.
//!
//! The updater runs `<executable> <hook_args> <step> [args]`. This module parses what follows
//! the verb: the step, the staged release directory (`prepare` only) and the file of the
//! `[hook.params]` table, which the five steps need and `restore` (the boot entry, run before
//! any update) does not take. Flags only: no value comes from the environment.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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
        let first = args.next().ok_or(ArgsError::MissingStep)?;
        let step: Step = first.to_string_lossy().parse()?;
        let (mut artifact, mut params) = (None, None);
        while let Some(arg) = args.next() {
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
        })
    }
}
