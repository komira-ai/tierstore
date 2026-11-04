//! From the command line to the settings: the file `--config` names, and the knobs of the run.
//!
//! The only flags are those of [`Args`]. A flag that used to describe the data path is refused
//! with the key of the file that replaces it (see [`removed`]), before the parser sees it, so
//! that the message says what to do and not that the argument is unexpected.

use std::ffi::OsString;

use clap::Parser;

use super::cli::Args;
use super::config::{ConfigError, PerRun, Settings, load, removed};

/// What the command line made.
#[derive(Debug)]
pub(super) struct Launch {
    /// The settings of the file, of any of the three shapes.
    pub settings: Settings,
    /// Whether `--check-config` was given: the settings are checked and nothing is started.
    pub check_only: bool,
}

/// Why there are no settings.
#[derive(Debug)]
pub(super) enum Refused {
    /// The parser refused the flags, or was asked for the usage or the version.
    Usage(clap::Error),
    /// The file was refused, or there is none, or a flag is one that went.
    Config(ConfigError),
}

/// The refusal of the first argument of `args` (the program name first) that is a flag the data
/// path used to have, whether it is written `--flag value` or `--flag=value`.
fn removed_flag(args: &[OsString]) -> Option<String> {
    args.iter()
        .skip(1)
        .filter_map(|arg| arg.to_str())
        .filter_map(|arg| arg.strip_prefix("--"))
        .map(|arg| arg.split_once('=').map_or(arg, |(name, _)| name))
        .find_map(removed)
}

/// The settings that `args` (the program name first) stand for.
///
/// # Errors
/// [`Refused::Usage`] for flags the parser refuses, and [`Refused::Config`] for a flag that went,
/// a start without `--config`, or a file that is refused.
pub(super) fn resolve(args: Vec<OsString>) -> Result<Launch, Refused> {
    if let Some(why) = removed_flag(&args) {
        return Err(Refused::Config(ConfigError::Removed(why)));
    }
    let args = Args::try_parse_from(args).map_err(Refused::Usage)?;
    let Some(path) = args.config else {
        let why = if args.check_config {
            ConfigError::CheckWithoutConfig
        } else {
            ConfigError::NoConfig
        };
        return Err(Refused::Config(why));
    };
    let per_run = PerRun {
        drain_lb_wait: args.drain_lb_wait,
        drain_deadline: args.drain_deadline,
    };
    let settings = load(&path, per_run).map_err(Refused::Config)?;
    Ok(Launch {
        settings,
        check_only: args.check_config,
    })
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
