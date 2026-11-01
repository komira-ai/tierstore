//! `prepare <artifact>`: everything before anything visible.
//!
//! Chooses the mode from the capabilities of the staged release and of the one the current slot
//! links to; defers (75) if the host cannot take the update; records the update; points the idle
//! slot's link at the staged release; and, in handover mode, starts the idle slot and waits for
//! it to answer `/readyz` read-only. In restart mode nothing is started: the old instance has no
//! lock, and a second one would write beside it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{Ctx, HANDOVER_CAPABILITY, IDLE_BOUND_SECS, IDLE_MARGIN_SECS, READY_ATTEMPTS};
use crate::server::update_hook::declaration::{PUBLIC_PORT, unit_name};
use crate::server::update_hook::params::Params;
use crate::server::update_hook::probe::CacheMode;
use crate::server::update_hook::record::{Mode, Record, SCHEMA};
use crate::server::update_hook::report::{Done, Reason};
use crate::server::update_hook::ruleset::guard_ruleset;

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
    let mode = if both
        .iter()
        .all(|caps| caps.iter().any(|cap| cap == HANDOVER_CAPABILITY))
    {
        Mode::Handover
    } else {
        Mode::Restart
    };
    let retry = params.defer_retry_secs;
    let ct = c.ports.host.conntrack()?;
    if ct.tcp_timeout_established_secs <= IDLE_BOUND_SECS + IDLE_MARGIN_SECS {
        return Err(Done::deferred(
            Reason::HostNotReady,
            "nf_conntrack_tcp_timeout_established is not longer than the idle bound plus a margin",
            retry,
        ));
    }
    if ct.count.saturating_mul(100) >= ct.max.saturating_mul(params.conntrack_percent) {
        return Err(Done::deferred(
            Reason::HostNotReady,
            "the conntrack table is nearly full",
            retry,
        ));
    }
    if let Err(why) = c.ports.nft.apply(&guard_ruleset(PUBLIC_PORT, c.decl.ports)) {
        return Err(Done::deferred(
            Reason::HostNotReady,
            format!("the guard table cannot be loaded: {why}"),
            retry,
        ));
    }
    let next = Record {
        schema: SCHEMA,
        serving: idle,
        previous: Some(current),
        mode,
        markers: BTreeSet::new(),
        boot_id: None,
        guard_placed: false,
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
        "prepared slot {idle} beside {current}, mode {}",
        mode.as_str()
    )))
}
