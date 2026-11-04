//! The verbs of `tierstore` that are not a start: each is answered before the server's flags are
//! parsed, as a short-lived process of an operator or of the host updater.
//!
//! ```text
//! tierstore update-hook <prepare|cutover|verify|rollback|cleanup> [<artifact>] --params <file>
//! tierstore format-epoch (--check|--drain|--cancel|--activate) --hot <tier> [flags]
//! tierstore entries (ls|show --key <key>) --hot <tier> [flags]
//! ```
//!
//! `update-hook` is [`update_hook`]. `format-epoch` ([`format_epoch`]) and `entries`
//! ([`entries`]) are the operator's: they read and write only tierstore's own objects in the hot
//! tier (`_tiering/format`, `_tiering/proxies/`, `_entries/`) and never ask a store how it behaves.
//! Both take the hot tier by the flags of the server that name it (`--hot`, `--hot-endpoint`,
//! `--hot-region`, `--hot-credentials-file`, `--s3-request-timeout`). The verbs that write are held:
//! without `--apply` they check and print what they would write, and write nothing.

mod entries;
mod format_epoch;
mod registrations;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser};

use super::cli::{S3Flags, open_tier, parse_duration};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::update_hook;
use crate::{Backend, SystemClock, TokioSleeper};

/// The name of the verb that pauses and resumes writes for a change of the stored format.
pub(super) const FORMAT_EPOCH: &str = "format-epoch";
/// The name of the verb that lists and shows the journal entries of keys.
pub(super) const ENTRIES: &str = "entries";

/// Why a verb did not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub(super) enum VerbError {
    /// A check the verb makes of what it found refused the request; nothing was written.
    #[error("{0}")]
    Refused(String),
    /// The hot tier failed a call.
    #[error("the hot tier: {0}")]
    Store(#[from] crate::Error),
    /// The output could not be written.
    #[error("writing the output: {0}")]
    Output(#[from] std::io::Error),
    /// The runtime of the process could not be made.
    #[error("starting the runtime: {0}")]
    Runtime(std::io::Error),
}

/// The flags that name the hot tier, the same as the server's.
#[derive(Args, Debug)]
pub(super) struct Hot {
    /// The hot tier: `memory`, `dir:<path>` or `s3:<bucket>`.
    #[arg(long)]
    hot: String,
    /// The endpoint of the hot tier when it is `s3:<bucket>`.
    #[arg(long)]
    hot_endpoint: Option<String>,
    /// The signing region of the hot tier when it is `s3:<bucket>`.
    #[arg(long)]
    hot_region: Option<String>,
    /// The file that holds the access key of the hot tier when it is `s3:<bucket>`.
    #[arg(long)]
    hot_credentials_file: Option<PathBuf>,
    /// The limit on a request to an `s3:<bucket>` tier: a number and `ms`, `s`, `m` or `h`.
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    s3_request_timeout: Duration,
}

impl Hot {
    /// The hot tier the flags name.
    fn open(&self) -> Result<Arc<dyn Backend>, VerbError> {
        let s3 = S3Flags {
            endpoint: self.hot_endpoint.as_deref(),
            region: self.hot_region.as_deref(),
            credentials: self.hot_credentials_file.as_deref(),
            conditional_delete: None,
        };
        open_tier("--hot", &self.hot, &s3, self.s3_request_timeout).map_err(VerbError::Refused)
    }
}

/// The arguments of the verb `verb`: the program name and the verb are skipped, and the usage
/// names both. A usage error, or a
/// request for help, is printed (to `err` for the first and `out` for the second) and its exit
/// status is the `Err`.
fn parsed<A: Parser>(
    verb: &str,
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<A, ExitCode> {
    let program = OsString::from(format!("tierstore {verb}"));
    let flags = std::iter::once(program).chain(args.iter().skip(2).cloned());
    A::try_parse_from(flags).map_err(|usage| {
        let text = usage.to_string();
        if usage.use_stderr() {
            let _ = err.write_all(text.as_bytes());
            ExitCode::from(2)
        } else {
            let _ = out.write_all(text.as_bytes());
            ExitCode::SUCCESS
        }
    })
}

/// Runs `work` to its end on a runtime of its own.
fn block_on(work: impl Future<Output = Result<(), VerbError>>) -> Result<(), VerbError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(VerbError::Runtime)?;
    runtime.block_on(work)
}

/// The exit status of a verb whose result is `result`: 0, or 1 with the reason on `err`.
fn finished(verb: &str, result: Result<(), VerbError>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore {verb}: {why}");
            ExitCode::from(1)
        }
    }
}

/// The exit status of the verb that `args` (the program name first) name, or `None` if the first
/// argument is not a verb: the caller then parses the flags of a start.
pub(super) fn answer(args: &[OsString]) -> Option<ExitCode> {
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    match args.get(1).and_then(|verb| verb.to_str()) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Some(update_hook::VERB) => Some(update_hook::main(args)),
        Some(FORMAT_EPOCH) => Some(format_epoch::run(
            args,
            &mut out,
            &mut err,
            &SystemClock,
            &TokioSleeper,
        )),
        Some(ENTRIES) => Some(entries::run(args, &mut out, &mut err, &SystemClock)),
        _ => None,
    }
}

#[cfg(test)]
mod testing;
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
