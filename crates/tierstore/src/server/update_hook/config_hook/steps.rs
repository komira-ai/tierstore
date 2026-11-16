//! The steps of the config hook over the ports of the two arms. Each step does what is the
//! config package's own (the check of the staged and the installed file, the guard, the
//! instance written back into the release's record) and hands the rest to the update hook's arm
//! with params that name this binary's build as the version on both sides of the update, so that
//! the arm starts, observes, retires and hands back instances exactly as for a release.

use std::path::{Path, PathBuf};

use super::super::args::Step;
use super::super::params::{Params, Previous};
use super::super::ports::{Guard, Host, Instance, LaunchdPorts, PortError, StateDir, SystemdPorts};
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::super::{DIR_TIER_KIND, HANDOFF_SOCKET_KEY, HOT_STORE_TABLE};
use super::staged_copy;

/// The network guard's unit on a Linux node.
pub const GUARD_UNIT: &str = "tierstore-guard.service";
/// The file the guard loads, which a config package may ship.
pub const GUARD_FILE: &str = "/etc/tierstore/guard.nft";

/// The host as the config hook asks it: every release is the one that serves, `release`, since
/// a config package carries no binary.
pub struct Running<'a> {
    /// The host.
    pub host: &'a dyn Host,
    /// The release whose binary serves.
    pub release: &'a Path,
}

impl Host for Running<'_> {
    fn config(&self, path: &Path) -> Result<Option<String>, PortError> {
        self.host.config(path)
    }

    fn capabilities(&self, _release: &Path) -> Result<Vec<String>, PortError> {
        self.host.capabilities(self.release)
    }

    fn check_config(&self, _release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.host.check_config(self.release, config)
    }
}

/// The configuration file the params name (the config hook's params always name one).
fn config_file(params: &Params) -> Result<&Path, Done> {
    params
        .config_file
        .as_deref()
        .ok_or_else(|| Done::fail(Reason::BadParams, "the params name no configuration file"))
}

/// The params the arm is run with: the build that serves is the version before and after, the
/// previous release is the previous config package (or the staged one, for the first: the arm
/// then restarts or hands over as for an update, never as for a first install), and
/// `config_file` is `config`.
fn arm_params(params: &Params, build: &str, config: &Path, restart: bool) -> Params {
    let mut arm = params.clone();
    arm.target_version = build.to_owned();
    arm.previous = Some(Previous {
        release: params
            .previous
            .as_ref()
            .map_or_else(|| params.staged_release.clone(), |p| p.release.clone()),
        version: build.to_owned(),
    });
    arm.config_file = Some(config.to_path_buf());
    arm.declared_restart = params.declared_restart || restart;
    arm
}

/// Refuses while an update of the release is in flight: one package moves the instances at a
/// time.
fn release_at_rest(service: &dyn StateDir) -> Result<(), Done> {
    let Some(bytes) = service.load()? else {
        return Ok(());
    };
    if Record::decode(&bytes)?.updating {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update of the release is in flight: it finishes first",
        ));
    }
    Ok(())
}

/// Has the binary that serves check `path`; a refusal fails the step with the line it said.
fn checked(host: &dyn Host, release: &Path, which: &str, path: &Path) -> Result<(), Done> {
    match host.check_config(release, path)? {
        None => Ok(()),
        Some(line) => Err(Done::fail(
            Reason::ConfigRefused,
            format!(
                "the {which} configuration {} is refused: {line}",
                path.display()
            ),
        )),
    }
}

/// The staged copy of the configuration, checked by the binary that serves; its text.
fn staged_checked(
    host: &dyn Host,
    release: &Path,
    params: &Params,
) -> Result<(PathBuf, String), Done> {
    let staged = staged_copy(&params.staged_release, config_file(params)?);
    let text = host.config(&staged)?.ok_or_else(|| {
        Done::fail(
            Reason::ConfigMissing,
            format!("the config package stages no {}", staged.display()),
        )
    })?;
    checked(host, release, "staged", &staged)?;
    Ok((staged, text))
}

/// What of a configuration decides whether an instance can take the sockets of an instance
/// started from the other: the listening addresses, the hand-over socket and a hot tier that is
/// a directory (which two instances never share).
fn handover_shape(text: &str) -> Option<(Option<toml::Value>, Vec<Option<toml::Value>>, bool)> {
    let table: toml::Table = text.parse().ok()?;
    let server = table.get("server").and_then(toml::Value::as_table);
    let of_server = |key: &str| server.and_then(|server| server.get(key)).cloned();
    let listening = vec![
        table.get("interface").cloned(),
        of_server("listen"),
        of_server("slot"),
    ];
    let dir_hot = table
        .get(HOT_STORE_TABLE)
        .and_then(toml::Value::as_table)
        .and_then(|hot| hot.get("kind"))
        .and_then(toml::Value::as_str)
        == Some(DIR_TIER_KIND);
    Some((of_server(HANDOFF_SOCKET_KEY), listening, dir_hot))
}

/// Whether the change from `serving` to `staged` must be a restart: the two files do not listen
/// alike, do not name the same hand-over socket, or either has a directory hot tier, or the file
/// that serves cannot be read as TOML.
fn must_restart(serving: Option<&str>, staged: &str) -> bool {
    match (serving.and_then(handover_shape), handover_shape(staged)) {
        (Some((socket, listening, dir_hot)), Some(new)) => {
            dir_hot || new.2 || socket != new.0 || listening != new.1
        }
        _ => true,
    }
}

/// What every arm needs beside its own ports.
pub struct Common<'a> {
    /// The update hook's record of the release, read and, after `cleanup`, written.
    pub service: &'a dyn StateDir,
    /// The release whose binary serves (for the [`Running`] host).
    pub release: &'a Path,
    /// The version the build gauge of the serving binary shows.
    pub build: &'a str,
}

/// `prepare`'s own part: the release at rest, the staged file accepted, and whether the change
/// is a restart. Returns the arm's params.
fn prepare_params(
    params: &Params,
    ports_host: &dyn Host,
    common: &Common<'_>,
) -> Result<Params, Done> {
    release_at_rest(common.service)?;
    let (staged, text) = staged_checked(ports_host, common.release, params)?;
    let serving = ports_host.config(config_file(params)?)?;
    let restart = must_restart(serving.as_deref(), &text);
    Ok(arm_params(params, common.build, &staged, restart))
}

/// A config package that is the node's first has no files to go back to: hostkeeper takes the
/// package, and with it the files, off the node. The instances stay as they are; the record
/// says nothing is in flight, with the instance that runs as the one that serves.
fn no_previous(serving: Option<Instance>) -> Done {
    let detail = serving.map_or_else(
        || "nothing to go back to: no earlier config package".to_owned(),
        |instance| {
            format!(
                "nothing to go back to: no earlier config package; tierstore@{instance} keeps \
                 serving on what it read, and the node needs a configuration before its next start"
            )
        },
    );
    Done::fail(Reason::NoPreviousRelease, detail)
}

/// The steps on a node of two systemd instances. `ports.state` is the config hook's record, and
/// `ports.host` a [`Running`] host.
pub fn systemd_step(
    step: Step,
    params: &Params,
    ports: &SystemdPorts<'_>,
    guard: &dyn Guard,
    common: &Common<'_>,
) -> Done {
    systemd_attempt(step, params, ports, guard, common).unwrap_or_else(std::convert::identity)
}

fn systemd_attempt(
    step: Step,
    params: &Params,
    ports: &SystemdPorts<'_>,
    guard: &dyn Guard,
    common: &Common<'_>,
) -> Result<Done, Done> {
    let config = config_file(params)?.to_path_buf();
    let arm = |step: Step, params: &Params| super::super::systemd::run_step(step, params, ports);
    let plain = arm_params(params, common.build, &config, false);
    match step {
        Step::Restore => Err(not_a_step()),
        Step::Uninstall => Ok(uninstalled()),
        Step::Prepare => Ok(arm(step, &prepare_params(params, ports.host, common)?)),
        Step::Cutover => {
            guard.reload()?;
            Ok(arm(step, &plain))
        }
        Step::Verify => {
            checked(ports.host, common.release, "installed", &config)?;
            Ok(arm(step, &plain))
        }
        Step::Rollback if params.previous.is_none() => systemd_first_back(ports),
        Step::Rollback => {
            guard.reload()?;
            Ok(arm(step, &plain))
        }
        Step::Cleanup => {
            let done = arm(step, &plain);
            if done.outcome == super::super::report::Outcome::Success {
                write_back(ports.state, common.service)?;
            }
            Ok(done)
        }
    }
}

/// The rollback of a node's first config package on systemd: the instance that runs serves,
/// the other one's marker goes, and the record says nothing is in flight.
fn systemd_first_back(ports: &SystemdPorts<'_>) -> Result<Done, Done> {
    let _lock = ports.state.lock()?;
    let Some(bytes) = ports.state.load()? else {
        return Ok(Done::ok(
            "nothing to roll back: no config update is recorded",
        ));
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be rolled back",
        ));
    }
    if !record.updating {
        return Ok(Done::ok(
            "nothing to roll back: no config update is in flight",
        ));
    }
    let mut running = Vec::new();
    for instance in Instance::ALL {
        if ports.units.state(instance)?.running() {
            running.push(instance);
        }
    }
    let serving = match running.as_slice() {
        [one] => Some(*one),
        [] => record.live,
        _ => {
            return Err(Done::deferred(
                Reason::Draining,
                "both instances run, and one of them drains: the rollback waits",
                10,
            ));
        }
    };
    for instance in Instance::ALL {
        if Some(instance) != serving {
            ports.markers.clear(instance)?;
        }
    }
    ports.state.save(&Record::idle(serving).encode())?;
    Err(no_previous(serving))
}

/// Writes the instance the config hook's record says serves into the release's record, which
/// the boot step reads; a release's record that is absent, or in an update, is left as it is.
/// The config hook's own record was written by the arm's `cleanup` just before; one that cannot
/// be decoded names no instance, and nothing is written back (the boot step then starts the
/// instance the release's record names, which serves the same files).
fn write_back(config: &dyn StateDir, service: &dyn StateDir) -> Result<(), Done> {
    let _lock = config.lock()?;
    let Some(live) = config
        .load()?
        .and_then(|bytes| Record::decode(&bytes).ok())
        .and_then(|record| record.live)
    else {
        return Ok(());
    };
    let Some(bytes) = service.load()? else {
        return Ok(());
    };
    let mut release = Record::decode(&bytes)?;
    if release.updating || release.live == Some(live) {
        return Ok(());
    }
    release.live = Some(live);
    release.check()?;
    service.save(&release.encode())?;
    Ok(())
}

/// The steps on a Mac, one launchd job `label`. `ports.state` is the config hook's record, and
/// `ports.host` a [`Running`] host.
pub fn launchd_step(
    step: Step,
    params: &Params,
    label: &str,
    ports: &LaunchdPorts<'_>,
    common: &Common<'_>,
) -> Done {
    launchd_attempt(step, params, label, ports, common).unwrap_or_else(std::convert::identity)
}

fn launchd_attempt(
    step: Step,
    params: &Params,
    label: &str,
    ports: &LaunchdPorts<'_>,
    common: &Common<'_>,
) -> Result<Done, Done> {
    let config = config_file(params)?.to_path_buf();
    let arm =
        |step: Step, params: &Params| super::super::launchd::run_step(step, params, label, ports);
    let plain = arm_params(params, common.build, &config, false);
    match step {
        Step::Restore => Err(not_a_step()),
        Step::Uninstall => Ok(uninstalled()),
        Step::Prepare => Ok(arm(step, &prepare_params(params, ports.host, common)?)),
        Step::Verify => {
            checked(ports.host, common.release, "installed", &config)?;
            Ok(arm(step, &plain))
        }
        Step::Rollback if params.previous.is_none() => launchd_first_back(ports),
        Step::Cutover => {
            stop_for_new_file(label, ports)?;
            Ok(arm(step, &plain))
        }
        Step::Rollback | Step::Cleanup => Ok(arm(step, &plain)),
    }
}

/// Boots the job out before the arm's `cutover` of an update that has not swapped yet: the job
/// serves the build the arm looks for, so the arm alone would see it serving and leave it on the
/// old file. The arm then bootstraps it, and the job reads the new file. A run again after the
/// swap boots nothing out.
fn stop_for_new_file(label: &str, ports: &LaunchdPorts<'_>) -> Result<(), Done> {
    let _lock = ports.state.lock()?;
    let Some(bytes) = ports.state.load()? else {
        return Ok(());
    };
    let record = Record::decode(&bytes)?;
    // A committed record has `swapped` (the record's rules), so this is an update before the swap.
    if record.updating && !record.has(Marker::Swapped) {
        ports.launchd.bootout(label)?;
    }
    Ok(())
}

/// The rollback of a Mac's first config package: the job runs on what it read, and the record
/// says nothing is in flight.
fn launchd_first_back(ports: &LaunchdPorts<'_>) -> Result<Done, Done> {
    let _lock = ports.state.lock()?;
    let Some(bytes) = ports.state.load()? else {
        return Ok(Done::ok(
            "nothing to roll back: no config update is recorded",
        ));
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be rolled back",
        ));
    }
    if !record.updating {
        return Ok(Done::ok(
            "nothing to roll back: no config update is in flight",
        ));
    }
    ports.state.save(&Record::idle(None).encode())?;
    Err(Done::fail(
        Reason::NoPreviousRelease,
        "nothing to go back to: no earlier config package; the job keeps serving on what it \
         read, and the node needs a configuration before its next start",
    ))
}

fn not_a_step() -> Done {
    Done::fail(
        Reason::BadArgs,
        "restore is not a step of config-hook: a config package has no boot step",
    )
}

fn uninstalled() -> Done {
    Done::ok("uninstall: the service keeps the configuration it read")
}
