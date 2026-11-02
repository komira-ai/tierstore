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
//! - `prepare` checks the staged release and the release `current` names
//!   ([`Releases::check`](super::Releases::check): reached through root's directories only, no
//!   link on the way, the binary a file of root's that no group or others can write; nothing is
//!   run), records the update (`from` the release `current` names, `to` the staged release, and
//!   its `version`) and points the link `previous` at `from`. Nothing is started. A re-run keeps
//!   the `from` of the record. The two-slot `prepare`'s guard table, its connection-tracking check
//!   and its second instance have nothing to act on here, and are skipped ([`SKIPPED_PREPARE`]).
//! - Every later step refuses params whose staged release or version is not the record's
//!   (`bad_params`), before it acts.
//! - `cutover` stops the service, points `current` at `to` and starts the service, then waits
//!   for `/readyz` and for [`PROBE_HITS`](super::steps::PROBE_HITS) answers in a row from
//!   `version` on the port, and writes `swapped`. A re-run after `swapped` looks again (`current`,
//!   `/readyz`, the probe) and swaps again if it must. A failure puts `from` back as `current`,
//!   restarts the service, waits for it to answer `/readyz`, clears `swapped` if it was written,
//!   and reports the failure with the words `rolled back` (or what failed in the rollback); the
//!   update stays on record, for the updater's `rollback` to clear. There is no redirect to move
//!   ([`SKIPPED_CUTOVER`]).
//! - `verify` looks at `current`, `/readyz` and the build on the port, waits for the promotion
//!   as a restart-mode `verify` does, and records `verified`.
//! - `rollback` points `current` at `from` again (when it is not already), restarts the service,
//!   and waits for it to answer `/readyz`; only then does it remove `previous` and clear the
//!   update. Valid only before the commit.
//! - `cleanup` confirms afresh what `verify` saw, writes `stopping` (the commit), removes
//!   `previous` and clears the update. There is no previous instance to stop
//!   ([`SKIPPED_CLEANUP`]). When `cleanup` fails, the record is read back, so that a write of
//!   `stopping` that reached the disk and then reported a failure (a rename made, its sync
//!   failed) is reported as the commit it is.
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
use super::report::Outcome;
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

    /// Puts `from` back as `current`, restarting the service when `current` named another
    /// release and otherwise making sure it runs, then waits for it to answer `/readyz`: a
    /// rollback is not done until the release it went back to serves.
    fn restore_previous(&self, from: &Path) -> Result<(), Done> {
        if self.target(Link::Current)?.as_deref() == Some(from) {
            self.ports.service.start()?;
        } else {
            self.restart_on(from)?;
        }
        if self.look().wait_ready(self.port, READY_ATTEMPTS).is_none() {
            return Err(Done::fail(
                Reason::NotReady,
                format!(
                    "{} does not answer /readyz on {}",
                    from.display(),
                    self.port
                ),
            ));
        }
        Ok(())
    }
}

/// Refuses params that name another release or version than the update on record.
fn same_update(record: &SingleRecord, params: &Params) -> Result<(), Done> {
    let to = record.to.as_deref();
    let version = record.version.as_deref();
    if to == Some(params.staged_release.as_path()) && version == Some(&params.target_version) {
        return Ok(());
    }
    Err(Done::fail(
        Reason::BadParams,
        format!(
            "the params name {} {}, and the update on record is {} {}",
            params.staged_release.display(),
            params.target_version,
            to.map_or_else(|| "none".into(), |to| to.display().to_string()),
            version.unwrap_or("none")
        ),
    ))
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
    if decl.state_dir != ports.state_dir {
        return Err(Done::fail(
            Reason::StateInvalid,
            format!(
                "the declaration changed under the step: it names {}, and the lock is that of {}",
                decl.state_dir.display(),
                ports.state_dir.display()
            ),
        ));
    }
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
    let mut committed = record.is_some_and(|r| r.committed());
    if step == Step::Cleanup && !committed && done.outcome != Outcome::Success {
        // A failed write of `stopping` may have reached the disk (a rename made, its sync
        // failed): the record on disk says what was decided. One that cannot be read leaves
        // the answer as the step saw it; a `rollback` refuses a commit it finds on disk anyway.
        if let Ok(Some(bytes)) = ports.state.load() {
            committed = SingleRecord::decode(&bytes).is_ok_and(|r| r.committed());
        }
    }
    Ok((done, committed))
}

fn prepare(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if record.updating && !record.markers.is_empty() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "an update is past its cutover: finish or roll it back first",
        ));
    }
    let staged = &params.staged_release;
    c.ports.releases.check(staged)?;
    // A re-run keeps the `from` an earlier run recorded: by then `current` may already name the
    // staged release. A fresh update takes it from `current`, whatever a crash left in `previous`.
    let from = match (&record.from, record.updating) {
        (Some(from), true) => from.clone(),
        _ => c.target(Link::Current)?.ok_or_else(|| {
            Done::fail(
                Reason::NoSlot,
                "the link current names no release: the first install lays it down",
            )
        })?,
    };
    c.ports.releases.check(&from)?;
    let next = SingleRecord::updating(from.clone(), staged.clone(), params.target_version.clone());
    c.save(&next)?;
    *record = next;
    c.ports.releases.point(Link::Previous, &from)?;
    Ok(Done::ok(format!(
        "prepared {} to follow {}, mode restart (single_instance); {SKIPPED_PREPARE}",
        staged.display(),
        from.display()
    )))
}

/// The swap: stop, point `current` at the staged release, start, and see it serve `version`.
fn swap(c: &Ctx<'_, '_>, staged: &Path, version: &str) -> Result<(), Done> {
    c.restart_on(staged)?;
    c.serves(version)
}

fn cutover(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if !record.updating || record.committed() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows a prepare and precedes the commit",
        ));
    }
    same_update(record, params)?;
    let staged = &params.staged_release;
    let version = &params.target_version;
    let swapped = record.has(Marker::Swapped);
    // A second run after `swapped` looks again: the service may have been stopped, `current`
    // moved or the instance gone since. It swaps again when what it sees is not the staged
    // release serving.
    let seen = swapped
        && c.current_is(staged)?
        && c.ports.service.start().is_ok()
        && c.serves(version).is_ok();
    if !seen {
        if let Err(mut done) = swap(c, staged, version) {
            let from = record.from.clone().unwrap_or_default();
            // The previous release goes back in front, whatever failed; the update stays on
            // record, for the updater's `rollback` to clear (or a second `cutover` to retry).
            let back = match c.restore_previous(&from) {
                Ok(()) => format!("rolled back: the service runs {} again", from.display()),
                Err(failed) => format!(
                    "the rollback to {} failed: {}",
                    from.display(),
                    failed.report.detail.unwrap_or_default()
                ),
            };
            let detail = done.report.detail.take().unwrap_or_default();
            done.report.detail = Some(format!("{detail}; {back}"));
            if swapped {
                let mut unswapped = record.clone();
                unswapped.markers.clear();
                c.save(&unswapped)?;
                *record = unswapped;
            }
            return Err(done);
        }
        if !swapped {
            let next = record.with(Marker::Swapped);
            c.save(&next)?;
            *record = next;
        }
    }
    Ok(Done::ok(format!(
        "the service runs {} on {}; {SKIPPED_CUTOVER}",
        staged.display(),
        c.port
    )))
}

fn verify(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if record.updating || record.committed() {
        same_update(record, params)?;
    }
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
    let from = record.from.clone().unwrap_or_default();
    // The update stays on record until the release it went back to answers: a rollback that
    // started a release that does not serve can be run again.
    c.restore_previous(&from)?;
    c.ports.releases.remove(Link::Previous)?;
    // The record is cleared last: a crash before it re-runs every part above.
    let idle = SingleRecord::idle();
    c.save(&idle)?;
    *record = idle;
    Ok(Done::ok(format!(
        "rolled back: the service runs {} again",
        from.display()
    )))
}

fn cleanup(c: &Ctx<'_, '_>, params: &Params, record: &mut SingleRecord) -> Result<Done, Done> {
    if !record.has(Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "no verify has succeeded: cleanup does not commit on its own authority",
        ));
    }
    same_update(record, params)?;
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
        retired.from = None;
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
