//! `verify`: the gate, run again and again; it only observes, and sets `verified`.
//!
//! Before the commit (handover mode): v-next is ready, the redirect names it, requests through
//! the public port are answered by it with the staged version, and v-current still serves; the
//! step succeeds with `promotion_pending`, since v-current holds the lock. In restart mode there
//! is no v-current, and the step waits as it does after the commit. After the commit it waits
//! for `read-write` up to `promotion_bound_secs`. There is no ceiling here: the updater applies
//! its own, and this step never fails for it.

use super::{Ctx, POLL_INTERVAL};
use crate::server::update_hook::params::Params;
use crate::server::update_hook::probe::CacheMode;
use crate::server::update_hook::record::{Marker, Mode, Record};
use crate::server::update_hook::report::{Done, Reason};

/// Polls `/readyz` of `port` until it is not read-only (true) or `bound_secs` have passed on the
/// clock (false); an instance that does not answer is an error.
fn wait_promoted(c: &Ctx<'_, '_>, port: u16, bound_secs: u64) -> Result<bool, Done> {
    let deadline = c
        .ports
        .clock
        .now_ms()
        .saturating_add(bound_secs.saturating_mul(1000));
    loop {
        match c.ready(port) {
            None => {
                return Err(Done::fail(
                    Reason::NotReady,
                    format!("the instance on {port} does not answer /readyz"),
                ));
            }
            Some(CacheMode::ReadOnly) => {}
            Some(_) => return Ok(true),
        }
        if c.ports.clock.now_ms() >= deadline {
            return Ok(false);
        }
        c.ports.clock.pause(POLL_INTERVAL);
    }
}

/// A success that says whether the promotion is still pending. The detail says that it is, and
/// not who holds the cache lock: no instance says that, and after the commit the previous one
/// may be gone while the lock is still not taken.
fn observed(promoted: bool, port: u16) -> Done {
    let mut done = Done::ok(if promoted {
        format!("{port} is read-write")
    } else {
        format!("{port} serves read-only: the promotion is pending")
    });
    done.report.promotion_pending = !promoted;
    done
}

pub(super) fn run(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    let next = record.serving;
    let version = &params.target_version;
    if record.committed() {
        if !c.probe_public(next, version) {
            return Err(Done::fail(
                Reason::WrongBuild,
                format!("the public port is not answered by {next} with the staged version"),
            ));
        }
        let promoted = wait_promoted(c, next, params.promotion_bound_secs)?;
        return Ok(observed(promoted, next));
    }
    if record.previous.is_none() || !record.markers.contains(&Marker::Swapped) {
        return Err(Done::fail(Reason::OutOfOrder, "verify follows a cutover"));
    }
    if c.ready(next).is_none() {
        return Err(Done::fail(
            Reason::NotReady,
            format!("the instance on {next} does not answer /readyz"),
        ));
    }
    if c.redirected()? != Some(next) {
        return Err(Done::fail(
            Reason::RedirectWrong,
            format!("the redirect does not name {next}"),
        ));
    }
    if !c.probe_public(next, version) {
        return Err(Done::fail(
            Reason::WrongBuild,
            format!("the public port is not answered by {next} with the staged version"),
        ));
    }
    let promoted = if record.mode == Mode::Handover {
        let current = record.current();
        if c.ready(current).is_none() {
            return Err(Done::fail(
                Reason::PreviousDown,
                format!("the previous instance on {current} no longer answers /readyz"),
            ));
        }
        false
    } else {
        wait_promoted(c, next, params.promotion_bound_secs)?
    };
    // `verified` follows its act: everything above was observed in this boot, which is recorded
    // with it. A later boot voids it until a `verify` looks again.
    let boot = c.ports.host.boot_id()?;
    // `boot_id` is only ever present with `verified` (the validator holds the record to it), so a
    // record without `verified` has no boot id and differs from this boot too.
    if record.boot_id.as_deref() != Some(boot.as_str()) {
        let mut verified = record.clone();
        verified.markers.insert(Marker::Verified);
        verified.boot_id = Some(boot);
        c.save(&verified)?;
        *record = verified;
    }
    Ok(observed(promoted, next))
}
