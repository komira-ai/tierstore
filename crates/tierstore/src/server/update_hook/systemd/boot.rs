//! `uninstall` and `restore`: the steps that need no update in flight.

use super::super::ports::Instance;
use super::super::record::{Marker, Record};
use super::super::report::Done;
use super::{Ctx, unit_name};

/// Stops both instances and removes both markers. The data directory, the configuration and the
/// record directory stay; the record says nothing is in flight and nothing serves.
pub(super) fn uninstall(c: &Ctx<'_, '_>) -> Result<Done, Done> {
    for instance in Instance::ALL {
        c.ports.units.stop(instance)?;
    }
    for instance in Instance::ALL {
        c.ports.markers.clear(instance)?;
    }
    c.save(&Record::idle(None))?;
    Ok(Done::ok(
        "tierstore@a and tierstore@b are stopped, their markers removed",
    ))
}

/// The boot step: the instance the record names runs, the other does not, and the markers say
/// so. An update that swapped names `next`; every other record names `live`. A first install
/// that crashed before `swapped` has no `live`: restore records nothing and touches nothing, and
/// an instance the boot condition started from its marker is left to the rollback of the update
/// in flight.
pub(super) fn restore(c: &Ctx<'_, '_>, record: Option<&Record>) -> Result<Done, Done> {
    let desired = record.and_then(|record| {
        if record.updating && record.has(Marker::Swapped) {
            record.next
        } else {
            record.live
        }
    });
    let Some(desired) = desired else {
        return Ok(Done::ok("nothing recorded: no instance is to run"));
    };
    let other = desired.other();
    if c.ports.units.state(other)?.running() {
        c.ports.units.stop(other)?;
    }
    c.ports.markers.clear(other)?;
    c.ports.markers.set(desired)?;
    c.ports.units.start(desired)?;
    Ok(Done::ok(format!("{} started", unit_name(desired))))
}
