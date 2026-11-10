//! The verb `tierstore fs`: the virtual build file system's process, and the operator's list of
//! what is attached.
//!
//! ```text
//! tierstore fs --config <file>                serve the mount (FUSE on Linux, NFS on macOS), until SIGTERM or SIGINT
//! tierstore fs attachments --config <file>    list the attachments: `<tree digest> <run>`
//! ```
//!
//! The file has an `[fs]` table (see the `treefs` module); it is not the file of the S3 instance.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use super::parsed;
#[cfg(not(target_os = "linux"))]
use crate::server::treefs::NfsError;
#[cfg(target_os = "linux")]
use crate::server::treefs::StartError;
use crate::server::treefs::{FsSettings, load};

/// The arguments of the verb.
#[derive(Debug, Parser)]
#[command(about = "The virtual build file system: one FUSE mount of the trees clients attach")]
struct Args {
    /// The configuration file, which has an `[fs]` table (needed).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the verb does besides serving.
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

/// The exit status of a serve that ended with `result`.
#[cfg(target_os = "linux")]
fn finished(result: Result<(), StartError>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore fs: {why}");
            ExitCode::from(why.status())
        }
    }
}

/// Serves the mount.
#[cfg(target_os = "linux")]
fn serve(settings: &FsSettings, err: &mut dyn Write) -> ExitCode {
    finished(crate::server::treefs::serve(settings), err)
}

/// Serves the NFS export and mounts it.
#[cfg(not(target_os = "linux"))]
fn serve(settings: &FsSettings, err: &mut dyn Write) -> ExitCode {
    match crate::server::treefs::serve(settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore fs: {why}");
            ExitCode::from(NfsError::status(&why))
        }
    }
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
    let settings = match load(&config) {
        Ok(settings) => settings,
        Err(why) => {
            let _ = writeln!(err, "tierstore fs: {why}");
            return ExitCode::from(2);
        }
    };
    match args.command {
        None => serve(&settings, err),
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
