//! `uninstall` and `restore`: the steps that need no update in flight.

use super::super::params::Params;
use super::super::ports::Instance;
use super::super::record::{Marker, Record};
use super::super::report::Done;
use super::{Ctx, unit_name};

/// Stops both instances, removes both markers and the copy of the binary the units run. The data
/// directory, the configuration and the record directory stay; the record says nothing is in
/// flight and nothing serves.
pub(super) fn uninstall(c: &Ctx<'_, '_>) -> Result<Done, Done> {
    for instance in Instance::ALL {
        c.ports.units.stop(instance)?;
    }
    for instance in Instance::ALL {
        c.ports.markers.clear(instance)?;
    }
    c.ports.host.remove_binary()?;
    c.save(&Record::idle(None))?;
    Ok(Done::ok(
        "tierstore@a and tierstore@b are stopped, their markers and binary removed",
    ))
}

/// The boot step: the instance the record names runs, the other does not, and the markers say
/// so. An update that swapped names `next`; every other record names `live`. Before that
/// instance is started, the binary of the release `current` names (the params' staged release, at
/// boot) is put where the units run it, as `cutover` and `rollback` leave it. A first install
/// that crashed before `swapped` has no `live`: restore records nothing and touches nothing, and
/// an instance the boot condition started from its marker is left to the rollback of the update
/// in flight.
pub(super) fn restore(
    c: &Ctx<'_, '_>,
    params: &Params,
    record: Option<&Record>,
) -> Result<Done, Done> {
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
    c.ports.host.install_binary(&params.staged_release)?;
    c.ports.units.start(desired)?;
    Ok(Done::ok(format!("{} started", unit_name(desired))))
}
