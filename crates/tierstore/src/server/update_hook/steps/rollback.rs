//! `rollback`: valid only before `cleanup` has written `stopping`.
//!
//! v-current is started if it is not running and the redirect is pointed at it; then the
//! instance the restored rule does not name is stopped and its slot link removed; the record
//! goes back to no update in flight. After `stopping` the step changes nothing and exits 1.

use super::Ctx;
use crate::server::update_hook::declaration::unit_name;
use crate::server::update_hook::record::Record;
use crate::server::update_hook::report::{Done, Reason};

pub(super) fn run(c: &Ctx<'_, '_>, record: &mut Record) -> Result<Done, Done> {
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun to stop the previous instance: nothing can be undone",
        ));
    }
    let Some(current) = record.previous else {
        return Ok(Done::ok("no update in flight: nothing to roll back"));
    };
    let idle = record.serving;
    c.back_to_current(record)?;
    c.ports.units.stop(&unit_name(idle))?;
    c.ports.slots.remove(idle)?;
    let mut back = Record::idle(current);
    back.mode = record.mode;
    c.save(&back)?;
    *record = back;
    Ok(Done::ok(format!(
        "traffic goes to {current}; {idle} is stopped"
    )))
}
