//! The verb `tierstore attachments`: the operator's list of what is attached to the virtual build
//! file system of a node. It serves nothing and mounts nothing: it asks the control socket of the
//! node's own process.
//!
//! ```text
//! tierstore attachments --config <node file>    list the attachments: `<tree digest> <run>`
//! ```
//!
//! The file is the node's file, loaded as a start loads it; the socket is the `control_socket` of
//! its `[[interface]]` with `type = "fs"` (by default `control.sock` in the runtime directory). A
//! file that does not load, or has no such interface, is status 2; a socket that cannot be asked,
//! or answers an error, is status 1.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use super::parsed;
use crate::server::config::{Host, PerRun, Settings};
use crate::server::config_load::load;

/// What the verb says of a file that has no `fs` interface.
const NO_FS_INTERFACE: &str = "the file has no [[interface]] with type = \"fs\": there is no file \
     system whose attachments could be listed";

/// The arguments of the verb.
#[derive(Debug, Parser)]
#[command(about = "The attachments of the virtual build file system of a node; it serves nothing")]
struct Args {
    /// The node's configuration file, which has an `[[interface]]` with `type = "fs"` (needed).
    #[arg(long)]
    config: Option<PathBuf>,
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

/// What listing the attachments asks of the control socket `socket`.
fn attachments(socket: &Path, out: &mut dyn Write) -> Result<(), String> {
    let answer = control_answer(socket)
        .map_err(|why| format!("the control socket {}: {why}", socket.display()))?;
    if answer["ok"].as_bool() == Some(false) {
        let why = answer["error"].as_str().unwrap_or("no reason given");
        return Err(format!("the control socket {}: {why}", socket.display()));
    }
    list(&answer, out).map_err(|why| format!("writing the output: {why}"))
}

/// The control socket of the `fs` interface of the node's file at `config`.
///
/// # Errors
/// The reason the file does not load, or that it has no `fs` interface (the shapes of node that
/// cannot have one are refused at load, so a file that loads without one is the same).
fn control_socket(config: &Path) -> Result<PathBuf, String> {
    let per_run = PerRun {
        drain_lb_wait: None,
        drain_deadline: None,
        state_dir: None,
        host: Host::SYSTEM,
    };
    let settings = load(config, per_run).map_err(|why| why.to_string())?;
    let fs = match settings {
        Settings::Cache(cache) => cache.fs,
        Settings::Storage(_) | Settings::Combined(_) | Settings::Cold(_) => None,
    };
    fs.map(|fs| fs.control_socket)
        .ok_or_else(|| NO_FS_INTERFACE.to_owned())
}

/// The exit status of `tierstore attachments <args>`.
pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let args = match parsed::<Args>("attachments", args, out, err) {
        Ok(args) => args,
        Err(status) => return status,
    };
    let Some(config) = args.config else {
        let _ = writeln!(err, "tierstore attachments: --config is needed");
        return ExitCode::from(2);
    };
    let socket = match control_socket(&config) {
        Ok(socket) => socket,
        Err(why) => {
            let _ = writeln!(err, "tierstore attachments: {why}");
            return ExitCode::from(2);
        }
    };
    match attachments(&socket, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore attachments: {why}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
