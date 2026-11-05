//! `rollback`: puts the previous release back, before the commit. hostkeeper has moved `current`
//! back to it already, so an instance that starts runs it.

use super::super::params::Params;
use super::super::ports::Instance;
use super::super::record::{Mode, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, DRAIN_RETRY_SECS, next_of, unit_name};

pub(super) fn rollback(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<Done, Done> {
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be rolled back",
        ));
    }
    if !record.updating {
        return Ok(Done::ok("nothing to roll back: no update is in flight"));
    }
    let next = next_of(record)?;
    if record.from.is_none() {
        return first_install(c, record, next);
    }
    let previous = params.previous.as_ref().ok_or_else(|| {
        Done::fail(
            Reason::BadParams,
            "the update on record follows a release, and the params name none",
        )
    })?;
    let back = record.live.unwrap_or_else(|| next.other());
    let version = previous.version.as_str();
    if record.mode == Some(Mode::Handover) {
        hand_back(c, back, next, version)?;
    } else {
        c.ports.units.stop(next)?;
        start_back(c, back, version)?;
    }
    if c.ports.units.state(next)?.running() {
        return Err(draining(back, next));
    }
    c.ports.markers.clear(next)?;
    c.save(&Record::idle(Some(back)))?;
    Ok(Done::ok(format!(
        "rolled back: {} serves {version}",
        unit_name(back)
    )))
}

/// A first install has no release to go back to: what `cutover` started is stopped, and the
/// failure says so. The update stays on record with nothing done, so that a run again says the
/// same and a later `prepare` may start afresh.
fn first_install(c: &Ctx<'_, '_>, record: &Record, next: Instance) -> Result<Done, Done> {
    c.ports.units.stop(next)?;
    c.ports.markers.clear(next)?;
    if !record.markers.is_empty() {
        let mut bare = record.clone();
        bare.markers.clear();
        c.save(&bare)?;
    }
    Err(Done::fail(
        Reason::NoPreviousRelease,
        format!("nothing to go back to: stopped {}", unit_name(next)),
    ))
}

/// Starts `back`, which takes its marker first, and waits for `version` to serve.
fn start_back(c: &Ctx<'_, '_>, back: Instance, version: &str) -> Result<(), Done> {
    c.ports.markers.set(back)?;
    c.ports.units.start(back)?;
    c.look().serves(version)?;
    Ok(())
}

/// The way back of a hand-over: the instance `back` takes the sockets from `next`, unless it
/// still has them.
fn hand_back(c: &Ctx<'_, '_>, back: Instance, next: Instance, version: &str) -> Result<(), Done> {
    let next_up = c.ports.units.state(next)?.running();
    let back_up = c.ports.units.state(back)?.running();
    if !back_up {
        return start_back(c, back, version);
    }
    // `back` runs. With `next` up too, one of them drains. With `next` down, `back` either
    // still serves (the hand-over never happened) or hands over to an instance that died, and
    // exits.
    if next_up || c.look().observed(version).is_err() {
        return Err(draining(back, next));
    }
    Ok(())
}

fn draining(back: Instance, next: Instance) -> Done {
    Done::deferred(
        Reason::Draining,
        format!(
            "{} and {} both run, and one of them drains: the rollback waits",
            unit_name(back),
            unit_name(next)
        ),
        DRAIN_RETRY_SECS,
    )
}
