//! `verify`: observes that the staged release serves, and records it; before the commit, a new
//! instance whose cache is still read-only defers it ([`writable`]). On a node whose fs process
//! the update handed over, it also checks that the new fs instance runs the binary the update
//! installed ([`fs_exe`]); on one whose fs process it left alone, the detail says first which
//! release that process still runs.

use super::super::PUBLIC_PORT;
use super::super::look::{verified, writable};
use super::super::params::Params;
use super::super::ports::UnitState;
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, fs, fs_exe, next_of, unit_name};

pub(super) fn verify(c: &Ctx<'_, '_>, params: &Params, record: &Record) -> Result<Done, Done> {
    let target = &params.target_version;
    // After the commit, `verify` watches the promotion: the build, and nothing of the units. The
    // old instance may still drain, after `cleanup`, and answer `/readyz` with 503 on the sockets
    // the two share: a look it answers is no verdict, and hostkeeper asks again.
    if record.committed() {
        let mode = c
            .look()
            .observed(target)
            .map_err(|done| not_ready_yet(done, &format!("port {PUBLIC_PORT}"), params))?;
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
    match state {
        UnitState::Active => {}
        // The new instance waits for the cache lock the old one holds until the old one drains:
        // hostkeeper asks again, and ends the deferrals itself at `verify_deadline_secs`.
        UnitState::Activating => {
            return Err(Done::deferred(
                Reason::NotReady,
                format!("{} is still activating", unit_name(next)),
                params.verify_retry_secs,
            ));
        }
        other => {
            return Err(Done::fail(
                Reason::NotReady,
                format!("{} is {other:?}, not active", unit_name(next)),
            ));
        }
    }
    fs::check_active(c, record, params.verify_retry_secs)?;
    let fs_said = fs_exe::check(c, record, target)?;
    let observed = c
        .look()
        .observed(target)
        .map_err(|done| not_ready_yet(done, &unit_name(next), params))?;
    // Before the commit the new instance must be the cache's writer: until it is, the node's jobs
    // cannot fill their trees or reach the mount provider, and the node stays in its update.
    let mode = writable(observed, &unit_name(next), params.verify_retry_secs)?;
    // `verified` follows its act: everything above was observed.
    if !record.has(Marker::Verified) {
        c.save(&record.with(Marker::Verified))?;
    }
    Ok(led_by(verified(mode, target), fs_said))
}

/// `done` with `first` before its detail, when there is anything to say first.
fn led_by(mut done: Done, first: String) -> Done {
    if !first.is_empty() {
        let rest = done.report.detail.take().unwrap_or_default();
        done.report.detail = Some(format!("{first}; {rest}"));
    }
    done
}

/// `done` deferred when it is a look at `/readyz` that was not answered ready, so that hostkeeper
/// asks again (`verify_retry_secs`) and ends the deferrals itself at `verify_deadline_secs`: `what`
/// does not answer it yet. Any other failure is `done` as it is.
fn not_ready_yet(done: Done, what: &str, params: &Params) -> Done {
    if done.report.reason == Some(Reason::NotReady) {
        Done::deferred(
            Reason::NotReady,
            format!("{what} does not answer /readyz yet"),
            params.verify_retry_secs,
        )
    } else {
        done
    }
}
