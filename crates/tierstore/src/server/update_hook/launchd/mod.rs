//! The steps of the launchd arm: one job, whose label is `launchd_label` of the params, and
//! whose plist is [`LAUNCHD_PLIST`]. An update is a restart with a gap: no hand-over on a Mac.
//!
//! hostkeeper switches `current` before `cutover` and back before `rollback`, so booting the
//! plist out and in again starts the release `current` names; the hook never moves the link.
//! Every step takes the lock of the state directory, reads and checks the record, and sets state
//! rather than does things once, so that a run after a crash converges. A record that names a
//! systemd instance (`live`, `next` or `mode`) is not this arm's and is refused.
//!
//! - `prepare` needs the node's configuration file to exist (it does not need a hand-over
//!   socket), and records the update: `from` the previous release (none on a first install),
//!   `to` the staged release and its `version`. Nothing is started or stopped.
//! - `cutover` boots the job out (an update only) and in, waits for `/readyz` and for the build
//!   of the target version, and records `swapped`. A first install never boots out: it
//!   boots in when the job is not loaded. A run that finds the job loaded and serving the target
//!   only records `swapped`. A failure is reported; hostkeeper then runs `rollback`.
//! - `verify` observes the target version served by the loaded job and records `verified`. After
//!   the commit it only observes.
//! - `rollback` boots the job out and, for an update, in again, and waits for the previous
//!   version; a first install has nothing to go back to: the job stays out and the step fails
//!   with `no_previous_release`, again on every re-run.
//! - `cleanup` confirms afresh what `verify` saw, writes `stopping` (the commit) and ends the
//!   update. There is no instance to retire. When the write of `stopping` fails, the record is
//!   read back, so that a write that reached the disk is reported as the commit it is; a record
//!   that cannot be read back counts as committed.
//! - `uninstall` boots the job out and leaves the data, the cache and the configuration of the
//!   node, and the record directory, as they are.
//! - `restore`, the boot step, boots the job in when it is not loaded.

use std::cell::Cell;
use std::path::Path;

use super::LAUNCHD_PLIST;
use super::args::Step;
use super::look::{Look, verified};
use super::params::Params;
use super::ports::LaunchdPorts;
use super::record::{Marker, Record};
use super::report::{Done, Outcome, Reason};

/// What a step works with.
struct Ctx<'a, 'b> {
    ports: &'b LaunchdPorts<'a>,
    label: &'b str,
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
    let to = record.to.as_deref();
    let version = record.version.as_deref();
    if to == Some(params.staged_release.as_path()) && version == Some(&params.target_version) {
        return Ok(());
    }
    Err(Done::fail(
        Reason::BadParams,
        format!(
            "the params name {} {}, and the update on record is {to:?} {version:?}",
            params.staged_release.display(),
            params.target_version,
        ),
    ))
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

/// Runs `step` for the job `label`. `params` is the parsed params file.
#[must_use]
pub(super) fn run_step(step: Step, params: &Params, label: &str, ports: &LaunchdPorts<'_>) -> Done {
    match attempt(step, params, label, ports) {
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
) -> Result<(Done, bool), Done> {
    let _lock = ports.state.lock()?;
    let mut record = match ports.state.load()? {
        Some(bytes) => Record::decode(&bytes)?,
        None => Record::idle(None),
    };
    record.check()?;
    launchd_record(&record)?;
    let c = Ctx {
        ports,
        label,
        commit_written: Cell::new(false),
    };
    let done = match step {
        Step::Prepare => prepare(&c, params, &mut record),
        Step::Cutover => cutover(&c, params, &mut record),
        Step::Verify => verify(&c, params, &mut record),
        Step::Rollback => rollback(&c, params, &mut record),
        Step::Cleanup => cleanup(&c, params, &mut record),
        Step::Uninstall => uninstall(&c, &mut record),
        Step::Restore => restore(&c),
    }
    .unwrap_or_else(std::convert::identity);
    let mut committed = record.committed();
    if c.commit_written.get() && !committed && done.outcome != Outcome::Success {
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
    if c.ports.host.config(config)?.is_none() {
        return Err(Done::fail(
            Reason::ConfigMissing,
            format!("there is no configuration file at {}", config.display()),
        ));
    }
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
            "prepared {} to follow {}: {} is restarted in cutover",
            params.staged_release.display(),
            from.display(),
            c.label
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
    if !seen {
        if record.from.is_some() {
            c.ports.launchd.bootout(c.label)?;
            c.bootstrap()?;
        } else if !loaded {
            c.bootstrap()?;
        }
        c.look().serves(target)?;
    }
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
    let mode = c.look().observed(target)?;
    // `verified` follows its act: everything above was observed.
    if !record.has(Marker::Verified) {
        let marked = record.with(Marker::Verified);
        c.save(record, marked)?;
    }
    Ok(verified(mode, target))
}

fn rollback(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be undone",
        ));
    }
    if !record.updating {
        return Ok(Done::ok("no update in flight: nothing to roll back"));
    }
    same_update(record, params)?;
    if record.from.is_none() {
        // A first install: what `cutover` started is stopped, and there is nothing to start in
        // its place. The update stays on record, without markers, so that a re-run says the
        // same and a later `prepare` may start afresh.
        c.ports.launchd.bootout(c.label)?;
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
    // `current` names the previous release again, so the job that is loaded now runs it.
    c.ports.launchd.bootout(c.label)?;
    c.bootstrap()?;
    c.look().serves(&previous.version)?;
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
    c.ports.launchd.bootout(c.label)?;
    c.save(record, Record::idle(None))?;
    Ok(Done::ok(format!("{} is stopped and unloaded", c.label)))
}

fn restore(c: &Ctx<'_, '_>) -> Result<Done, Done> {
    if c.ports.launchd.loaded(c.label)? {
        return Ok(Done::ok(format!("{} is loaded", c.label)));
    }
    c.bootstrap()?;
    Ok(Done::ok(format!("{} is loaded", c.label)))
}
