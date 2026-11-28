//! `uninstall` and `restore`: the steps that need no update in flight.

use super::super::params::Params;
use super::super::ports::{Instance, UnitState};
use super::super::record::{Marker, Record};
use super::super::report::Done;
use super::cleanup::finish;
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

/// The boot step: the instance the record names runs, the other does not start again, and the
/// markers say so. An update that swapped names `next`; every other record names `live`. Before
/// that instance is started, the binary of the release `current` names (the params' staged
/// release, at boot) is put where the units run it, as `cutover` and `rollback` leave it. A first
/// install that crashed before `swapped` has no `live`: restore records nothing and touches
/// nothing, and an instance the boot condition started from its marker is left to the rollback of
/// the update in flight.
///
/// hostkeeper runs this step at every start of its own, not only after a reboot, so it may find
/// the old instance of a finished hand-over still draining beside the new one. The other instance
/// is stopped only when the one the record names is not active; when it is, the other drains and
/// ends by itself and loses only its marker. A record whose update is committed and still in
/// flight (a `cleanup` that ended before it finished, or one of an earlier release that gave up
/// while the old instance drained) is finished here, as `cleanup` finishes it.
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
    let mut drains = Vec::new();
    if c.ports.units.state(other)?.running() {
        if c.ports.units.state(desired)? == UnitState::Active {
            drains.push(unit_name(other));
        } else {
            c.ports.units.stop(other)?;
        }
    }
    c.ports.markers.clear(other)?;
    c.ports.markers.set(desired)?;
    c.ports.host.install_binary(&params.staged_release)?;
    c.ports.units.start(desired)?;
    let mut said = format!("{} started", unit_name(desired));
    if let Some(committed) = record.filter(|r| r.updating && r.committed()) {
        finish(c, committed)?;
        said.push_str("; the committed update is finished");
    }
    for unit in drains {
        said.push_str(&format!("; {unit} still drains and ends by itself"));
    }
    Ok(Done::ok(said))
}
