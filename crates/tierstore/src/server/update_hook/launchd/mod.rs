//! The steps of the launchd arm: one job, whose label is `launchd_label` of the params, and
//! whose plist is [`LAUNCHD_PLIST`]. An update is a hand-over to the next image of the job's
//! process when both releases list `handoff-exec-v1` and the params name no fs job, and a restart
//! with a gap otherwise; [`exec`] says how the way is decided.
//!
//! hostkeeper switches `current` before `cutover` and back before `rollback`; the hook never
//! moves the link. The plist runs [`SERVICE_BINARY`](super::SERVICE_BINARY), where the hook puts
//! the binary of the release `current` names before it loads the job (in `cutover`, `rollback`
//! and `restore`), so booting the plist out and in again starts that release.
//! Every step takes the lock of the state directory, reads and checks the record, and sets state
//! rather than does things once, so that a run after a crash converges. A record that names a
//! systemd instance (`live`, `next` or `mode`) is not this arm's and is refused.
//!
//! The update hook of a release ([`run_step_with`] with `releases`) refuses in `prepare` a node of
//! the two-process layout and proves the node's volume in `verify`, as [`volume`] describes.
//!
//! When the params name the job of `tierstore fs` (`fs_launchd_label`), the arm drives it too, as
//! [`fs`] describes, except in an update of the release on a node whose file has an `fs`
//! interface: that node's one process serves the mount, its fs job is booted out and disabled by
//! the switch, and every step (the boot step too) leaves it alone, whatever the params say. The macOS package's manifest does not name it: that job serves nothing since
//! 0.3.196 (`tierstore fs --config` exits 2). The config hook's switch of a Mac names it for its
//! rollback, which goes back to the two-process layout. Every boot-out of the main job (`cutover`
//! of an update or of a first install over a loaded job, `rollback`, `uninstall`) is then
//! preceded by the boot-out of the fs job and a check that its mount is gone (a first install
//! looks at the fs job even when the main job is not loaded), and the fs job is booted in after
//! the main job serves (`cutover`, `rollback`) or after it is loaded (`restore`). A mount that
//! stays ends the step with `not_ready`, with the main job untouched.
//!
//! - `prepare` needs the node's configuration file to exist (it does not need a hand-over
//!   socket), and records the update: `from` the previous release (none on a first install),
//!   `to` the staged release and its `version`; the way is not recorded. Where the params leave
//!   it open it asks the two releases what they list. It refuses a restart of a node whose file
//!   has an `fs` interface (`mode_unsupported`), and, for a release, a node whose file has a mount
//!   provider and no `fs` interface (`mode_unsupported`, [`volume`]), and has the staged release
//!   check the file before a hand-over. Nothing is started or stopped.
//! - `cutover` of a restart boots the fs job out first when there is one and it is loaded, boots
//!   the job out and in, waits for `/readyz` and for the build of the target version, boots the
//!   fs job in when there is one and it is not loaded, and records `swapped`. Of a hand-over, a
//!   job that is loaded and does not serve the target gets the staged binary and `SIGUSR2`, and
//!   the step waits for the build of the target version; a job that is not loaded is booted in. A first install boots the
//!   job out only when it is loaded: a copy installed by hand, whose job serves another version
//!   or nothing, is taken over as an update would take it over. A run that finds the job loaded
//!   and serving the target only records `swapped`. A failure is reported; hostkeeper then runs
//!   `rollback`.
//! - `verify` observes the target version served by the loaded job and, for a release whose node
//!   has an `fs` interface, the volume served at its mount point ([`volume`]); on a node with a
//!   mount provider the job, once it is the writer, must then pass the mount check
//!   ([`mount_check`]); and it records `verified`. A job that serves read-only (its cache has not
//!   taken the write role yet) defers with `not_ready` and records nothing, and a volume that is
//!   not served fails with `not_ready`. After the commit it only observes the version.
//! - `rollback` boots the fs job out first when there is one, then, for an update, puts the
//!   previous binary in place and boots the job out and in again (a hand-over of a loaded job
//!   sends `SIGUSR2` instead), waits for the previous version and boots the fs job in; a first
//!   install
//!   has nothing to go back to: the jobs stay out and the step fails with `no_previous_release`,
//!   again on every re-run.
//! - `cleanup` confirms afresh what `verify` saw, writes `stopping` (the commit) and ends the
//!   update. There is no instance to retire. When the write of `stopping` fails, the record is
//!   read back, so that a write that reached the disk is reported as the commit it is; a record
//!   that cannot be read back counts as committed.
//! - `uninstall` boots the fs job out first when there is one, boots the job out, removes the
//!   copy of the binary, and leaves the data, the cache and the configuration of the node, and
//!   the record directory, as they are.
//! - `restore`, the boot step, puts the current release's binary in place and boots the job in
//!   when it is not loaded, then boots the fs job in when there is one and it is not loaded.
//!
//! A record of another update, in flight and not committed, that returns the host to the params'
//! previous release is one whose updater is gone: a `prepare` that finds it past its cutover, and
//! a `rollback`, roll it back first with the params it was written under, then do their own step
//! (`Record::stale`).

mod exec;
pub(super) mod fs;
mod volume;

use std::cell::Cell;
use std::path::Path;

use super::LAUNCHD_PLIST;
use super::args::Step;
use super::look::{Look, verified, writable};
use super::mount_check;
use super::params::Params;
use super::ports::LaunchdPorts;
use super::record::{Marker, Record, Stale, Written};
use super::report::{Done, Reason};

/// What a step works with.
struct Ctx<'a, 'b> {
    ports: &'b LaunchdPorts<'a>,
    label: &'b str,
    /// The label of the job of `tierstore fs`, when the params name one.
    fs: Option<&'b str>,
    /// Whether an update may hand over to the next image: the staged and the previous release
    /// are programs the hook can ask. A package that carries no binary is not.
    releases: bool,
    /// Whether this step began to write `stopping`: from then on, a failure leaves the commit
    /// undecided until the record on disk says otherwise.
    commit_written: Cell<bool>,
}

impl Ctx<'_, '_> {
    fn look(&self) -> Look<'_> {
        Look {
            instances: self.ports.instances,
            clock: self.ports.clock,
        }
    }

    /// Stores `next` and makes it the record.
    fn save(&self, record: &mut Record, next: Record) -> Result<(), Done> {
        self.ports.state.save(&next.encode())?;
        *record = next;
        Ok(())
    }

    /// Loads the job's plist into the system domain.
    fn bootstrap(&self) -> Result<(), Done> {
        Ok(self.ports.launchd.bootstrap(Path::new(LAUNCHD_PLIST))?)
    }
}

/// Refuses params that name another release or version than the update on record.
fn same_update(record: &Record, params: &Params) -> Result<(), Done> {
    if record.is_of(params) {
        return Ok(());
    }
    Err(another_update(record, params))
}

/// The refusal of params that name another update than the one on record.
fn another_update(record: &Record, params: &Params) -> Done {
    Done::fail(
        Reason::BadParams,
        format!(
            "the params name {} {}, and the update on record is {:?} {:?}",
            params.staged_release.display(),
            params.target_version,
            record.to.as_deref(),
            record.version.as_deref(),
        ),
    )
}

/// A `prepare` that finds another update past its cutover, or a `rollback` that finds another
/// update in flight, neither committed, whose updater is gone ([`Stale::Own`]): that update is
/// rolled back first with the params it was written under, then `step` runs on the record at
/// rest. Both details are said, the rollback's first.
fn roll_back_first(
    c: &Ctx<'_, '_>,
    step: Step,
    params: &Params,
    record: &mut Record,
    own: &Written,
) -> Result<Done, Done> {
    let whose = format!(
        "the record is of the update to {} {}, not {} {}",
        own.params.staged_release.display(),
        own.params.target_version,
        params.staged_release.display(),
        params.target_version
    );
    let back = roll_back_own(c, own, record).map_err(|done| {
        led_by(
            done,
            &format!("{whose}, and its rollback with its own target and releases is not done"),
        )
    })?;
    let note = format!(
        "{whose}: rolled back with its own target and releases ({})",
        back.report.detail.as_deref().unwrap_or_default()
    );
    let done = if step == Step::Prepare {
        prepare(c, params, record)
    } else {
        rollback(c, params, record)
    };
    done.map(|done| led_by(done, &note))
        .map_err(|done| led_by(done, &note))
}

/// The rollback of the update on record with the params it was written under. A job that is
/// loaded and already serves the release that update started from (the boot step loaded it
/// again, with the binary `current` names) has nothing of that update running: the binary is
/// put back, the fs job is booted in when it is not loaded, and only the record changes. The
/// staged release of that update is then not read, so a release directory that is gone does not
/// matter. Any other state is `rollback`'s with those params.
fn roll_back_own(c: &Ctx<'_, '_>, own: &Written, record: &mut Record) -> Result<Done, Done> {
    let version = own.previous.version.as_str();
    if c.ports.launchd.loaded(c.label)? && c.look().observed(version).is_ok() {
        c.ports.host.install_binary(&own.previous.release)?;
        c.start_fs()?;
        c.save(record, Record::idle(None))?;
        return Ok(Done::ok(format!(
            "{} already serves {version}: nothing of that update runs",
            c.label
        )));
    }
    rollback(c, &own.params, record)
}

/// `done` with `first` and `; ` before its detail.
fn led_by(mut done: Done, first: &str) -> Done {
    let rest = done.report.detail.take().unwrap_or_default();
    done.report.detail = Some(format!("{first}; {rest}"));
    done
}

/// Refuses a record that names a systemd instance or a mode.
fn launchd_record(record: &Record) -> Result<(), Done> {
    if record.live.is_some() || record.next.is_some() || record.mode.is_some() {
        return Err(Done::fail(
            Reason::StateInvalid,
            "the record names a systemd instance or mode: this host runs launchd",
        ));
    }
    Ok(())
}

/// Runs `step` for the job `label`; every update is a restart. `params` is the parsed params file.
/// This is the arm of a package that carries no binary (a configuration package), whose releases
/// are not programs to ask.
#[must_use]
pub(super) fn run_step(step: Step, params: &Params, label: &str, ports: &LaunchdPorts<'_>) -> Done {
    run_step_with(step, params, label, ports, false)
}

/// [`run_step`], where `releases` says that the staged and the previous release are programs of
/// the service: an update between releases that both list `handoff-exec-v1` is then a hand-over
/// (see [`exec`]).
#[must_use]
pub(super) fn run_step_with(
    step: Step,
    params: &Params,
    label: &str,
    ports: &LaunchdPorts<'_>,
    releases: bool,
) -> Done {
    match attempt(step, params, label, ports, releases) {
        Ok((mut done, committed)) => {
            done.report.reversible &= !committed;
            done
        }
        Err(done) => done,
    }
}

/// Takes the lock, reads the record and runs the step; the second value is whether the record,
/// as the step left it, says `stopping`.
fn attempt(
    step: Step,
    params: &Params,
    label: &str,
    ports: &LaunchdPorts<'_>,
    releases: bool,
) -> Result<(Done, bool), Done> {
    let _lock = ports.state.lock()?;
    let mut record = match ports.state.load()? {
        Some(bytes) => Record::decode(&bytes)?,
        None => Record::idle(None),
    };
    record.check()?;
    launchd_record(&record)?;
    let fs = match params.fs_launchd_label() {
        Some(fs) if !(releases && volume::serves_its_mount(ports, params)?) => Some(fs),
        _ => None,
    };
    let c = Ctx {
        ports,
        label,
        fs,
        releases,
        commit_written: Cell::new(false),
    };
    let done = match step {
        Step::Prepare => prepare(&c, params, &mut record),
        Step::Cutover => cutover(&c, params, &mut record),
        Step::Verify => verify(&c, params, &mut record),
        Step::Rollback => rollback(&c, params, &mut record),
        Step::Cleanup => cleanup(&c, params, &mut record),
        Step::Uninstall => uninstall(&c, &mut record),
        Step::Restore => restore(&c, params),
    }
    .unwrap_or_else(std::convert::identity);
    let mut committed = record.commits(step, params);
    // Not committed after the write of `stopping` began means the write failed, and so did the
    // step: a write that succeeds commits the record.
    if c.commit_written.get() && !committed {
        // The write of `stopping` failed, and may have reached the disk all the same (a rename
        // made, its sync failed): the record on disk says what was decided. A record that cannot
        // be read leaves it undecided, which counts as committed: the updater then goes forward
        // with `cleanup`, which finishes a commit or makes it, and never rolls back one.
        committed = ports
            .state
            .load()
            .ok()
            .flatten()
            .is_none_or(|bytes| Record::decode(&bytes).map_or(true, |r| r.committed()));
    }
    Ok((done, committed))
}

fn prepare(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if record.updating && !record.markers.is_empty() {
        if let Stale::Own(own) = record.stale(params) {
            return roll_back_first(c, Step::Prepare, params, record, &own);
        }
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    let Some(config) = &params.config_file else {
        return Err(Done::fail(
            Reason::BadParams,
            "prepare needs the params key config_file",
        ));
    };
    let Some(text) = c.ports.host.config(config)? else {
        return Err(Done::fail(
            Reason::ConfigMissing,
            format!("there is no configuration file at {}", config.display()),
        ));
    };
    c.one_process(config, &text)?;
    // The way is not recorded: `cutover` and `rollback` decide it again from the same inputs.
    let hands_over = c.plan(params, config, &text)?;
    let from = params.previous.as_ref().map(|p| p.release.clone());
    let next = Record {
        updating: true,
        from: from.clone(),
        to: Some(params.staged_release.clone()),
        version: Some(params.target_version.clone()),
        ..Record::idle(None)
    };
    c.save(record, next)?;
    Ok(Done::ok(match from {
        Some(from) => format!(
            "prepared {} to follow {}: {} {} in cutover",
            params.staged_release.display(),
            from.display(),
            c.label,
            if hands_over {
                "hands over to the next image of its process"
            } else {
                "is restarted"
            }
        ),
        None => format!(
            "prepared the first install of {}: {} is loaded in cutover",
            params.staged_release.display(),
            c.label
        ),
    }))
}

fn cutover(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if !record.updating || record.committed() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows a prepare and precedes the commit",
        ));
    }
    same_update(record, params)?;
    let target = &params.target_version;
    let loaded = c.ports.launchd.loaded(c.label)?;
    // A run after the job was loaded (a crash, or a second run) looks first: the job may already
    // serve the target, and an update restarts it only when it does not.
    let seen = loaded && c.look().observed(target).is_ok();
    if !seen && loaded && c.hands_over(params)? {
        // The process of the loaded job executes the binary that is now at the plist's program.
        c.ports.host.install_binary(&params.staged_release)?;
        c.ports.launchd.hand_to_next_image(c.label)?;
        c.look().serves(target)?;
    } else if !seen {
        // The file service goes first, and the step goes no further while its mount stays: on
        // an update, and on a first install, which takes over a copy installed by hand that
        // left its jobs loaded. Nothing is stopped when the fs job is not loaded.
        c.stop_fs()?;
        // The plist runs the copy at `SERVICE_BINARY`: it becomes the staged release's first.
        c.ports.host.install_binary(&params.staged_release)?;
        // A loaded job that does not serve the target runs another copy (on a first install,
        // one installed by hand), or none: it is booted out, so that the bootstrap reads the
        // plist that is in place now.
        if record.from.is_some() || loaded {
            c.ports.launchd.bootout(c.label)?;
        }
        c.bootstrap()?;
        c.look().serves(target)?;
    }
    c.start_fs()?;
    if !record.has(Marker::Swapped) {
        let swapped = record.with(Marker::Swapped);
        c.save(record, swapped)?;
    }
    Ok(Done::ok(format!("{} serves {target}", c.label)))
}

fn verify(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if record.updating || record.committed() {
        same_update(record, params)?;
    }
    let target = &params.target_version;
    if record.committed() {
        let mode = c.look().observed(target)?;
        return Ok(verified(mode, target));
    }
    if !record.updating || !record.has(Marker::Swapped) {
        return Err(Done::fail(Reason::OutOfOrder, "verify follows a cutover"));
    }
    if !c.ports.launchd.loaded(c.label)? {
        return Err(Done::fail(
            Reason::NotReady,
            format!("{} is not loaded", c.label),
        ));
    }
    let observed = c.look().observed(target)?;
    let mode = writable(observed, c.label, params.verify_retry_secs)?;
    c.volume_served(params)?;
    // The writer binds the mount provider's socket, and the volume answers: the provider must
    // now do its job.
    let mounted = mount_check::on_node(c.ports.host, c.ports.provider, params)?;
    // `verified` follows its act: everything above was observed.
    if !record.has(Marker::Verified) {
        let marked = record.with(Marker::Verified);
        c.save(record, marked)?;
    }
    Ok(verified(mode, target).led_by(mounted))
}

fn rollback(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if let Some(detail) = record.nothing_to_roll_back(params) {
        return Ok(Done::ok(detail));
    }
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be undone",
        ));
    }
    if !record.updating {
        return Ok(Done::ok("no update in flight: nothing to roll back"));
    }
    match record.stale(params) {
        Stale::Not => same_update(record, params)?,
        Stale::Own(own) => return roll_back_first(c, Step::Rollback, params, record, &own),
        Stale::Kept(why) => {
            let mut done = another_update(record, params);
            let said = done.report.detail.take().unwrap_or_default();
            done.report.detail = Some(format!("{said}: {why}"));
            return Err(done);
        }
    }
    if record.from.is_none() {
        // A first install: what `cutover` started is stopped, and there is nothing to start in
        // its place; the copy of the binary goes too. The update stays on record, without
        // markers, so that a re-run says the same and a later `prepare` may start afresh.
        c.stop_fs()?;
        c.ports.launchd.bootout(c.label)?;
        c.ports.host.remove_binary()?;
        let mut cleared = record.clone();
        cleared.markers.clear();
        c.save(record, cleared)?;
        return Err(Done::fail(
            Reason::NoPreviousRelease,
            format!("nothing to go back to: {} is stopped", c.label),
        ));
    }
    let Some(previous) = &params.previous else {
        return Err(Done::fail(
            Reason::BadParams,
            "the update on record has a previous release and the params name none",
        ));
    };
    // `current` names the previous release again; its binary goes where the plist runs it, so
    // the job that is loaded now runs it.
    c.stop_fs()?;
    c.ports.host.install_binary(&previous.release)?;
    // A new image that failed may have left the job with no process to signal (launchd is
    // between two of its starts): the signal that launchctl refuses is then a restart.
    let handed = c.hands_over(params)?
        && c.ports.launchd.loaded(c.label)?
        && c.ports.launchd.hand_to_next_image(c.label).is_ok();
    if !handed {
        c.ports.launchd.bootout(c.label)?;
        c.bootstrap()?;
    }
    c.look().serves(&previous.version)?;
    c.start_fs()?;
    c.save(record, Record::idle(None))?;
    Ok(Done::ok(format!(
        "rolled back: {} serves {}",
        c.label, previous.version
    )))
}

fn cleanup(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if !record.has(Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "no verify has succeeded: cleanup does not commit on its own authority",
        ));
    }
    same_update(record, params)?;
    if !record.committed() {
        let confirmed =
            c.ports.launchd.loaded(c.label)? && c.look().observed(&params.target_version).is_ok();
        if !confirmed {
            return Err(Done::fail(
                Reason::NotConfirmed,
                format!("{} is not seen to serve {}", c.label, params.target_version),
            ));
        }
        let stopping = record.with(Marker::Stopping);
        c.commit_written.set(true);
        c.save(record, stopping)?;
    }
    if record.updating {
        let mut ended = record.clone();
        ended.updating = false;
        ended.from = None;
        c.save(record, ended)?;
    }
    Ok(Done::ok(format!(
        "{} serves {} alone",
        c.label, params.target_version
    )))
}

fn uninstall(c: &Ctx<'_, '_>, record: &mut Record) -> Result<Done, Done> {
    c.stop_fs()?;
    c.ports.launchd.bootout(c.label)?;
    c.ports.host.remove_binary()?;
    c.save(record, Record::idle(None))?;
    Ok(Done::ok(format!("{} is stopped and unloaded", c.label)))
}

/// The boot step: a job that is loaded is left as it is; otherwise the binary of the release
/// `current` names (the params' staged release, at boot) is put where the plist runs it, and the
/// job is loaded. The fs job, when the params name one and it is not loaded, is loaded after the
/// main job.
fn restore(c: &Ctx<'_, '_>, params: &Params) -> Result<Done, Done> {
    if !c.ports.launchd.loaded(c.label)? {
        c.ports.host.install_binary(&params.staged_release)?;
        c.bootstrap()?;
    }
    c.start_fs()?;
    Ok(Done::ok(format!("{} is loaded", c.label)))
}
