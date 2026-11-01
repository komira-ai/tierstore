//! `prepare <artifact>`: everything before anything visible.
//!
//! Chooses the mode: `restart` when the params say `mode = "restart"`, and otherwise from the
//! capabilities of the staged release and of the one the current slot links to; defers (75) if
//! the host cannot take the update; records the update and why it chose the mode; points the idle
//! slot's link at the staged release; and, in handover mode, starts the idle slot and waits for
//! it to answer `/readyz` read-only. In restart mode nothing is started: the old instance has no
//! lock, and a second one would write beside it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{Ctx, HANDOVER_CAPABILITY, IDLE_BOUND_SECS, IDLE_MARGIN_SECS, READY_ATTEMPTS};
use crate::server::update_hook::declaration::{PUBLIC_PORT, unit_name};
use crate::server::update_hook::params::Params;
use crate::server::update_hook::probe::CacheMode;
use crate::server::update_hook::record::{Mode, ModeReason, Record, SCHEMA};
use crate::server::update_hook::report::{Done, Reason};
use crate::server::update_hook::ruleset::{GUARD_TABLE, guard_ruleset};

/// The release the link of `port` points at.
fn release_of(c: &Ctx<'_, '_>, port: u16) -> Result<PathBuf, Done> {
    c.ports
        .slots
        .target(port)?
        .ok_or_else(|| Done::fail(Reason::NoSlot, format!("the slot {port} has no link")))
}

pub(super) fn run(c: &Ctx<'_, '_>, params: &Params, record: &mut Record) -> Result<Done, Done> {
    if record.previous.is_some() && !record.markers.is_empty() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    let current = record.current();
    let idle = c.decl.other(current);
    let both = [
        c.ports.host.capabilities(&params.staged_release)?,
        c.ports.host.capabilities(&release_of(c, current)?)?,
    ];
    // The declaration's word wins over the capabilities: a `dir:` hot tier cannot have two
    // instances side by side, whatever the releases can do.
    let why = if params.mode == Some(Mode::Restart) {
        ModeReason::DeclaredRestart
    } else if both
        .iter()
        .all(|caps| caps.iter().any(|cap| cap == HANDOVER_CAPABILITY))
    {
        ModeReason::HandoverCapable
    } else {
        ModeReason::NotHandoverCapable
    };
    let mode = why.mode();
    let retry = params.defer_retry_secs;
    // Nothing is applied before the guard is in place: up to here the step has only looked. What
    // the guard does not cover is the private port of an instance, which starts after it.
    let existed = c.ports.nft.list_table(GUARD_TABLE)?.is_some();
    let ours = !existed || (record.previous.is_some() && record.guard_placed);
    if let Err(why) = c.ports.nft.apply(&guard_ruleset(PUBLIC_PORT, c.decl.ports)) {
        return Err(Done::deferred(
            Reason::HostNotReady,
            format!("the guard table cannot be loaded: {why}"),
            retry,
        ));
    }
    let ct = c.ports.host.conntrack()?;
    let refusal = if ct.tcp_timeout_established_secs <= IDLE_BOUND_SECS + IDLE_MARGIN_SECS {
        Some("nf_conntrack_tcp_timeout_established is not longer than the idle bound plus a margin")
    } else if ct.count.saturating_mul(100) >= ct.max.saturating_mul(params.conntrack_percent) {
        Some("the conntrack table is nearly full")
    } else {
        None
    };
    if let Some(why) = refusal {
        // A deferral leaves the host as it was: what this update placed is removed, what was
        // there before stays.
        if ours {
            c.ports.nft.delete_table(GUARD_TABLE)?;
        }
        return Err(Done::deferred(Reason::HostNotReady, why, retry));
    }
    // `guard_placed` follows the loading of the guard, and is written with the record.
    let next = Record {
        schema: SCHEMA,
        serving: idle,
        previous: Some(current),
        mode,
        mode_reason: Some(why),
        markers: BTreeSet::new(),
        boot_id: None,
        guard_placed: ours,
    };
    c.save(&next)?;
    *record = next;
    c.ports.slots.point(idle, &params.staged_release)?;
    if mode == Mode::Handover {
        c.ports.units.start(&unit_name(idle))?;
        match c.wait_ready(idle, READY_ATTEMPTS) {
            None => {
                return Err(Done::fail(
                    Reason::NotReady,
                    format!("the instance on {idle} never answered /readyz"),
                ));
            }
            Some(CacheMode::ReadWrite) => {
                return Err(Done::fail(
                    Reason::UnexpectedMode,
                    format!("the instance on {idle} is read-write beside a running one"),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(Done::ok(format!(
        "prepared slot {idle} beside {current}, mode {} ({})",
        mode.as_str(),
        why.as_str()
    )))
}
