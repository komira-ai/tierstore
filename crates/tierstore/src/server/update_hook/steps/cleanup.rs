//! `cleanup`: the commit point.
//!
//! Refuses (exit 1, `reversible:true`, nothing done) unless the record says `verified` and the
//! step, freshly, confirms what `verify` saw: the boot of the host is the one `verify` recorded
//! (a reboot sends `restore` to the previous slot, which voids `verified`), the guard table is
//! there, the redirect names `serving`, `serving` answers `/readyz`, and the public port is
//! answered by it with the staged version. Then: write `stopping` (it precedes its act: from
//! here every report says `"reversible":false`); remove the previous slot's link first, so that
//! its unit's `ConditionPathExists=` keeps a restart from bringing it back; stop the previous
//! unit (the drain); clear `previous`. No unit is masked: the hook has no such call. A re-run
//! goes forward from wherever the record says, without the confirmation: the commit is made.

use super::Ctx;
use crate::server::update_hook::declaration::unit_name;
use crate::server::update_hook::params::Params;
use crate::server::update_hook::record::{Marker, Record};
use crate::server::update_hook::report::{Done, Reason};
use crate::server::update_hook::ruleset::GUARD_TABLE;

fn unconfirmed(why: String) -> Done {
    Done::fail(Reason::NotConfirmed, why)
}

/// Looks again, in this step, at what the commit depends on.
fn confirm(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<(), Done> {
    let next = record.serving;
    if record.boot_id.as_deref() != Some(c.ports.host.boot_id()?.as_str()) {
        return Err(unconfirmed(
            "the host has booted since verify: verify again".to_owned(),
        ));
    }
    if c.ports.nft.list_table(GUARD_TABLE)?.is_none() {
        return Err(unconfirmed("the guard table is not loaded".to_owned()));
    }
    if c.redirected()? != Some(next) {
        return Err(unconfirmed(format!("the redirect does not name {next}")));
    }
    if c.ready(next).is_none() || !c.probe_public(next, &params.target_version) {
        return Err(unconfirmed(format!(
            "{next} is not seen to serve through the public port"
        )));
    }
    Ok(())
}

pub(super) fn run(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if !record.markers.contains(&Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "no verify has succeeded: cleanup does not commit on its own authority",
        ));
    }
    if !record.committed() {
        confirm(c, params, record)?;
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
        retired.guard_placed = false;
        c.save(&retired)?;
        *record = retired;
    }
    Ok(Done::ok(format!("{} serves alone", record.serving)))
}
