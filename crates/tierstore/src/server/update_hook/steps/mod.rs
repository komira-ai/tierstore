//! The bodies of the steps, over the ports.
//!
//! Every step takes the lock of the state directory, reads the declaration and the record, holds
//! them to [`validate`](super::validate) before any `nft`, unit or link call, and then sets
//! state: what it does depends on the record and on what it sees, not on the order the updater
//! called the steps in, so a re-run after a crash at any point converges. After `cleanup` has
//! written `stopping`, every report says `"reversible":false`.

mod cleanup;
mod cutover;
mod prepare;
mod restore;
mod rollback;
mod verify;

use std::time::Duration;

use super::args::Step;
use super::declaration::{Declaration, PUBLIC_PORT, unit_name};
use super::params::Params;
use super::ports::{PortError, Ports};
use super::probe::{CacheMode, build_of, readiness};
use super::record::Record;
use super::report::{Done, Reason};
use super::ruleset::{FORWARD_TABLE, redirect_ruleset};
use super::validate::{check_params, validate};

/// The capability that says a release holds the cache lock, serves read-only beside another
/// instance, and drains with a lingering close.
pub const HANDOVER_CAPABILITY: &str = "handover-v1";

/// The time between two looks at an instance.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How many looks `prepare` and a restart-mode `cutover` give an instance to become ready.
pub const READY_ATTEMPTS: u32 = 120;
/// How many looks through the public port a probe makes.
pub const PROBE_ATTEMPTS: u32 = 60;
/// How many answers in a row by the new instance a probe needs.
pub const PROBE_HITS: u32 = 3;
/// The longest the service keeps an idle connection, in seconds (`header_timeout`).
pub const IDLE_BOUND_SECS: u64 = 60;
/// What `nf_conntrack_tcp_timeout_established` must exceed the idle bound by, in seconds.
pub const IDLE_MARGIN_SECS: u64 = 60;

/// What a step works with.
pub(super) struct Ctx<'a, 'b> {
    pub ports: &'b Ports<'a>,
    pub decl: &'b Declaration,
}

impl Ctx<'_, '_> {
    /// One look at `/readyz` of the instance on `port`.
    fn ready(&self, port: u16) -> Option<CacheMode> {
        readiness(&self.ports.instances.get(port, "/readyz"))
    }

    /// Up to `attempts` looks at `/readyz`, a pause between them.
    fn wait_ready(&self, port: u16, attempts: u32) -> Option<CacheMode> {
        for _ in 0..attempts {
            if let Some(mode) = self.ready(port) {
                return Some(mode);
            }
            self.ports.clock.pause(POLL_INTERVAL);
        }
        None
    }

    /// Whether one request through the public port was answered by the instance on `slot`
    /// running `version`.
    fn answered_by(&self, slot: u16, version: &str) -> bool {
        match self.ports.instances.get(PUBLIC_PORT, "/metricz") {
            Ok(reply) if reply.status == 200 => {
                build_of(&reply.body).is_some_and(|build| build.is(slot, version))
            }
            _ => false,
        }
    }

    /// Whether [`PROBE_HITS`] requests in a row through the public port were answered by the
    /// instance on `slot` running `version`, within [`PROBE_ATTEMPTS`] looks.
    fn probe_public(&self, slot: u16, version: &str) -> bool {
        let mut streak = 0;
        for _ in 0..PROBE_ATTEMPTS {
            streak = if self.answered_by(slot, version) {
                streak + 1
            } else {
                0
            };
            if streak >= PROBE_HITS {
                return true;
            }
            self.ports.clock.pause(POLL_INTERVAL);
        }
        false
    }

    /// Stores `record`.
    fn save(&self, record: &Record) -> Result<(), Done> {
        Ok(self.ports.state.save(&record.encode())?)
    }

    /// Points the public port at `port`, in one transaction.
    fn redirect(&self, port: u16) -> Result<(), PortError> {
        self.ports.nft.apply(&redirect_ruleset(PUBLIC_PORT, port))
    }

    /// The port the redirect table names now.
    fn redirected(&self) -> Result<Option<u16>, PortError> {
        Ok(self
            .ports
            .nft
            .list_table(FORWARD_TABLE)?
            .and_then(|listing| super::ruleset::redirect_target(&listing)))
    }

    /// Puts v-current back in front: starts its unit (a no-op when it runs) and points the
    /// redirect at it.
    fn back_to_current(&self, record: &Record) -> Result<(), PortError> {
        let current = record.current();
        self.ports.units.start(&unit_name(current))?;
        self.redirect(current)
    }
}

/// Runs `step`. `params` is the parsed params file; the five steps need it, `restore` does not.
#[must_use]
pub fn run_step(step: Step, params: Option<&Params>, ports: &Ports<'_>) -> Done {
    match attempt(step, params, ports) {
        Ok((mut done, committed)) => {
            done.report.reversible &= !committed;
            done
        }
        Err(done) => done,
    }
}

/// Opens the session and runs the step; the second value is whether the record, as the step left
/// it, says `stopping`.
fn attempt(step: Step, params: Option<&Params>, ports: &Ports<'_>) -> Result<(Done, bool), Done> {
    let _lock = ports.state.lock()?;
    let decl = Declaration::parse(&ports.host.declaration()?)?;
    if let Some(params) = params {
        check_params(params, &decl)?;
    }
    let mut record = match ports.state.load()? {
        Some(bytes) => Some(Record::decode(&bytes)?),
        None => None,
    };
    if let Some(record) = &record {
        validate(record, &decl)?;
    }
    let ctx = Ctx { ports, decl: &decl };
    let done = match (step, params, record.as_mut()) {
        (Step::Restore, _, record) => restore::run(&ctx, record.as_deref()),
        (_, _, None) => Err(Done::fail(
            Reason::StateInvalid,
            "there is no record: the first install lays it down",
        )),
        (Step::Rollback, _, Some(record)) => rollback::run(&ctx, record),
        (Step::Cleanup, _, Some(record)) => cleanup::run(&ctx, record),
        (_, None, Some(_)) => Err(Done::fail(
            Reason::BadParams,
            "this step needs the params file",
        )),
        (Step::Prepare, Some(params), Some(record)) => prepare::run(&ctx, params, record),
        (Step::Cutover, Some(params), Some(record)) => cutover::run(&ctx, params, record),
        (Step::Verify, Some(params), Some(record)) => verify::run(&ctx, params, record),
    }
    .unwrap_or_else(std::convert::identity);
    Ok((done, record.is_some_and(|r| r.committed())))
}
