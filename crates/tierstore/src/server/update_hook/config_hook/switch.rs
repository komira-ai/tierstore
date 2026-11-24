//! The switch of a Mac from two jobs to one (`docs/design/one-service.md` section 4.3), the part of
//! the config hook's steps that is not the update hook's arm: the staged file has an `fs`
//! interface and the job `tierstore-fs` is loaded.
//!
//! The switch is a restart, as every update of a Mac that unmounts: `prepare` has the arm's
//! params declare one, and notes the switch in the config hook's record (`fs_switch`), so that a
//! run again and a rollback know it. `cutover` boots the fs job out, requires its mount gone,
//! disables the job (kept across reboots, so that the plist the package still ships never starts
//! it again), and then the arm boots the main job out and in on the new file, which mounts and
//! serves. `verify` also requires the mount listed at the mount point of the new file. `rollback`
//! enables the fs job first and then has the arm do the old restart with the fs job: fs job out,
//! main job out and in, fs job in.
//!
//! The mount point to wait for is the one the fs job's own file names ([`FS_CONFIG_FILE`]), as for
//! an update that restarts the fs job; a node with no such file is refused before anything is
//! stopped.

use std::path::{Path, PathBuf};

use super::super::launchd::fs::{FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS, mountpoint_of};
use super::super::params::{Params, ServiceManager};
use super::super::ports::{LaunchdPorts, StateDir};
use super::super::record::Record;
use super::super::report::{Done, Reason};
use super::super::shape::fs_interface;
use super::super::{FS_CONFIG_FILE, FS_LAUNCHD_LABEL, FS_LAUNCHD_PLIST};

/// Notes the switch in the record the arm's `prepare` wrote.
pub(super) fn note(state: &dyn StateDir) -> Result<(), Done> {
    let _lock = state.lock()?;
    // The arm has just saved a record; one that is gone fails to decode as an empty text does.
    let mut record = Record::decode(&state.load()?.unwrap_or_default())?;
    record.fs_switch = true;
    record.check()?;
    state.save(&record.encode())?;
    Ok(())
}

/// Whether the update in flight is the switch.
pub(super) fn noted(state: &dyn StateDir) -> Result<bool, Done> {
    let _lock = state.lock()?;
    let Some(bytes) = state.load()? else {
        return Ok(false);
    };
    let record = Record::decode(&bytes)?;
    Ok(record.updating && record.fs_switch)
}

/// The params of the arm's `cutover` of the switch: a restart, with no fs job to boot in.
pub(super) fn cutover_params(plain: &Params, label: &str) -> Params {
    let mut params = plain.clone();
    params.declared_restart = true;
    params.service_manager = ServiceManager::Launchd {
        label: label.to_owned(),
        fs_label: None,
    };
    params
}

/// The params of the arm's `rollback` of the switch: the old restart, with the fs job.
pub(super) fn rollback_params(plain: &Params, label: &str) -> Params {
    let mut params = plain.clone();
    params.service_manager = ServiceManager::Launchd {
        label: label.to_owned(),
        fs_label: Some(FS_LAUNCHD_LABEL.to_owned()),
    };
    params
}

/// Boots the fs job out, requires its mount gone and disables the job. A job that is not loaded
/// (an earlier run booted it out, and may have stopped before the disable) is disabled again,
/// which launchd takes for done when it is. A mount that stays ends the step with the fs job
/// booted in again and not disabled, and nothing else stopped.
pub(super) fn fs_job_out(ports: &LaunchdPorts<'_>) -> Result<(), Done> {
    if !ports.launchd.loaded(FS_LAUNCHD_LABEL)? {
        ports.launchd.disable(FS_LAUNCHD_LABEL)?;
        return Ok(());
    }
    let mountpoint = fs_mountpoint(ports)?;
    ports
        .launchd
        .bootout_within(FS_LAUNCHD_LABEL, FS_BOOTOUT_TIMEOUT, FS_GONE_ATTEMPTS)?;
    let listed = ports.host.mounted(&mountpoint);
    if !matches!(listed, Ok(false)) {
        ports.launchd.bootstrap(Path::new(FS_LAUNCHD_PLIST))?;
        return Err(match listed {
            Err(error) => error.into(),
            Ok(_) => Done::fail(
                Reason::NotReady,
                format!(
                    "{} is still mounted after {FS_LAUNCHD_LABEL} stopped: the job is loaded \
                     again, and the main job is left as it is",
                    mountpoint.display()
                ),
            ),
        });
    }
    ports.launchd.disable(FS_LAUNCHD_LABEL)?;
    Ok(())
}

/// The mount point the fs job's file names.
fn fs_mountpoint(ports: &LaunchdPorts<'_>) -> Result<PathBuf, Done> {
    let Some(text) = ports.host.config(Path::new(FS_CONFIG_FILE))? else {
        return Err(Done::fail(
            Reason::ConfigMissing,
            format!(
                "there is no configuration file at {FS_CONFIG_FILE}, so the mount point of the \
                 file service is not known: nothing was stopped"
            ),
        ));
    };
    mountpoint_of(&text).map_err(|why| {
        Done::fail(
            Reason::ConfigRefused,
            format!(
                "the mount point cannot be read from {FS_CONFIG_FILE} ({why}): nothing was \
                 stopped"
            ),
        )
    })
}

/// `verify`'s part of the switch: the mount is listed at the mount point of the installed file.
pub(super) fn mounted(ports: &LaunchdPorts<'_>, config: &Path) -> Result<(), Done> {
    let Some(shape) = ports.host.config(config)?.as_deref().and_then(fs_interface) else {
        return Err(Done::fail(
            Reason::ConfigMissing,
            format!(
                "{} has no fs interface: the mount point to look for is not known",
                config.display()
            ),
        ));
    };
    if ports.host.mounted(&shape.mountpoint)? {
        return Ok(());
    }
    Err(Done::fail(
        Reason::NotReady,
        format!("{} is not mounted", shape.mountpoint.display()),
    ))
}
