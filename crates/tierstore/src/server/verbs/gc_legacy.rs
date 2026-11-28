//! The verb `tierstore gc legacy`: removes the `digest-v1` layout of a build node's cache
//! directory, the files the new cache (`packs-v2/`) does not read (`docs/design/fetch-cache-serve.md`
//! section 13).
//!
//! ```text
//! tierstore gc legacy --config <file>           say what the legacy layout holds, remove nothing
//! tierstore gc legacy --config <file> --apply   remove it
//! ```
//!
//! The cache directory is `local_cache.path` of the file. The legacy layout is three directories
//! under it: `obj/` (the objects and their `.meta` sidecars), `.stage/` (fills in progress) and
//! `extents-v1/` (the entries by extents). Nothing else is touched: not `packs-v2/`, which is the
//! new cache's when `[cache] root` is the same directory, not the lock file `.lock`, and no other
//! file. Each directory is walked depth first; every file is removed on its own, then each
//! directory once it is empty. A symbolic link is removed itself and never followed.
//!
//! Without `--apply` the verb reads the directories, says what they hold and writes nothing.
//! With it, the verb first takes the lock of the cache directory, `.lock`, without waiting, and
//! holds it for the whole removal: a process that holds it (a running `tierstore` that serves
//! from `obj/`) makes the verb refuse, with status 1 and nothing removed. The verb lowers its own
//! CPU priority to the lowest (nice 19) before it reads anything; a priority that cannot be
//! lowered is a warning.
//!
//! The exit status is 0 when the verb did what it said, 1 when the lock is held or a file could
//! not be read or removed, and 2 for a usage error or a file that names no cache directory.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use super::parsed;
use crate::server::lock::{self, LockFile};

/// The name of the verb.
pub(super) const VERB: &str = "gc";

/// The directories of the legacy layout under the cache directory, in the order they are removed.
const LEGACY: [&str; 3] = ["obj", ".stage", "extents-v1"];

/// The lock file of the cache directory, which the process that writes to it holds.
const LOCK: &str = ".lock";

/// The directory of the new cache, which the verb never touches.
const PACKS: &str = "packs-v2";

/// The lowest CPU priority, as `nice` counts it.
const LOWEST: i32 = 19;

/// The arguments of the verb.
#[derive(Debug, Parser)]
#[command(about = "Garbage collection of a build node's cache directory")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

/// What is collected.
#[derive(Debug, Subcommand)]
enum Command {
    /// Remove the digest-v1 layout of the cache directory (obj/, .stage/ and extents-v1/ under
    /// local_cache.path), never packs-v2/.
    Legacy {
        /// The configuration file of the build node, whose `[local_cache]` names the directory.
        #[arg(long)]
        config: PathBuf,
        /// Remove the files; without it the verb says what it would remove and writes nothing.
        #[arg(long)]
        apply: bool,
    },
}

/// What one directory of the legacy layout held: its files (links included) and their bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    files: u64,
    bytes: u64,
}

/// The cache directory the file at `config` names: `local_cache.path`.
fn cache_path(config: &Path) -> Result<PathBuf, String> {
    let text =
        std::fs::read_to_string(config).map_err(|why| format!("{}: {why}", config.display()))?;
    // What the file says is not shown: a file may hold a secret (see `config_syntax`).
    let table = toml::from_str::<toml::Table>(&text)
        .map_err(|_| format!("{}: the file is not TOML", config.display()))?;
    let path = table
        .get("local_cache")
        .and_then(|cache| cache.get("path"))
        .and_then(toml::Value::as_str);
    path.map(PathBuf::from).ok_or_else(|| {
        "local_cache.path: the file names no cache directory, whose legacy layout the verb removes"
            .to_owned()
    })
}

/// Says on `err` that the priority could not be lowered, if `result` is that failure.
fn lowered(result: rustix::io::Result<()>, err: &mut dyn Write) {
    if let Err(why) = result {
        let _ = writeln!(
            err,
            "tierstore gc legacy: warning: the CPU priority was not lowered: {why}"
        );
    }
}

/// Counts `path`, and with `apply` removes it: a file or a link is one file, a directory is
/// everything under it and then itself. A path that is not there counts nothing.
fn sweep(path: &Path, apply: bool, tally: &mut Tally) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !meta.is_dir() {
        tally.files += 1;
        tally.bytes += meta.len();
        if apply {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        sweep(&entry?.path(), apply, tally)?;
    }
    if apply {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

/// The lock of the cache directory `path`, held, or why it cannot be had.
fn take_lock(path: &Path) -> Result<LockFile, String> {
    let file = path.join(LOCK);
    match lock::try_take(&file) {
        Ok(Some(held)) => Ok(held),
        Ok(None) => Err(format!(
            "{} is held by another process, a tierstore that serves this cache: stop it first; \
             nothing was removed",
            file.display()
        )),
        Err(why) => Err(format!("{}: {why}; nothing was removed", file.display())),
    }
}

/// Counts, and with `apply` removes, the legacy layout of the cache directory `path`, saying on
/// `out` what each directory held.
fn legacy(path: &Path, apply: bool, out: &mut dyn Write) -> Result<(), String> {
    let _held = if apply { Some(take_lock(path)?) } else { None };
    let done = if apply { "removed" } else { "would remove" };
    let output = |why: io::Error| format!("writing the output: {why}");
    for name in LEGACY {
        let dir = path.join(name);
        let mut tally = Tally::default();
        sweep(&dir, apply, &mut tally).map_err(|why| format!("{}: {why}", dir.display()))?;
        let (files, bytes) = (tally.files, tally.bytes);
        writeln!(
            out,
            "{done} {}: {files} files, {bytes} bytes",
            dir.display()
        )
        .map_err(output)?;
    }
    let packs = path.join(PACKS);
    writeln!(out, "left as it is: {}", packs.display()).map_err(output)?;
    Ok(())
}

/// The exit status of `tierstore gc <args>`.
pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let args = match parsed::<Args>(VERB, args, out, err) {
        Ok(args) => args,
        Err(status) => return status,
    };
    let Command::Legacy { config, apply } = args.command;
    let path = match cache_path(&config) {
        Ok(path) => path,
        Err(why) => {
            let _ = writeln!(err, "tierstore gc legacy: {why}");
            return ExitCode::from(2);
        }
    };
    lowered(rustix::process::setpriority_process(None, LOWEST), err);
    match legacy(&path, apply, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(err, "tierstore gc legacy: {why}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[path = "gc_legacy_tests.rs"]
mod tests;
