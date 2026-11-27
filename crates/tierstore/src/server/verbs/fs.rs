//! The verb `tierstore fs`: the operator's list of what is attached, and the check of an `[fs]`
//! file. It serves nothing: the file system is the `fs` interface of the node's file, served by the
//! node's own process, so a start of the verb (a unit of the two-process layout left behind) exits
//! 2 and says so ([`NOT_A_PROCESS`]), and its `RestartPreventExitStatus=2` stops it from looping.
//!
//! ```text
//! tierstore fs --config <file>                exit 2: the file system is the fs interface of the node's file
//! tierstore fs --config <file> --check-config load the file and say nothing: status 0 if it loads, 2 and the reason if not
//! tierstore fs attachments --config <file>    list the attachments: `<tree digest> <run>`
//! ```
//!
//! `--check-config` serves nothing, mounts nothing and touches no socket; names in `fs.owner` and
//! `fs.client_group` are looked up through the system. It does not go with `attachments`: that is
//! status 2.
//!
//! The file has an `[fs]` table (see the `treefs` module); it is not the file of the S3 instance.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use super::parsed;
use crate::server::treefs::{FsSettings, load};

/// What a start of the verb says before it exits 2.
pub(super) const NOT_A_PROCESS: &str = "tierstore fs is no longer a process of its own: the file \
     system is the [[interface]] with type = \"fs\" of the node's file";

/// The arguments of the verb.
#[derive(Debug, Parser)]
#[command(
    about = "The attachments of the virtual build file system, and the check of an [fs] file; it serves nothing"
)]
struct Args {
    /// The configuration file, which has an `[fs]` table (needed).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Load the file as a start does, write nothing and serve nothing: status 0 if it loads, 2 and
    /// the reason if not.
    #[arg(long)]
    check_config: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the verb does when it is given a subcommand.
#[derive(Debug, Subcommand)]
enum Command {
    /// List what is attached, one line each: the digest of the tree and the run.
    Attachments,
}

/// The answer of the control socket to `attachments`.
fn control_answer(socket: &Path) -> std::io::Result<serde_json::Value> {
    use std::io::{BufRead as _, BufReader};
    let stream = UnixStream::connect(socket)?;
    (&stream).write_all(b"attachments\n")?;
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Writes the attachments of the answer, one line each: the digest of the tree and the run.
fn list(answer: &serde_json::Value, out: &mut dyn Write) -> std::io::Result<()> {
    for attachment in answer["attachments"].as_array().into_iter().flatten() {
        let (tree, run) = (&attachment["tree"], &attachment["run"]);
        let line = format!(
            "{} {}\n",
            tree.as_str().unwrap_or(""),
            run.as_str().unwrap_or("")
        );
        out.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// What listing the attachments asks of the control socket.
fn attachments(settings: &FsSettings, out: &mut dyn Write) -> Result<(), String> {
    let socket = &settings.control_socket;
    let answer = control_answer(socket)
        .map_err(|why| format!("the control socket {}: {why}", socket.display()))?;
    list(&answer, out).map_err(|why| format!("writing the output: {why}"))
}

/// The exit status of `tierstore fs <args>`.
pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let args = match parsed::<Args>("fs", args, out, err) {
        Ok(args) => args,
        Err(status) => return status,
    };
    let Some(config) = args.config else {
        let _ = writeln!(err, "tierstore fs: --config is needed");
        return ExitCode::from(2);
    };
    if !args.check_config && args.command.is_none() {
        let _ = writeln!(err, "{NOT_A_PROCESS}");
        return ExitCode::from(2);
    }
    if args.check_config && args.command.is_some() {
        let _ = writeln!(
            err,
            "tierstore fs: --check-config does not go with attachments"
        );
        return ExitCode::from(2);
    }
    let settings = match load(&config) {
        Ok(settings) => settings,
        Err(why) => {
            let _ = writeln!(err, "tierstore fs: {why}");
            return ExitCode::from(2);
        }
    };
    match args.command {
        None => ExitCode::SUCCESS,
        Some(Command::Attachments) => match attachments(&settings, out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                let _ = writeln!(err, "tierstore fs: {why}");
                ExitCode::from(1)
            }
        },
    }
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
