//! `cutover`: the visible step.
//!
//! Handover mode: the idle instance must still answer `/readyz`; the redirect is applied in one
//! transaction; requests through the public port must be answered by the new instance with the
//! staged version; a probe that never succeeds puts the redirect back and exits 1. Restart mode:
//! v-current is stopped and v-next started first. The step records `swapped` last. A second run
//! only re-applies the redirect.

use super::{Ctx, READY_ATTEMPTS};
use crate::server::update_hook::declaration::unit_name;
use crate::server::update_hook::params::Params;
use crate::server::update_hook::record::{Marker, Mode, Record};
use crate::server::update_hook::report::{Done, Reason};

/// The visible part: make v-next ready, point the redirect at it and watch it answer.
fn swap(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<(), Done> {
    let next = record.serving;
    let swapped = record.markers.contains(&Marker::Swapped);
    if !swapped {
        let attempts = if record.mode == Mode::Restart {
            c.ports.units.stop(&unit_name(record.current()))?;
            c.ports.units.start(&unit_name(next))?;
            READY_ATTEMPTS
        } else {
            1
        };
        if c.wait_ready(next, attempts).is_none() {
            return Err(Done::fail(
                Reason::NotReady,
                format!("the instance on {next} does not answer /readyz"),
            ));
        }
    }
    c.redirect(next)?;
    if !swapped && !c.probe_public(next, &params.target_version) {
        return Err(Done::fail(
            Reason::ProbeFailed,
            format!("no request through the public port was answered by {next}"),
        ));
    }
    Ok(())
}

pub(super) fn run(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if record.committed() || record.previous.is_none() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows a prepare and precedes the commit",
        ));
    }
    if let Err(done) = swap(c, params, record) {
        // The old instance goes back in front, whatever failed; a failure here is not news.
        let _ = c.back_to_current(record);
        return Err(done);
    }
    let mut next = record.clone();
    next.markers.insert(Marker::Swapped);
    c.save(&next)?;
    *record = next;
    Ok(Done::ok(format!("traffic goes to {}", record.serving)))
}
