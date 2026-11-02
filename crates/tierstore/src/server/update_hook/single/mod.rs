//! The steps of a host that runs one instance under its service manager (launchd on macOS), over
//! the ports of [`SinglePorts`]: a plain restart, with a gap in serving while the service
//! restarts.
//!
//! The contract is the two-slot hook's: the same five steps and the boot entry, the same params,
//! report, exit statuses and markers, a record ([`SingleRecord`]) that every step holds to its
//! rules before it acts, a lock of the state directory for the whole step, and steps that set
//! state, so that a re-run after a crash at any point converges. What differs is the machine.
//! The service runs the binary that the release link `current` points at, on one port of
//! loopback, so:
//!
//! - `prepare` records the update and points the link `previous` at the release `current` names.
//!   Nothing is started. The two-slot `prepare`'s guard table, its connection-tracking check and
//!   its second instance have nothing to act on here, and are skipped ([`SKIPPED_PREPARE`]).
//! - `cutover` stops the service, points `current` at the staged release and starts the service,
//!   then waits for `/readyz` and for [`PROBE_HITS`](super::steps::PROBE_HITS) answers in a row
//!   from the staged version on the port. A failure puts `previous` back as `current`, restarts
//!   the service and reports the failure with the words `rolled back`; the update stays on
//!   record, for the updater's `rollback` to clear. There is no redirect to move
//!   ([`SKIPPED_CUTOVER`]).
//! - `verify` looks at `current`, `/readyz` and the build on the port, waits for the promotion
//!   as a restart-mode `verify` does, and records `verified`.
//! - `rollback` points `current` at `previous` again (when it is not already), restarts the
//!   service, removes `previous` and clears the update. Valid only before the commit.
//! - `cleanup` confirms afresh what `verify` saw, writes `stopping` (the commit), removes
//!   `previous` and clears the update. There is no previous instance to stop
//!   ([`SKIPPED_CLEANUP`]).
//! - `restore`, the boot entry, starts the service if it is not running. launchd starts the job
//!   at boot by itself (`RunAtLoad`), so this is a no-op there.
//!
//! No step makes an nftables, slot or slot-unit call: [`SinglePorts`] has none to make.

mod record;

pub use record::{Kind, SingleRecord};

use std::path::{Path, PathBuf};

use super::args::Step;
use super::declaration::LaunchdDeclaration;
use super::params::Params;
use super::ports::{Link, PortError, SinglePorts};
use super::record::Marker;
use super::report::{Done, Reason};
use super::steps::READY_ATTEMPTS;
use super::steps::look::{Look, observed};
use super::validate::Invalid;

/// What `prepare` skips, which the two-slot `prepare` does: in its detail.
pub const SKIPPED_PREPARE: &str =
    "skipped: the guard table, the conntrack check and a second instance (one instance)";
/// What `cutover` skips, which the two-slot `cutover` does: in its detail.
pub const SKIPPED_CUTOVER: &str = "skipped: the redirect (one instance on its own port)";
/// What `cleanup` skips, which the two-slot `cleanup` does: in its detail.
pub const SKIPPED_CLEANUP: &str = "skipped: stopping a previous instance (there is none)";

/// What a step works with.
struct Ctx<'a, 'b> {
    ports: &'b SinglePorts<'a>,
    port: u16,
}

impl Ctx<'_, '_> {
    fn look(&self) -> Look<'_> {
        Look {
            instances: self.ports.instances,
            clock: self.ports.clock,
        }
    }

    /// Whether the instance on the port answers `/readyz` within [`READY_ATTEMPTS`] looks and
    /// then [`PROBE_HITS`](super::steps::PROBE_HITS) answers in a row from `version`.
    fn serves(&self, version: &str) -> Result<(), Done> {
        let port = self.port;
        if self.look().wait_ready(port, READY_ATTEMPTS).is_none() {
            return Err(Done::fail(
                Reason::NotReady,
                format!("the instance on {port} does not answer /readyz"),
            ));
        }
        if !self.look().probe(port, port, version) {
            return Err(Done::fail(
                Reason::ProbeFailed,
                format!("no request to {port} was answered by {version}"),
            ));
        }
        Ok(())
    }

    /// Whether the build on the port is `version`: one look at `/readyz`, then the probe.
    fn observed_build(&self, version: &str) -> Result<(), Done> {
        let port = self.port;
        if self.look().ready(port).is_none() {
            return Err(Done::fail(
                Reason::NotReady,
                format!("the instance on {port} does not answer /readyz"),
            ));
        }
        if !self.look().probe(port, port, version) {
            return Err(Done::fail(
                Reason::WrongBuild,
                format!("the port {port} is not answered by {version}"),
            ));
        }
        Ok(())
    }

    /// The release `link` points at.
    fn target(&self, link: Link) -> Result<Option<PathBuf>, PortError> {
        self.ports.releases.target(link)
    }

    /// Whether `current` points at `staged`.
    fn current_is(&self, staged: &Path) -> Result<bool, PortError> {
        Ok(self.target(Link::Current)?.as_deref() == Some(staged))
    }

    /// Stops the service, points `current` at `release` and starts the service.
    fn restart_on(&self, release: &Path) -> Result<(), PortError> {
        self.ports.service.stop()?;
        self.ports.releases.point(Link::Current, release)?;
        self.ports.service.start()
    }

    /// Stores `record`.
    fn save(&self, record: &SingleRecord) -> Result<(), Done> {
        Ok(self.ports.state.save(&record.encode())?)
    }

    /// Puts the release `previous` names back as `current`, restarting the service when `current`
    /// named another, and otherwise makes sure the service runs. Returns that release, when
    /// `previous` names one.
    fn restore_previous(&self) -> Result<Option<PathBuf>, PortError> {
        let previous = self.target(Link::Previous)?;
        match &previous {
            Some(release) if self.target(Link::Current)?.as_ref() != Some(release) => {
                self.restart_on(release)?;
            }
            _ => self.ports.service.start()?,
        }
        Ok(previous)
    }
}

/// Runs `step`. `params` is the parsed params file; the five steps need it, `restore` does not.
#[must_use]
pub fn run_step(step: Step, params: Option<&Params>, ports: &SinglePorts<'_>) -> Done {
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
fn attempt(
    step: Step,
    params: Option<&Params>,
    ports: &SinglePorts<'_>,
) -> Result<(Done, bool), Done> {
    let _lock = ports.state.lock()?;
    let decl = LaunchdDeclaration::parse(&ports.host.declaration()?)?;
    if let Some(params) = params
        && params.staged_release.starts_with(&decl.state_dir)
    {
        return Err(Invalid::Staged.into());
    }
    let mut record = match ports.state.load()? {
        Some(bytes) => Some(SingleRecord::decode(&bytes)?),
        None => None,
    };
    if let Some(record) = &record {
        record.check()?;
    }
    let c = Ctx {
        ports,
        port: decl.port,
    };
    let done = match (step, params, record.as_mut()) {
        (Step::Restore, _, record) => restore(&c, record.as_deref()),
        (_, _, None) => Err(Done::fail(
            Reason::StateInvalid,
            "there is no record: the first install lays it down",
        )),
        (Step::Rollback, _, Some(record)) => rollback(&c, record),
        (_, None, Some(_)) => Err(Done::fail(
            Reason::BadParams,
            "this step needs the params file",
        )),
        (Step::Prepare, Some(params), Some(record)) => prepare(&c, params, record),
        (Step::Cutover, Some(params), Some(record)) => cutover(&c, params, record),
        (Step::Verify, Some(params), Some(record)) => verify(&c, params, record),
        (Step::Cleanup, Some(params), Some(record)) => cleanup(&c, params, record),
    }
    .unwrap_or_else(std::convert::identity);
    Ok((done, record.is_some_and(|r| r.committed())))
}

fn prepare(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if record.updating && !record.markers.is_empty() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    let current = c.target(Link::Current)?.ok_or_else(|| {
        Done::fail(
            Reason::NoSlot,
            "the link current names no release: the first install lays it down",
        )
    })?;
    // A re-run keeps the `previous` an earlier run made: by then `current` may already be the
    // staged release. A fresh update replaces whatever a crash left there.
    let keep = record.updating && c.target(Link::Previous)?.is_some();
    let next = SingleRecord::updating();
    c.save(&next)?;
    *record = next;
    if !keep {
        c.ports.releases.point(Link::Previous, &current)?;
    }
    Ok(Done::ok(format!(
        "prepared {} to follow {}, mode restart (single_instance); {SKIPPED_PREPARE}",
        params.staged_release.display(),
        current.display()
    )))
}

fn cutover(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if !record.updating || record.committed() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows a prepare and precedes the commit",
        ));
    }
    let staged = &params.staged_release;
    if record.has(Marker::Swapped) {
        // A second run: the swap is made; only the service's running is set again.
        c.ports.service.start()?;
    } else {
        let Some(previous) = c.target(Link::Previous)? else {
            return Err(Done::fail(
                Reason::OutOfOrder,
                "the link previous names no release: prepare first",
            ));
        };
        let swapped = c
            .restart_on(staged)
            .map_err(Done::from)
            .and_then(|()| c.serves(&params.target_version));
        if let Err(mut done) = swapped {
            // The previous release goes back in front, whatever failed; the update stays on
            // record, for the updater's `rollback` to clear (or a second `cutover` to retry).
            let back = match c.restore_previous() {
                Ok(_) => format!("rolled back: the service runs {} again", previous.display()),
                Err(failed) => format!("the rollback to {} failed: {failed}", previous.display()),
            };
            let detail = done.report.detail.take().unwrap_or_default();
            done.report.detail = Some(format!("{detail}; {back}"));
            return Err(done);
        }
        let next = record.with(Marker::Swapped);
        c.save(&next)?;
        *record = next;
    }
    Ok(Done::ok(format!(
        "the service runs {} on {}; {SKIPPED_CUTOVER}",
        staged.display(),
        c.port
    )))
}

fn verify(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    let version = &params.target_version;
    let bound = params.promotion_bound_secs;
    if record.committed() {
        if !c.look().probe(c.port, c.port, version) {
            return Err(Done::fail(
                Reason::WrongBuild,
                format!("the port {} is not answered by {version}", c.port),
            ));
        }
        let promoted = c.look().wait_promoted(c.port, bound)?;
        return Ok(observed(promoted, c.port));
    }
    if !record.updating || !record.has(Marker::Swapped) {
        return Err(Done::fail(Reason::OutOfOrder, "verify follows a cutover"));
    }
    if !c.current_is(&params.staged_release)? {
        return Err(Done::fail(
            Reason::WrongBuild,
            format!(
                "the link current does not name {}",
                params.staged_release.display()
            ),
        ));
    }
    c.observed_build(version)?;
    let promoted = c.look().wait_promoted(c.port, bound)?;
    // `verified` follows its act: everything above was observed.
    if !record.has(Marker::Verified) {
        let verified = record.with(Marker::Verified);
        c.save(&verified)?;
        *record = verified;
    }
    Ok(observed(promoted, c.port))
}

fn rollback(c: &Ctx<'_, '_>, record: &mut SingleRecord) -> Result<Done, Done> {
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be undone",
        ));
    }
    if !record.updating {
        return Ok(Done::ok("no update in flight: nothing to roll back"));
    }
    let previous = c.restore_previous()?;
    c.ports.releases.remove(Link::Previous)?;
    // The record is cleared last: a crash before it re-runs every part above.
    let idle = SingleRecord::idle();
    c.save(&idle)?;
    *record = idle;
    Ok(match previous {
        Some(previous) => Done::ok(format!(
            "rolled back: the service runs {} again",
            previous.display()
        )),
        None => Done::ok("rolled back: current was never changed, and the service runs"),
    })
}

fn cleanup(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if !record.has(Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "no verify has succeeded: cleanup does not commit on its own authority",
        ));
    }
    let staged = &params.staged_release;
    if !record.committed() {
        let unconfirmed = |why: String| Done::fail(Reason::NotConfirmed, why);
        if !c.current_is(staged)? {
            return Err(unconfirmed(format!(
                "the link current does not name {}",
                staged.display()
            )));
        }
        if c.observed_build(&params.target_version).is_err() {
            return Err(unconfirmed(format!(
                "{} is not seen to serve on {}",
                staged.display(),
                c.port
            )));
        }
        let stopping = record.with(Marker::Stopping);
        c.save(&stopping)?;
        *record = stopping;
    }
    if record.updating {
        c.ports.releases.remove(Link::Previous)?;
        let mut retired = record.clone();
        retired.updating = false;
        c.save(&retired)?;
        *record = retired;
    }
    Ok(Done::ok(format!(
        "{} serves alone; {SKIPPED_CLEANUP}",
        staged.display()
    )))
}

fn restore(c: &Ctx<'_, '_>, record: Option<&SingleRecord>) -> Result<Done, Done> {
    if record.is_none() {
        return Ok(Done::ok("no record: nothing to restore"));
    }
    let Some(current) = c.target(Link::Current)? else {
        return Err(Done::fail(
            Reason::NoSlot,
            "the link current names no release",
        ));
    };
    c.ports.service.start()?;
    Ok(Done::ok(format!("the service runs {}", current.display())))
}
