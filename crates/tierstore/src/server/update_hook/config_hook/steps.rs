//! The steps of the config hook over the ports of the two arms. Each step does what is the
//! config package's own (the check of the staged and the installed file, the guard, the
//! instance written back into the release's record) and hands the rest to the update hook's arm
//! with params that name this binary's build as the version on both sides of the update, so that
//! the arm starts, observes, retires and hands back instances exactly as for a release. The fs
//! process of the old layout is left alone but for the switch of a node to one process, which the
//! arm's `prepare` records (systemd) or `switch.rs` does (a Mac). On systemd a change that the
//! instance that serves applies in place is not handed to the arm at all (`reload.rs`).

use std::path::{Path, PathBuf};

use super::super::args::Step;
use super::super::params::{Params, Previous};
use super::super::ports::{
    Control, Guard, Host, Instance, LaunchdPorts, PortError, StateDir, SystemdPorts,
};
use super::super::record::{Marker, Record, Reload};
use super::super::report::{Done, Reason};
use super::super::shape::{FS_INTERFACE, fs_interface};
use super::super::systemd::FsPolicy;
use super::super::{DIR_TIER_KIND, FS_LAUNCHD_LABEL, HANDOFF_SOCKET_KEY, HOT_STORE_TABLE};
use super::{reload, staged_copy, switch};

/// The network guard's unit on a Linux node.
pub const GUARD_UNIT: &str = "tierstore-guard.service";
/// The file the guard loads, which a config package may ship.
pub const GUARD_FILE: &str = "/etc/tierstore/guard.nft";

/// The host as the config hook asks it: every release is the one that serves, `release`, since
/// a config package carries no binary, and no binary is ever put in place or removed.
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

    fn mounted(&self, path: &Path) -> Result<bool, PortError> {
        self.host.mounted(path)
    }

    fn capabilities(&self, _release: &Path) -> Result<Vec<String>, PortError> {
        self.host.capabilities(self.release)
    }

    fn check_config(&self, _release: &Path, config: &Path) -> Result<Option<String>, PortError> {
        self.host.check_config(self.release, config)
    }

    /// Nothing: the binary that serves stays, as the release does.
    fn install_binary(&self, _release: &Path) -> Result<(), PortError> {
        Ok(())
    }

    /// Nothing: the config hook never removes the binary that serves.
    fn remove_binary(&self) -> Result<(), PortError> {
        Ok(())
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

/// The `interface` of a configuration without its `fs` entries: the file service holds no
/// listening socket of the store, and a successor takes the mount whatever its params say of
/// anything but the three paths of [`shape::FsShape`](super::super::shape::FsShape), which
/// `prepare` compares on their own. A list that has nothing else left is none.
fn listening_interfaces(table: &toml::Table) -> Option<toml::Value> {
    match table.get("interface")? {
        toml::Value::Array(entries) => {
            let kept: Vec<toml::Value> = entries
                .iter()
                .filter(|entry| {
                    entry.get("type").and_then(toml::Value::as_str) != Some(FS_INTERFACE)
                })
                .cloned()
                .collect();
            (!kept.is_empty()).then_some(toml::Value::Array(kept))
        }
        other => Some(other.clone()),
    }
}

/// What of a configuration decides whether an instance can take the sockets of an instance
/// started from the other: the listening addresses (the `fs` interface apart), the hand-over
/// socket and a hot tier that is a directory (which two instances never share).
fn handover_shape(text: &str) -> Option<(Option<toml::Value>, Vec<Option<toml::Value>>, bool)> {
    let table: toml::Table = text.parse().ok()?;
    let server = table.get("server").and_then(toml::Value::as_table);
    let of_server = |key: &str| server.and_then(|server| server.get(key)).cloned();
    let listening = vec![
        listening_interfaces(&table),
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

/// What `prepare`'s own part decided: the arm's params, whether the change is the switch of a node
/// to one process (the staged file has an `fs` interface, the file that serves has none, and the
/// file service of the old layout runs), and, on systemd, what of it the instance that serves
/// applies in place, when that is all of it.
struct Prepared {
    params: Params,
    switch: bool,
    reload: Option<Reload>,
}

/// Whether the package changes the guard's file: the staged copy is not the installed one.
fn guard_changed(host: &dyn Host, params: &Params) -> Result<bool, Done> {
    let installed = host.config(Path::new(GUARD_FILE))?;
    let staged = host.config(&staged_copy(&params.staged_release, Path::new(GUARD_FILE)))?;
    Ok(installed != staged)
}

/// The refusal of a change that would drop or move a mount (design 3.6): the taker would say
/// `nak`, or a restart would end the process that holds it. `mode = "restart"` overrides it.
fn mount_refused(why: String) -> Done {
    Done::fail(
        Reason::ModeUnsupported,
        format!(
            "{why}; the params' mode = \"restart\" overrides it for a node whose jobs are drained"
        ),
    )
}

/// `prepare`'s own part: the release at rest, the staged file accepted, whether the change is a
/// restart, and whether it is the switch (`holds_fs` says whether the file service of the old
/// layout runs; `mac` is true on a Mac, where every config update is a restart of the one job, and
/// so is the switch, by design). Unless the params declare a restart, these are refused, as each
/// would drop or lose the mount of a node whose process serves it: a staged file that drops the
/// `fs` interface or moves its mount; a change that must be a restart; and on a Mac any config
/// update of a node whose file has an `fs` interface (the hand-over to the next image is the
/// update hook's; a config update cannot tell the new image from the one it replaces, as both
/// report the same build). Returns the arm's params. With `in_place` (given on systemd alone, and
/// taken when the params declare no restart), a change the instance that serves applies itself is returned as that, before any
/// of the refusals, which are of a new process: nothing moves.
fn prepare_params(
    params: &Params,
    ports_host: &dyn Host,
    common: &Common<'_>,
    holds_fs: &dyn Fn() -> Result<bool, Done>,
    (mac, in_place): (bool, bool),
) -> Result<Prepared, Done> {
    release_at_rest(common.service)?;
    let (staged, text) = staged_checked(ports_host, common.release, params)?;
    let serving = ports_host.config(config_file(params)?)?;
    let restart = must_restart(serving.as_deref(), &text);
    let reload = serving
        .as_deref()
        .filter(|_| in_place && !params.declared_restart)
        .and_then(|serving| reload::of(serving, &text));
    if let Some(reload) = reload {
        return Ok(Prepared {
            params: arm_params(params, common.build, &staged, restart),
            switch: false,
            reload: Some(Reload {
                guard: guard_changed(ports_host, params)?,
                ..reload
            }),
        });
    }
    let old_fs = serving.as_deref().and_then(fs_interface);
    let new_fs = fs_interface(&text);
    let switch = new_fs.is_some() && old_fs.is_none() && holds_fs()?;
    let switch_restarts = mac;
    if old_fs.is_some() && !params.declared_restart {
        if new_fs.is_none() {
            return Err(mount_refused(
                "the staged file has no fs interface, and the file that serves has one: the new \
                 process would not take the mount, which would be dropped"
                    .to_owned(),
            ));
        }
        if mac {
            return Err(mount_refused(
                "the file that serves has an fs interface, and a config update of a Mac restarts \
                 its job, which would drop the mount"
                    .to_owned(),
            ));
        }
    }
    if let Some(new) = &new_fs
        && !params.declared_restart
    {
        let moved = old_fs.as_ref().map_or_else(Vec::new, |old| old.moved(new));
        if !moved.is_empty() {
            return Err(mount_refused(format!(
                "the staged file moves the mount: {} differ from the file that serves, and the \
                 new process cannot take the mount over",
                moved.join(", ")
            )));
        }
        // On a Mac the switch is a restart by design, and a Mac that serves its mount was refused
        // above.
        if restart && !switch_restarts && (old_fs.is_some() || switch) {
            return Err(mount_refused(
                "the staged file with an fs interface needs a restart, which would drop the \
                 mount"
                    .to_owned(),
            ));
        }
    }
    Ok(Prepared {
        params: arm_params(
            params,
            common.build,
            &staged,
            restart || (switch && switch_restarts),
        ),
        switch,
        reload: None,
    })
}

/// Whether a `tierstore-fs` instance runs.
fn fs_runs(ports: &SystemdPorts<'_>) -> Result<bool, Done> {
    for instance in Instance::ALL {
        if ports.fs_units.state(instance)?.running() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A config package that is the node's first has no files to go back to: hostkeeper takes the
/// package, and with it the files, off the node. The instances stay as they are; the record
/// says nothing is in flight, with the instance that runs as the one that serves.
pub(super) fn no_previous(serving: Option<Instance>) -> Done {
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
/// `ports.host` a [`Running`] host; `control` reaches the control socket of the instance that
/// serves, for a change it applies in place.
pub fn systemd_step(
    step: Step,
    params: &Params,
    ports: &SystemdPorts<'_>,
    guard: &dyn Guard,
    control: &dyn Control,
    common: &Common<'_>,
) -> Done {
    systemd_attempt(step, params, ports, guard, control, common)
        .unwrap_or_else(std::convert::identity)
}

fn systemd_attempt(
    step: Step,
    params: &Params,
    ports: &SystemdPorts<'_>,
    guard: &dyn Guard,
    control: &dyn Control,
    common: &Common<'_>,
) -> Result<Done, Done> {
    let config = config_file(params)?.to_path_buf();
    // A configuration update leaves the fs process alone, as its own file is not in that package,
    // except the switch to one process, which `prepare` notes in the record.
    let arm = |step: Step, params: &Params, policy: FsPolicy| {
        super::super::systemd::run_step_with(step, params, ports, policy)
    };
    let left = FsPolicy::LeftAlone;
    let plain = arm_params(params, common.build, &config, false);
    // A step after `prepare` of an update applied in place is the reload's own, not the arm's.
    let act = reload::Act {
        ports,
        guard,
        control,
        params: &plain,
        first: params.previous.is_none(),
    };
    let in_place = || -> Result<Option<Done>, Done> {
        Ok(reload::open(ports.state)?.map(|opened| reload::run(step, &opened, &act)))
    };
    match step {
        Step::Restore => Err(not_a_step()),
        Step::Uninstall => Ok(uninstalled()),
        Step::Prepare => {
            let holds_fs = || fs_runs(ports);
            let mut prepared =
                prepare_params(params, ports.host, common, &holds_fs, (false, true))?;
            if let Some(reload) = prepared.reload.take() {
                if let Some(done) = reload::prepare(params, ports, common.build, reload)? {
                    return Ok(done);
                }
                // The node is not one instance serving: the change is the arm's, refusals and all.
                prepared = prepare_params(params, ports.host, common, &holds_fs, (false, false))?;
            }
            let policy = if prepared.switch {
                FsPolicy::Switch
            } else {
                left
            };
            Ok(arm(step, &prepared.params, policy))
        }
        Step::Cutover => {
            if let Some(done) = in_place()? {
                return Ok(done);
            }
            guard.reload()?;
            Ok(arm(step, &plain, left))
        }
        Step::Verify => {
            checked(ports.host, common.release, "installed", &config)?;
            Ok(in_place()?.unwrap_or_else(|| arm(step, &plain, left)))
        }
        Step::Rollback => match in_place()? {
            Some(done) => Ok(done),
            None if params.previous.is_none() => systemd_first_back(ports, &plain),
            None => {
                guard.reload()?;
                Ok(arm(step, &plain, left))
            }
        },
        Step::Cleanup => {
            let done = in_place()?.unwrap_or_else(|| arm(step, &plain, left));
            if done.outcome == super::super::report::Outcome::Success {
                write_back(ports.state, common.service)?;
            }
            Ok(done)
        }
    }
}

/// The rollback of a node's first config package on systemd: the instance that runs serves,
/// the other one's marker goes, and the record says nothing is in flight.
fn systemd_first_back(ports: &SystemdPorts<'_>, plain: &Params) -> Result<Done, Done> {
    let _lock = ports.state.lock()?;
    let Some(bytes) = ports.state.load()? else {
        return Ok(Done::ok(
            "nothing to roll back: no config update is recorded",
        ));
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    // A record at rest on another config package: this package's update was never recorded (its
    // prepare failed), and there is nothing to roll back.
    if let Some(detail) = record.nothing_to_roll_back(plain) {
        return Ok(Done::ok(detail));
    }
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

/// Writes the instances the config hook's record says serve (the S3 process, and the fs process
/// when the record names one) into the release's record, which the boot step reads; a release's
/// record that is absent, or in an update, is left as it is. The config hook's own record was
/// written by the arm's `cleanup` just before; one that cannot be decoded names no instance, and
/// nothing is written back (the boot step then starts the instance the release's record names,
/// which serves the same files).
pub(super) fn write_back(config: &dyn StateDir, service: &dyn StateDir) -> Result<(), Done> {
    let _lock = config.lock()?;
    let Some((live, fs_live, switched)) = config
        .load()?
        .and_then(|bytes| Record::decode(&bytes).ok())
        .and_then(|record| {
            record
                .live
                .map(|live| (live, record.fs_live, record.fs_switch))
        })
    else {
        return Ok(());
    };
    let Some(bytes) = service.load()? else {
        return Ok(());
    };
    let mut release = Record::decode(&bytes)?;
    // A node with no fs process leaves the release's `fs_live` as it is; the switch to one
    // process leaves none.
    let fs_live = if switched {
        None
    } else {
        fs_live.or(release.fs_live)
    };
    if release.updating || (release.live == Some(live) && release.fs_live == fs_live) {
        return Ok(());
    }
    release.live = Some(live);
    release.fs_live = fs_live;
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
        Step::Prepare => {
            let loaded = || Ok(ports.launchd.loaded(FS_LAUNCHD_LABEL)?);
            let prepared = prepare_params(params, ports.host, common, &loaded, (true, false))?;
            let done = arm(step, &prepared.params);
            if prepared.switch && done.outcome == super::super::report::Outcome::Success {
                switch::note(ports.state)?;
            }
            Ok(done)
        }
        Step::Verify => {
            checked(ports.host, common.release, "installed", &config)?;
            if switch::noted(ports.state)? {
                switch::mounted(ports, &config)?;
            }
            Ok(arm(step, &plain))
        }
        Step::Rollback if params.previous.is_none() => launchd_first_back(ports, &plain),
        Step::Cutover if switch::noted(ports.state)? => {
            switch::fs_job_out(ports)?;
            stop_for_new_file(label, ports)?;
            Ok(arm(step, &switch::cutover_params(&plain, label)))
        }
        Step::Cutover => {
            stop_for_new_file(label, ports)?;
            Ok(arm(step, &plain))
        }
        Step::Rollback if switch::noted(ports.state)? => {
            ports.launchd.enable(FS_LAUNCHD_LABEL)?;
            Ok(arm(step, &switch::rollback_params(&plain, label)))
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
fn launchd_first_back(ports: &LaunchdPorts<'_>, plain: &Params) -> Result<Done, Done> {
    let _lock = ports.state.lock()?;
    let Some(bytes) = ports.state.load()? else {
        return Ok(Done::ok(
            "nothing to roll back: no config update is recorded",
        ));
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    // A record at rest on another config package: this package's update was never recorded (its
    // prepare failed), and there is nothing to roll back.
    if let Some(detail) = record.nothing_to_roll_back(plain) {
        return Ok(Done::ok(detail));
    }
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
