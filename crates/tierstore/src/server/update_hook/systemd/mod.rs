//! The steps of a host whose service runs as two instances of one systemd template,
//! `tierstore@a.service` and `tierstore@b.service`, over the ports of [`SystemdPorts`].
//!
//! Both instances run [`SERVICE_BINARY`](super::SERVICE_BINARY) and serve port 9300; an instance
//! starts only when its marker file is there (`ConditionPathExists=` of the template), so at boot
//! only the recorded one does. The hook alternates: the instance that serves is `live`, the other
//! is `next`, and an update starts `next`. hostkeeper moves the link `current` before `cutover`
//! and back before `rollback`; the hook never moves it, and puts the binary of the release it
//! names at `SERVICE_BINARY` before it starts an instance (in `cutover`, `rollback` and
//! `restore`; [`Host::install_binary`](super::Host::install_binary)), so an instance that starts
//! runs whichever release `current` names at that moment.
//!
//! - `prepare` checks that the node can take the update (a configuration that names
//!   `server.handoff_socket`, unless the params force a restart), asks the staged and the
//!   previous binary what they can do (`handoff-socket-v1`, and `handoff-cache-v1` for a
//!   configuration with a `[local_cache]` table), picks the mode (hand-over when both can, restart
//!   otherwise) and records the update. Nothing is started.
//! - `cutover` starts `next`: in a hand-over it takes the listening sockets from `live`, which
//!   drains and exits on its own; in a restart `live` is stopped first; on a first install
//!   there is no `live`. It then waits for the staged release to serve, and records `swapped`.
//!   A failure is reported and nothing is undone here: hostkeeper runs `rollback`.
//! - `verify` observes the staged release on the public port and records `verified`; after a
//!   hand-over of the fs process it also needs the new fs process to run the installed binary,
//!   and on a node with a mount provider the mount check to pass (`mount_check`).
//! - `rollback` brings the previous release back on the instance that served it (or stops the
//!   only instance of a first install) and records nothing in flight. Before the commit only.
//! - `cleanup` confirms afresh (it waits for `/readyz`, which the old instance, still draining
//!   on the same sockets, answers 503), records `stopping` (the commit), removes the old
//!   instance's marker and records that `next` is now `live`, in the same run: it does not wait
//!   for the old instance to end, which it does by itself, in a time the hook does not know.
//! - `uninstall` stops both instances and removes both markers and the copy of the binary; the
//!   data stays.
//! - `restore`, the boot step, makes the markers say what the record says and starts that
//!   instance; the old instance of a hand-over that still drains beside it is left to end.
//!
//! A record committed and still in flight (a `cleanup` that ended before it finished, or one of
//! an earlier release that gave up while the old instance drained) is finished by the next
//! `cleanup`, `prepare` or `restore`, whichever comes first. A record of another update, in flight
//! and not committed, that returns the host to the params' previous release is one whose updater
//! is gone: a `prepare` that finds it past its cutover, and a `rollback`, roll it back first with
//! the params it was written under, then do their own step (`Record::stale`).
//!
//! The second unit, `tierstore-fs@a` and `tierstore-fs@b` ([`fs`]), is handed over in the same
//! steps on a node where an instance of it runs, between releases that both list
//! `handoff-fs-v1`: `cutover` starts it after the store (the new instance takes the mount from the
//! old one), `verify` and `cleanup` need it active, `verify` needs its process to run the binary
//! the update installed ([`fs_exe`]) and names the release of an fs process it left alone,
//! `cleanup` removes the old instance's marker and neither waits for it nor stops it, `rollback`
//! puts it back before the store, `restore` starts the recorded instance after the store and
//! `uninstall` stops the instances that run.
//! The config hook runs the steps with [`FsPolicy::LeftAlone`], or, for the switch of a node to one
//! process, [`FsPolicy::Switch`]: `prepare` records the switch (`fs_switch`, with `fs_live` naming
//! the instance that runs), `cutover` removes both fs markers once the new instance serves and
//! starts and stops no fs unit, `cleanup` records no fs instance, and `rollback` starts the fs
//! instance and has it take the mount before the store's way back.
//!
//! Every step holds the lock of the state directory, holds the record to its rules before it
//! acts, and sets state rather than doing things once, so a step run again after a crash
//! converges. `swapped` and `verified` follow their acts; `stopping` precedes its act.

mod boot;
mod cleanup;
mod cutover;
mod fs;
mod fs_exe;
mod prepare;
mod rollback;
mod verify;

use std::cell::Cell;
use std::convert::identity;

use super::args::Step;
use super::look::Look;
use super::params::Params;
use super::ports::{Instance, Lock, SystemdPorts};
use super::record::{Record, Stale};
use super::report::{Done, Reason};

/// Whether the steps hand the fs process over to the other instance with the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FsPolicy {
    /// The fs process is handed over when both releases list `handoff-fs-v1`.
    Managed,
    /// The fs process is left alone, running the release it runs: the update is of the store's
    /// configuration, and the fs process's own file is not in that package.
    LeftAlone,
    /// The update is the switch of a node to one process: the staged file has an `fs`
    /// interface, the file that serves had none, and the fs process that runs is not handed
    /// over but given up. The new instance of the store takes the mount from it; once that
    /// instance serves, both markers of the fs unit are removed, so that it never starts again,
    /// and no fs unit is started or stopped. A rollback starts the fs instance again, and the
    /// mount goes back to it before the store's sockets do.
    Switch,
}

/// What a step works with.
struct Ctx<'a, 'b> {
    ports: &'b SystemdPorts<'a>,
    /// What the steps do to the fs process.
    fs: FsPolicy,
    /// Whether the record as it is on disk, as far as this step knows, says `stopping`.
    committed: Cell<bool>,
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

    /// Stores `record`, and remembers whether it is the commit.
    fn save(&self, record: &Record) -> Result<(), Done> {
        self.ports.state.save(&record.encode())?;
        self.committed.set(record.committed());
        Ok(())
    }
}

/// How long hostkeeper waits before it runs a step again that waits for an instance to drain.
const DRAIN_RETRY_SECS: u64 = 10;

/// The unit's name as the details spell it.
fn unit_name(instance: Instance) -> String {
    format!("tierstore@{instance}")
}

/// The instance the update in flight starts.
fn next_of(record: &Record) -> Result<Instance, Done> {
    record.next.ok_or_else(|| {
        Done::fail(
            Reason::StateInvalid,
            "the record of the update in flight names no instance to start",
        )
    })
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

/// Refuses params of another update, while one is in flight or committed.
fn of_this_update(record: &Record, params: &Params) -> Result<(), Done> {
    if record.updating || record.committed() {
        same_update(record, params)?;
    }
    Ok(())
}

/// Whether the record, as a step of the update `params` name finds or leaves it, is that update's
/// commit ([`Record::commits`]), with the two cases this arm adds: a committed record still in
/// flight that a `prepare` finds is the last update's, which it finishes before it records its
/// own, and a `rollback` of another update finds nothing of that one committed.
fn commits(record: &Record, step: Step, params: &Params) -> bool {
    match step {
        Step::Prepare => false,
        Step::Rollback => record.committed() && record.is_of(params),
        _ => record.commits(step, params),
    }
}

/// Takes the lock and reads the record, which holds to its rules. `uninstall` reads none: it
/// replaces the record whatever it says.
fn open(step: Step, ports: &SystemdPorts<'_>) -> Result<(Lock, Option<Record>), Done> {
    let lock = ports.state.lock()?;
    if step == Step::Uninstall {
        return Ok((lock, None));
    }
    let Some(bytes) = ports.state.load()? else {
        return Ok((lock, None));
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    Ok((lock, Some(record)))
}

/// Runs `step`. `params` is the parsed params file.
pub(super) fn run_step(step: Step, params: &Params, ports: &SystemdPorts<'_>) -> Done {
    run_step_with(step, params, ports, FsPolicy::Managed)
}

/// Runs `step` with `fs` saying what it does to the fs process.
pub(super) fn run_step_with(
    step: Step,
    params: &Params,
    ports: &SystemdPorts<'_>,
    fs: FsPolicy,
) -> Done {
    let (_lock, record) = match open(step, ports) {
        Ok(opened) => opened,
        Err(done) => return done,
    };
    // A step after `prepare` goes by what the record says: the switch it noted.
    let fs = match &record {
        Some(record) if step != Step::Prepare && record.fs_switch => FsPolicy::Switch,
        _ => fs,
    };
    let c = Ctx {
        ports,
        fs,
        // The record as it stands says whether the update is committed, at rest included
        // ([`Record::commits`]).
        committed: Cell::new(record.as_ref().is_some_and(|r| commits(r, step, params))),
        commit_written: Cell::new(false),
    };
    let mut done = dispatch(&c, step, params, record.as_ref()).unwrap_or_else(identity);
    // Not committed after the write of `stopping` began means the write failed, and so did the
    // step: a write that succeeds commits the record.
    if c.commit_written.get() && !c.committed.get() {
        // The write of `stopping` failed, and may have reached the disk all the same (a rename
        // made, its sync failed): the record on disk says what was decided. A record that cannot
        // be read leaves it undecided, which counts as committed: hostkeeper then goes forward
        // with `cleanup`, which finishes a commit or makes it, and never rolls back one.
        let on_disk = ports
            .state
            .load()
            .ok()
            .flatten()
            .map(|b| Record::decode(&b));
        let committed = on_disk.and_then(Result::ok).is_none_or(|r| r.committed());
        c.committed.set(committed);
    }
    done.report.reversible &= !c.committed.get();
    done
}

/// The step itself, over the record as it was read.
fn dispatch(
    c: &Ctx<'_, '_>,
    step: Step,
    params: &Params,
    record: Option<&Record>,
) -> Result<Done, Done> {
    if let Some(record) = record
        && let Some(done) = past_a_stale(c, step, params, record)
    {
        return done;
    }
    match (step, record) {
        (Step::Prepare, record) => prepare::prepare(c, params, record),
        (Step::Restore, record) => boot::restore(c, params, record),
        (Step::Uninstall, _) => boot::uninstall(c),
        (Step::Rollback, None) => Ok(Done::ok("nothing to roll back: no update is recorded")),
        (_, None) => Err(Done::fail(
            Reason::OutOfOrder,
            "no update is recorded: prepare comes first",
        )),
        (Step::Cutover, Some(record)) => {
            of_this_update(record, params)?;
            cutover::cutover(c, params, record)
        }
        (Step::Verify, Some(record)) => {
            of_this_update(record, params)?;
            verify::verify(c, params, record)
        }
        (Step::Rollback, Some(record)) => {
            if let Some(detail) = record.nothing_to_roll_back_over(params, true) {
                return Ok(Done::ok(detail));
            }
            of_this_update(record, params)?;
            rollback::rollback(c, params, record)
        }
        (Step::Cleanup, Some(record)) => {
            of_this_update(record, params)?;
            cleanup::cleanup(c, params, record)
        }
    }
}

/// A `prepare` that finds another update past its cutover, and a `rollback` that finds another
/// update in flight, neither committed, whose updater is gone ([`Stale::Own`]): that update is
/// rolled back first, by `rollback` with the params it was written under, and the step then runs
/// on the record at rest. `None` lets the step go by its own rules: a record of the update the
/// params name, at rest, or committed (a committed one is finished, never rolled back), a
/// `prepare` over a record whose `cutover` never swapped (it prepares over it, as before), a
/// `cutover`, `verify` or `cleanup` (hostkeeper runs them only after a `prepare` of their own
/// update succeeded, so another update's record there is a second order racing the first), and a
/// record that [`Stale::Kept`] keeps, which a `rollback` refuses with why.
fn past_a_stale(
    c: &Ctx<'_, '_>,
    step: Step,
    params: &Params,
    record: &Record,
) -> Option<Result<Done, Done>> {
    let asked = match step {
        Step::Prepare => !record.markers.is_empty(),
        Step::Rollback => true,
        _ => false,
    };
    if !asked {
        return None;
    }
    match record.stale(params) {
        Stale::Not => None,
        Stale::Kept(why) => (step == Step::Rollback).then(|| {
            let mut done = another_update(record, params);
            let said = done.report.detail.take().unwrap_or_default();
            done.report.detail = Some(format!("{said}: {why}"));
            Err::<Done, Done>(done)
        }),
        Stale::Own(own) => Some(roll_back_first(c, step, params, record, &own.params)),
    }
}

/// Rolls the update on record back with `own`, the params it was written under, then runs `step`
/// on the record at rest. Both details are said, the rollback's first.
fn roll_back_first(
    c: &Ctx<'_, '_>,
    step: Step,
    params: &Params,
    record: &Record,
    own: &Params,
) -> Result<Done, Done> {
    let whose = format!(
        "the record is of the update to {} {}, not {} {}",
        own.staged_release.display(),
        own.target_version,
        params.staged_release.display(),
        params.target_version
    );
    let back = rollback::rollback(c, own, record).map_err(|done| {
        led_by(
            done,
            &format!("{whose}, and its rollback with its own target and releases is not done"),
        )
    })?;
    let note = format!(
        "{whose}: rolled back with its own target and releases ({})",
        back.report.detail.as_deref().unwrap_or_default()
    );
    // `rollback` saved the record at rest.
    let at_rest = Record::decode(&c.ports.state.load()?.unwrap_or_default())?;
    dispatch(c, step, params, Some(&at_rest))
        .map(|done| led_by(done, &note))
        .map_err(|done| led_by(done, &note))
}

/// `done` with `first` and `; ` before its detail.
fn led_by(mut done: Done, first: &str) -> Done {
    let rest = done.report.detail.take().unwrap_or_default();
    done.report.detail = Some(format!("{first}; {rest}"));
    done
}
