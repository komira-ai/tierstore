//! `rollback`: not written yet (the next change). The tests of the step are the specification.

use super::Ctx;
use crate::server::update_hook::record::Record;
use crate::server::update_hook::report::{Done, Reason};

pub(super) fn run(_c: &Ctx<'_, '_>, _record: &mut Record) -> Result<Done, Done> {
    Err(Done::fail(
        Reason::NotImplemented,
        "rollback is not written yet",
    ))
}
