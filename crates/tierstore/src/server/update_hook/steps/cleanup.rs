//! `cleanup`: the commit point.
//!
//! Refuses (exit 1, nothing done) unless the record says `verified`. Then: write `stopping`
//! (from here every report says `"reversible":false`); remove the previous slot's link first, so
//! that its unit's `ConditionPathExists=` keeps a restart or a manual start from bringing it
//! back; stop the previous unit (the drain); clear `previous`. No unit is masked: the hook has
//! no such call. A re-run goes forward from wherever the record says.

use super::Ctx;
use crate::server::update_hook::declaration::unit_name;
use crate::server::update_hook::record::{Marker, Record};
use crate::server::update_hook::report::{Done, Reason};

pub(super) fn run(c: &Ctx<'_, '_>, record: &mut Record) -> Result<Done, Done> {
    if !record.markers.contains(&Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "no verify has succeeded: cleanup does not commit on its own authority",
        ));
    }
    if !record.committed() {
        let mut stopping = record.clone();
        stopping.markers.insert(Marker::Stopping);
        c.save(&stopping)?;
        *record = stopping;
    }
    if let Some(previous) = record.previous {
        c.ports.slots.remove(previous)?;
        c.ports.units.stop(&unit_name(previous))?;
        let mut retired = record.clone();
        retired.previous = None;
        c.save(&retired)?;
        *record = retired;
    }
    Ok(Done::ok(format!("{} serves alone", record.serving)))
}
