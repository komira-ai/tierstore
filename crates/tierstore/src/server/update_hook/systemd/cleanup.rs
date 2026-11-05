//! `cleanup`: the commit, and the retirement of the old instance.

use super::super::params::Params;
use super::super::ports::{Instance, UnitState};
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, DRAIN_RETRY_SECS, next_of, unit_name};

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
        confirm(c, next, &params.target_version)?;
        // `stopping` precedes its act: from here on nothing is reversible.
        let committed = record.with(Marker::Stopping);
        c.commit_written.set(true);
        c.save(&committed)?;
        committed
    };
    if let Some(live) = committed.live {
        if c.ports.units.state(live)?.running() {
            return Err(Done::deferred(
                Reason::Draining,
                format!("{} still drains: cleanup runs again", unit_name(live)),
                DRAIN_RETRY_SECS,
            ));
        }
        c.ports.markers.clear(live)?;
    }
    let mut finished = committed;
    finished.updating = false;
    finished.from = None;
    finished.live = Some(next);
    finished.next = None;
    finished.mode = None;
    c.save(&finished)?;
    Ok(Done::ok(format!(
        "committed: {} serves, the old instance is retired",
        unit_name(next)
    )))
}

/// Looks afresh at what `verify` saw: `next` runs and the staged release answers.
fn confirm(c: &Ctx<'_, '_>, next: Instance, target: &str) -> Result<(), Done> {
    let not_confirmed = |why: String| Done::fail(Reason::NotConfirmed, why);
    let state = c.ports.units.state(next)?;
    if state != UnitState::Active {
        return Err(not_confirmed(format!(
            "{} is {state:?}, not active",
            unit_name(next)
        )));
    }
    c.look().observed(target).map(|_| ()).map_err(|failed| {
        not_confirmed(format!(
            "{target} was not seen serving: {}",
            failed.report.detail.unwrap_or_default()
        ))
    })
}
