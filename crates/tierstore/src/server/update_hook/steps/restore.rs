//! `restore`, the boot entry (not one of the five steps): applies the guard and the redirect
//! for one slot and starts that slot's unit; the other slot is not started.
//!
//! The slot: with no update in flight, `serving`; with one in flight and no `stopping`, the
//! previous slot (a reboot before the commit is an abort); with `stopping`, `serving`. The
//! record is not changed: the updater follows its step table for the step the reboot cut short.
//! With no record the host has never been set up and there is nothing to restore.

use super::Ctx;
use crate::server::update_hook::declaration::unit_name;
use crate::server::update_hook::record::Record;
use crate::server::update_hook::report::{Done, Reason};
use crate::server::update_hook::ruleset::guard_ruleset;

pub(super) fn run(c: &Ctx<'_, '_>, record: Option<&Record>) -> Result<Done, Done> {
    let Some(record) = record else {
        return Ok(Done::ok("no record: nothing to restore"));
    };
    let slot = if record.committed() {
        record.serving
    } else {
        record.current()
    };
    if c.ports.slots.target(slot)?.is_none() {
        return Err(Done::fail(
            Reason::NoSlot,
            format!("the slot {slot} has no link"),
        ));
    }
    c.ports
        .nft
        .apply(&guard_ruleset(c.ports.public_port, c.decl.ports))?;
    c.redirect(slot)?;
    c.ports.units.start(&unit_name(slot))?;
    Ok(Done::ok(format!("restored slot {slot}")))
}
