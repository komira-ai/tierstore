//! `cutover`: starts the instance `prepare` chose, and waits for the staged release to serve.

use super::super::params::Params;
use super::super::record::{Marker, Mode, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, next_of, unit_name};

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
    if swapped && c.ports.units.state(next)?.running() && c.look().observed(target).is_ok() {
        return Ok(Done::ok(format!(
            "{} already serves {target}",
            unit_name(next)
        )));
    }
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
    c.look().serves(target)?;
    // `swapped` follows its act: the probe above saw the new release serve.
    if !swapped {
        c.save(&record.with(Marker::Swapped))?;
    }
    Ok(Done::ok(format!(
        "{} serves {target} ({how})",
        unit_name(next)
    )))
}
