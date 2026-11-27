//! The verb `tierstore fs`, which is gone. The file system is the `fs` interface of the node's
//! file, served by the node's own process, so any start of the verb (a unit of the two-process
//! layout left behind) exits 2 and says so ([`NOT_A_PROCESS`]), and its
//! `RestartPreventExitStatus=2` stops it from looping. Its list of attachments is the verb
//! `tierstore attachments` ([`super::attachments`]).
//!
//! ```text
//! tierstore fs [anything]    exit 2: the file system is the fs interface of the node's file
//! ```
//!
//! A request for help (`--help` or `-h` as the only argument) is the one thing answered otherwise:
//! a success that says where the verb went. Arguments are not parsed, so that a unit that still
//! passes `--config <file>`, `--check-config` or `attachments` gets the message and not a usage
//! error.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

/// What a start of the verb says before it exits 2.
pub(super) const NOT_A_PROCESS: &str = "tierstore fs is no longer a process of its own: the file \
     system is the [[interface]] with type = \"fs\" of the node's file";

/// What a request for help says.
const HELP: &str = "tierstore fs: gone; the file system is the [[interface]] with type = \"fs\" of \
     the node's file, and its list of attachments is `tierstore attachments --config <file>`\n";

/// The exit status of `tierstore fs <args>`: 2 with the reason, whatever the arguments, but for
/// a request for help, which is a success.
pub(super) fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = args.get(2..).unwrap_or_default();
    if matches!(flags, [flag] if flag == "--help" || flag == "-h") {
        let _ = out.write_all(HELP.as_bytes());
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(err, "{NOT_A_PROCESS}");
    ExitCode::from(2)
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
