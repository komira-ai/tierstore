//! `cutover`: puts the staged release's binary where the units run it, starts the instance
//! `prepare` chose, and waits for the staged release to serve.

use super::super::params::Params;
use super::super::record::{Marker, Mode, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, fs, next_of, unit_name};

pub(super) fn cutover(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<Done, Done> {
    if !record.updating || record.committed() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows prepare, and comes before cleanup",
        ));
    }
    let next = next_of(record)?;
    let target = &params.target_version;
    let swapped = record.has(Marker::Swapped);
    // A run again after `swapped` looks first: when the new instance serves, nothing is left.
    if swapped
        && c.ports.units.state(next)?.running()
        && c.look().observed(target).is_ok()
        && fs::is_active(c, record)?
    {
        return Ok(Done::ok(format!(
            "{} already serves {target}{}",
            unit_name(next),
            fs::cutover_note(record),
        )));
    }
    // A record that `prepare` of the previous release's hook wrote has no plan of the fs process:
    // it is made here, before the first act of the step, and saved, so that a rollback and a run
    // again see it. The plan is not an act: a failure of it changes nothing.
    let planned = fs::plan_late(c, params, record)?;
    let (record, plan_note) = match &planned {
        Some((fresh, note)) => {
            if fresh != record {
                c.save(fresh)?;
            }
            (fresh, note.as_str())
        }
        None => (record, ""),
    };
    // The units run the copy at `SERVICE_BINARY`, which becomes the staged release's before
    // anything is stopped or started: a failure here leaves `live` serving. A `live` that runs
    // the old copy keeps it, and drains on it.
    c.ports.host.install_binary(&params.staged_release)?;
    let how = match (record.live, record.mode) {
        // The new instance takes the sockets; the live one drains and exits by itself.
        (Some(_), Some(Mode::Handover)) => "hand-over",
        (Some(live), _) => {
            c.ports.units.stop(live)?;
            "restart"
        }
        (None, _) => "start",
    };
    c.ports.markers.set(next)?;
    c.ports.units.start(next)?;
    let mode = c.look().serves(target)?;
    // The switch: the new process took the mount from the fs unit, which ends by itself; its
    // markers go so that it never starts again.
    if record.fs_switch {
        fs::give_up(c)?;
    }
    // The fs process after the store, as the unit orders itself: the new fs instance talks to the
    // new store, the pair a release is tested as. Not while that store is read-only: its fill
    // socket is not up yet (`fs::take`).
    let mut take_note = String::new();
    if let Some(fs_next) = record.fs_next {
        take_note = fs::take(c, fs_next, mode)?;
        // Both markers coexist only during the take.
        c.ports.fs_markers.clear(fs_next.other())?;
    }
    // `swapped` follows its acts: the probe above saw the new release serve, and the unit of
    // the fs process is active.
    if !swapped {
        c.save(&record.with(Marker::Swapped))?;
    }
    Ok(Done::ok(format!(
        "{} serves {target} ({how}){}{plan_note}{take_note}",
        unit_name(next),
        fs::cutover_note(record),
    )))
}
