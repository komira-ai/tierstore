//! The verb `tierstore fs`: the virtual build file system's process, and the operator's list of
//! what is attached.
//!
//! ```text
//! tierstore fs --config <file>                serve the mount (Linux), until SIGTERM or SIGINT
//! tierstore fs attachments --config <file>    list the attachments: `<tree digest> <run>`
//! ```
//!
//! The file has an `[fs]` table (see the `treefs` module); it is not the file of the S3 instance.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use super::parsed;
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

/// What listing the attachments asks of the control socket.
fn attachments(settings: &FsSettings, out: &mut dyn Write) -> Result<(), String> {
    use std::io::{BufRead as _, BufReader};
    let socket = &settings.control_socket;
    let stream = UnixStream::connect(socket)
        .map_err(|why| format!("the control socket {}: {why}", socket.display()))?;
    (&stream)
        .write_all(b"attachments\n")
        .map_err(|why| format!("the control socket: {why}"))?;
    let mut line = String::new();
    let _ = BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|why| format!("the control socket: {why}"))?;
    let answer: serde_json::Value =
        serde_json::from_str(&line).map_err(|why| format!("the control socket's answer: {why}"))?;
    let listed = answer["attachments"].as_array().into_iter().flatten();
    for attachment in listed {
        let (tree, run) = (&attachment["tree"], &attachment["run"]);
        writeln!(out, "{} {}", tree.as_str().unwrap_or(""), run.as_str().unwrap_or(""))
            .map_err(|why| format!("writing the output: {why}"))?;
    }
    Ok(())
}

/// Serves the mount.
#[cfg(target_os = "linux")]
fn serve(settings: &FsSettings, err: &mut dyn Write) -> ExitCode {
    match crate::server::treefs::serve(settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore fs: {why}");
            ExitCode::from(why.status())
        }
    }
}

/// The mount is Linux's.
#[cfg(not(target_os = "linux"))]
fn serve(_settings: &FsSettings, err: &mut dyn Write) -> ExitCode {
    let _ = writeln!(err, "tierstore fs: the mount is made on Linux only");
    ExitCode::from(2)
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
