//! `verify`'s look at the binary the fs process runs.
//!
//! A unit that is active says nothing of the binary its process runs. An fs process that was
//! handed over runs the binary at [`SERVICE_BINARY`] that the hook installed before it started
//! the instance; `verify` checks that from the process itself: the file its `/proc/<pid>/exe`
//! names is the file at [`SERVICE_BINARY`] (the same device and inode). The hook installs its copy
//! by a rename, so a process that runs an older copy runs a file that no longer has that name: the
//! link reads `<path> (deleted)`, the inodes differ, and the step fails before the commit.
//!
//! An fs process that the update left alone (the two releases do not both list
//! `handoff-fs-v1`) runs its older release on purpose, and `verify` does not fail for it: that
//! would force the restart the hook refuses. The detail says first which release it still runs,
//! as its binary's `--version` prints it (through its `/proc/<pid>/exe`: the process serves no
//! port and so no build gauge), and the release the update installed.

use super::super::SERVICE_BINARY;
use super::super::ports::{FileId, Instance, PortError};
use super::super::probe::Build;
use super::super::record::Record;
use super::super::report::{Done, Reason};
use super::fs::fs_name;
use super::{Ctx, FsPolicy};

/// What `verify` checks of the binary of the fs process and says of it. For an update that handed
/// the fs process over: the new instance's process runs the installed binary, or the step fails,
/// and nothing is said. For one that left it alone: the sentence that leads the detail. For any
/// other: nothing.
///
/// # Errors
/// The new fs instance has no process, its binary cannot be read, or it is not the installed one.
pub(super) fn check(c: &Ctx<'_, '_>, record: &Record, target: &str) -> Result<String, Done> {
    if c.fs != FsPolicy::Managed {
        return Ok(String::new());
    }
    match (record.fs_next, record.fs_live) {
        (Some(next), _) => runs_installed(c, next).map(|()| String::new()),
        (None, Some(live)) => Ok(left_alone(c, live, target)),
        (None, None) => Ok(String::new()),
    }
}

/// Requires that the process of the fs instance `next` runs the file at [`SERVICE_BINARY`].
fn runs_installed(c: &Ctx<'_, '_>, next: Instance) -> Result<(), Done> {
    let unit = fs_name(next);
    let processes = c.ports.fs_processes;
    let Some(pid) = processes.main_pid(next)? else {
        return Err(Done::fail(
            Reason::NotReady,
            format!("{unit} is active and systemd names no main process of it"),
        ));
    };
    let exe = processes.exe(pid).map_err(|error| {
        Done::fail(
            Reason::HostFailed,
            format!("the binary that {unit} (pid {pid}) runs cannot be read: {error}"),
        )
    })?;
    let installed = processes.installed().map_err(|error| {
        Done::fail(
            Reason::HostFailed,
            format!("the binary the update installed cannot be read: {error}"),
        )
    })?;
    if exe.file == installed {
        return Ok(());
    }
    Err(Done::fail(
        Reason::NotReady,
        format!(
            "{unit} (pid {pid}) runs {} ({}), not the binary the update installed, \
             {SERVICE_BINARY} ({}): it is still on an older release",
            exe.path.display(),
            shown(exe.file),
            shown(installed),
        ),
    ))
}

/// A file as a detail names it.
fn shown(file: FileId) -> String {
    format!("device {}, inode {}", file.dev, file.ino)
}

/// What the detail of `verify` says first of the fs instance `live` that the update left alone.
/// Nothing of it fails the step: a process that cannot be asked is said to be so.
fn left_alone(c: &Ctx<'_, '_>, live: Instance, target: &str) -> String {
    let unit = fs_name(live);
    let processes = c.ports.fs_processes;
    let pid = match processes.main_pid(live) {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            return format!(
                "{unit} was left on its release and has no process now, and the update installed \
                 {target}"
            );
        }
        Err(error) => return cannot_ask(&unit, target, &error),
    };
    match processes.version(pid) {
        Ok(version) if is_version(&version, target) => {
            format!("{unit} (pid {pid}) runs {version}, the release the update installed")
        }
        Ok(version) => format!(
            "{unit} (pid {pid}) still runs release {version}, and the update installed {target}: \
             it was left on its release"
        ),
        Err(error) => cannot_ask(&format!("{unit} (pid {pid})"), target, &error),
    }
}

/// The sentence of an fs process left alone whose release cannot be had.
fn cannot_ask(what: &str, target: &str, error: &PortError) -> String {
    format!(
        "{what} was left on its release, which cannot be read ({error}), and the update installed \
         {target}"
    )
}

/// Whether `version`, as `--version` prints it (`0.5.0+0123456789ab`), is `target`, which is the
/// version alone or the version, `+` and the commit, as for the build gauge.
fn is_version(version: &str, target: &str) -> bool {
    let (version, commit) = version
        .split_once('+')
        .map_or((version, None), |(version, commit)| {
            (version, Some(commit.to_owned()))
        });
    Build {
        version: version.to_owned(),
        commit,
        slot: None,
    }
    .is(target)
}
