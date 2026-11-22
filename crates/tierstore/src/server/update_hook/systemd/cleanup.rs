//! `cleanup`: the commit, and the end of the update on record.

use super::super::params::Params;
use super::super::ports::{Instance, UnitState};
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, fs, next_of, unit_name};

pub(super) fn cleanup(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<Done, Done> {
    if !record.updating {
        return if record.committed() {
            Ok(Done::ok(
                "nothing left to clean: the update is committed and finished",
            ))
        } else {
            Err(Done::fail(
                Reason::OutOfOrder,
                "cleanup follows a verify that succeeded",
            ))
        };
    }
    if !record.has(Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "cleanup follows a verify that succeeded",
        ));
    }
    let next = next_of(record)?;
    let committed = if record.committed() {
        record.clone()
    } else {
        confirm(c, record, next, &params.target_version)?;
        // `stopping` precedes its act: from here on nothing is reversible.
        let committed = record.with(Marker::Stopping);
        c.commit_written.set(true);
        c.save(&committed)?;
        committed
    };
    let (_, drains) = finish(c, &committed)?;
    Ok(Done::ok(format!(
        "committed: {} serves{}",
        unit_name(next),
        drain_note(&drains)
    )))
}

/// The end of a committed update, whatever its old instances are doing: the markers of the old
/// instances go and the record is saved at rest, `next` as `live` and `fs_next` as `fs_live`.
/// Returns the record at rest, and the names of the old units that still run.
///
/// An old instance of a hand-over is never stopped here: it ends by itself, in a time the hook
/// does not know (the store's within its drain deadline; an fs process of a release that frees
/// its whole state before it ends can take minutes). Nothing waits for it, since nothing of the update is
/// left to do once the new instances serve and the commit is written. Its marker goes while it
/// runs: systemd reads `ConditionPathExists=` only when it starts a unit, so a process that runs
/// is not touched, and without the marker the unit is never started again, by a boot or by
/// `Restart=`. Every later step looks at the units themselves: a `prepare` that finds both
/// instances of a template running waits for the drain, and the boot step leaves the old instance
/// to end. A crash in here leaves the record committed and in flight, which the next `cleanup`,
/// `prepare` or boot step finishes the same way.
pub(super) fn finish(c: &Ctx<'_, '_>, committed: &Record) -> Result<(Record, Vec<String>), Done> {
    let next = next_of(committed)?;
    let mut drains = Vec::new();
    if let Some(live) = committed.live {
        if c.ports.units.state(live)?.running() {
            drains.push(unit_name(live));
        }
        c.ports.markers.clear(live)?;
    }
    // The old fs instance of a hand-over ends by itself, and is never stopped here.
    if let Some(old) = fs::retire(c, committed)? {
        drains.push(fs::fs_name(old));
    }
    let mut finished = committed.clone();
    finished.updating = false;
    finished.from = None;
    finished.live = Some(next);
    finished.next = None;
    finished.mode = None;
    finished.fs_live = finished.fs_next.or(finished.fs_live);
    finished.fs_next = None;
    c.save(&finished)?;
    Ok((finished, drains))
}

/// What a step that finished an update says of the old instances that still drain.
pub(super) fn drain_note(drains: &[String]) -> String {
    match drains {
        [] => ", the old instance is retired".to_owned(),
        [one] => format!("; {one} still drains and ends by itself"),
        many => format!("; {} still drain and end by themselves", many.join(" and ")),
    }
}

/// Looks afresh at what `verify` saw: `next` runs and the staged release answers. In a
/// hand-over the old instance still drains on the same listening sockets and answers `/readyz`
/// with 503 until it exits, so a look that reaches it is no verdict: `confirm` waits for
/// `/readyz` as `cutover` does, then probes for the build.
fn confirm(c: &Ctx<'_, '_>, record: &Record, next: Instance, target: &str) -> Result<(), Done> {
    let not_confirmed = |why: String| Done::fail(Reason::NotConfirmed, why);
    let state = c.ports.units.state(next)?;
    if state != UnitState::Active {
        return Err(not_confirmed(format!(
            "{} is {state:?}, not active",
            unit_name(next)
        )));
    }
    // The fs instance the update started, too: active is a mount it has taken.
    if let Some(fs_next) = record.fs_next {
        let state = c.ports.fs_units.state(fs_next)?;
        if state != UnitState::Active {
            return Err(not_confirmed(format!(
                "{} is {state:?}, not active",
                fs::fs_name(fs_next)
            )));
        }
    }
    c.look().serves(target).map(|_| ()).map_err(|failed| {
        not_confirmed(format!(
            "{target} was not seen serving: {}",
            failed.report.detail.unwrap_or_default()
        ))
    })
}
