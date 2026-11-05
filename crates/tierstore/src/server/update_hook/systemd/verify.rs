//! `verify`: observes that the staged release serves, and records it.

use super::super::look::verified;
use super::super::params::Params;
use super::super::ports::UnitState;
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, next_of, unit_name};

pub(super) fn verify(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<Done, Done> {
    let target = &params.target_version;
    // After the commit, `verify` watches the promotion: the build, and nothing of the units.
    if record.committed() {
        let mode = c.look().observed(target)?;
        return Ok(verified(mode, target));
    }
    if !record.updating || !record.has(Marker::Swapped) {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "verify follows a cutover that succeeded",
        ));
    }
    let next = next_of(record)?;
    let state = c.ports.units.state(next)?;
    if state != UnitState::Active {
        return Err(Done::fail(
            Reason::NotReady,
            format!("{} is {state:?}, not active", unit_name(next)),
        ));
    }
    let mode = c.look().observed(target)?;
    // `verified` follows its act: everything above was observed.
    if !record.has(Marker::Verified) {
        c.save(&record.with(Marker::Verified))?;
    }
    Ok(verified(mode, target))
}
