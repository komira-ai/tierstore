//! The steps of a host whose service runs as two instances of one systemd template,
//! `tierstore@a.service` and `tierstore@b.service`, over the ports of [`SystemdPorts`].
//!
//! Both instances run `current/bin/tierstore` and serve port 9300; an instance starts only when
//! its marker file is there (`ConditionPathExists=` of the template), so at boot only the
//! recorded one does. The hook alternates: the instance that serves is `live`, the other is
//! `next`, and an update starts `next`. hostkeeper moves the link `current` before `cutover`
//! and back before `rollback`; the hook never moves it, so an instance that starts runs
//! whichever release `current` names at that moment.
//!
//! - `prepare` checks that the node can take the update (a configuration that names
//!   `server.handoff_socket`, unless the params force a restart), asks the staged and the
//!   previous binary what they can do, picks the mode (hand-over when both can, restart
//!   otherwise) and records the update. Nothing is started.
//! - `cutover` starts `next`: in a hand-over it takes the listening sockets from `live`, which
//!   drains and exits on its own; in a restart `live` is stopped first; on a first install
//!   there is no `live`. It then waits for the staged release to serve, and records `swapped`.
//!   A failure is reported and nothing is undone here: hostkeeper runs `rollback`.
//! - `verify` observes the staged release on the public port and records `verified`.
//! - `rollback` brings the previous release back on the instance that served it (or stops the
//!   only instance of a first install) and records nothing in flight. Before the commit only.
//! - `cleanup` confirms afresh, records `stopping` (the commit), waits for the old instance to
//!   have exited, removes its marker and records that `next` is now `live`.
//! - `uninstall` stops both instances and removes both markers; the data stays.
//! - `restore`, the boot step, makes the markers say what the record says and starts that
//!   instance.
//!
//! Every step holds the lock of the state directory, holds the record to its rules before it
//! acts, and sets state rather than doing things once, so a step run again after a crash
//! converges. `swapped` and `verified` follow their acts; `stopping` precedes its act.

mod boot;
mod cleanup;
mod cutover;
mod prepare;
mod rollback;
mod verify;

use std::cell::Cell;
use std::convert::identity;

use super::args::Step;
use super::look::Look;
use super::params::Params;
use super::ports::{Instance, Lock, SystemdPorts};
use super::record::Record;
use super::report::{Done, Reason};

/// What a step works with.
struct Ctx<'a, 'b> {
    ports: &'b SystemdPorts<'a>,
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

/// Refuses params of another update, while one is in flight or committed.
fn of_this_update(record: &Record, params: &Params) -> Result<(), Done> {
    if record.updating || record.committed() {
        same_update(record, params)?;
    }
    Ok(())
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
    let (_lock, record) = match open(step, ports) {
        Ok(opened) => opened,
        Err(done) => return done,
    };
    let c = Ctx {
        ports,
        // The record as it stands says whether the update is committed, at rest included; only a
        // `prepare` on a record at rest starts a new update, which nothing has committed yet.
        committed: Cell::new(
            record
                .as_ref()
                .is_some_and(|r| r.committed() && (r.updating || step != Step::Prepare)),
        ),
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
    match (step, record) {
        (Step::Prepare, record) => prepare::prepare(c, params, record),
        (Step::Restore, record) => boot::restore(c, record),
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
            of_this_update(record, params)?;
            rollback::rollback(c, params, record)
        }
        (Step::Cleanup, Some(record)) => {
            of_this_update(record, params)?;
            cleanup::cleanup(c, params, record)
        }
    }
}
