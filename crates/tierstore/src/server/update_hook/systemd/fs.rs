//! The second unit of the systemd arm, `tierstore-fs@a.service` and `tierstore-fs@b.service`:
//! the fs process that holds the FUSE mount of the node.
//!
//! The hook hands it over as it hands the S3 process over, with one difference it keeps to: the
//! fs process has no port and no status request, so what it says for itself is the state of its
//! unit. The unit is `Type=notify`, and a new instance says `READY=1` only once it has taken the
//! mount from the running one (`handoff-fs-v1`), so an instance whose unit is active has the
//! mount, and the old one ends by itself with status 0. Two rules follow from what the hook
//! cannot see of the processes:
//!
//! - the hook never stops an fs instance that may be taking or holding the mount. `systemctl stop`
//!   of an instance with no successor drops its descriptor store, the kernel aborts the
//!   connection and the jobs fail; an instance that is taking the mount does not act on SIGTERM
//!   until it serves, and one stopped while its take is in progress that then completes leaves at
//!   once, after the old instance has ended. An instance that failed to take the mount, or that a
//!   rollback finds activating (it may be in a `Restart=on-failure` retry), has its marker removed
//!   instead: systemd evaluates `ConditionPathExists=` on every start, automatic restarts
//!   included, so with the marker gone the unit is not started again and the instance ends by
//!   itself;
//! - `next` is started only while `live` runs. A new instance that finds nobody to ask makes a
//!   mount afresh, and over a mount that is held that is a collision: a `live` that is
//!   restarting or stopping defers `cutover`, and one that is failed or inactive holds nothing,
//!   so a fresh mount is the only way. Once `next` is active the marker of `live` is removed, so
//!   that both markers exist together only during the take.
//!
//! An update hands the fs process over only between releases that both list `handoff-fs-v1`. With
//! another pair it is left alone, running the release it runs: a restart of the process on a
//! kernel without `FUSE_HAS_RESEND` can leave requests unanswered until `fs.recover_wait` aborts
//! the connection, which fails jobs. The hook manages the unit only on a node where an instance
//! of it runs or the record already names one; a first install manages none, and an operator
//! makes the marker `/var/lib/tierstore/run-fs-<instance>` by hand after writing
//! `/etc/tierstore/fs.toml`.

use super::super::HANDOFF_FS_CAPABILITY;
use super::super::params::{Params, Previous};
use super::super::ports::{Instance, UnitState};
use super::super::record::{Marker, Record};
use super::super::report::{Done, Reason};
use super::{Ctx, DRAIN_RETRY_SECS, FsPolicy};

/// How long hostkeeper waits before it asks again when both fs instances run.
const BOTH_RUNNING_RETRY_SECS: u64 = 30;

/// The unit's name as the details spell it.
pub(super) fn fs_name(instance: Instance) -> String {
    format!("tierstore-fs@{instance}")
}

/// What `prepare` decided of the fs process.
pub(super) struct Plan {
    /// The instance that serves the mount now.
    pub live: Option<Instance>,
    /// The instance the update starts: `None` when the fs process is left alone.
    pub next: Option<Instance>,
    /// What the step says of it, after its own words.
    pub note: String,
}

impl Plan {
    fn none() -> Plan {
        Plan {
            live: None,
            next: None,
            note: String::new(),
        }
    }
}

/// Whether a list of capabilities (`tierstore --capabilities`) has `handoff-fs-v1`.
fn can(list: &[String]) -> bool {
    list.iter().any(|word| word == HANDOFF_FS_CAPABILITY)
}

/// The fs instance that serves now, for an update: the one that runs, else the one the record
/// says served last; `None` on a node that has none. Both running is not a state to start an
/// update from: the old one of an earlier update still drains.
fn serving(c: &Ctx<'_, '_>, record: Option<&Record>) -> Result<Option<Instance>, Done> {
    let a = c.ports.fs_units.state(Instance::A)?.running();
    let b = c.ports.fs_units.state(Instance::B)?.running();
    match (a, b) {
        (true, true) => Err(Done::deferred(
            Reason::HostNotReady,
            "both tierstore-fs@a and tierstore-fs@b run: the old one of an earlier update still \
             drains",
            BOTH_RUNNING_RETRY_SECS,
        )),
        (true, false) => Ok(Some(Instance::A)),
        (false, true) => Ok(Some(Instance::B)),
        (false, false) => Ok(record.and_then(|record| record.fs_live)),
    }
}

/// `prepare`'s look at the fs process. A first install (`update` is `None`) manages none. For an
/// update, `staged` is what the staged release lists, and `before` what the previous release
/// lists when `prepare` has asked it already; the previous release is asked here otherwise, and
/// only on a node that has an fs process.
pub(super) fn plan(
    c: &Ctx<'_, '_>,
    update: Option<&Previous>,
    record: Option<&Record>,
    staged: &[String],
    before: Option<Vec<String>>,
) -> Result<Plan, Done> {
    let Some(previous) = update else {
        return Ok(Plan::none());
    };
    let Some(live) = serving(c, record)? else {
        return Ok(Plan::none());
    };
    if c.fs == FsPolicy::LeftAlone {
        return Ok(Plan {
            live: Some(live),
            next: None,
            note: format!(
                "; fs {} is left on its release: a configuration update does not touch it",
                fs_name(live)
            ),
        });
    }
    let before = match before {
        Some(list) => list,
        None => c.ports.host.capabilities(&previous.release)?,
    };
    Ok(decide(live, staged, &before))
}

/// The one rule of the hand-over: `live` serves now; the fs process is handed over to the other
/// letter when both releases list `handoff-fs-v1`, and left alone otherwise.
fn decide(live: Instance, staged: &[String], before: &[String]) -> Plan {
    if can(staged) && can(before) {
        let next = live.other();
        return Plan {
            live: Some(live),
            next: Some(next),
            note: format!("; fs serving {}, next {}", fs_name(live), fs_name(next)),
        };
    }
    Plan {
        live: Some(live),
        next: None,
        note: format!(
            "; fs {} is left on its release: both releases must list {HANDOFF_FS_CAPABILITY}",
            fs_name(live)
        ),
    }
}

/// `cutover`'s own plan of the fs process, for a record that `prepare` wrote without one: when the
/// hook that ran `prepare` is the previous release's (the hook command of a package is the
/// installed binary), it does not know the fs unit, and the record has neither `fs_live` nor
/// `fs_next`. Before `swapped`, with exactly one fs instance running and a previous release, the
/// same rule as `plan` applies; the record is returned with the plan (the caller saves it before
/// anything of the fs unit starts) and the words for the detail. With both instances running the
/// process is left alone and the detail says so; with none, or no previous release, nothing is
/// planned. A record that already has a plan, or has `swapped`, is returned as it is.
pub(super) fn plan_late(
    c: &Ctx<'_, '_>,
    params: &Params,
    record: &Record,
) -> Result<Option<(Record, String)>, Done> {
    // A record that names `fs_next` names `fs_live` too (a record that does not decodes as invalid).
    if c.fs == FsPolicy::LeftAlone || record.fs_live.is_some() || record.has(Marker::Swapped) {
        return Ok(None);
    }
    let Some(previous) = &params.previous else {
        return Ok(None);
    };
    let a = c.ports.fs_units.state(Instance::A)?.running();
    let b = c.ports.fs_units.state(Instance::B)?.running();
    let live = match (a, b) {
        (false, false) => return Ok(None),
        (true, true) => {
            return Ok(Some((
                record.clone(),
                "; both tierstore-fs@a and tierstore-fs@b run: the fs process is left alone"
                    .to_owned(),
            )));
        }
        (true, false) => Instance::A,
        (false, true) => Instance::B,
    };
    let staged = c.ports.host.capabilities(&params.staged_release)?;
    let before = c.ports.host.capabilities(&previous.release)?;
    let plan = decide(live, &staged, &before);
    let mut planned = record.clone();
    planned.fs_live = plan.live;
    planned.fs_next = plan.next;
    Ok(Some((planned, String::new())))
}

/// What `cutover` says of the fs process, after its own words.
pub(super) fn cutover_note(record: &Record) -> String {
    match (record.fs_next, record.fs_live) {
        (Some(next), _) => format!("; {} holds the mount", fs_name(next)),
        (None, Some(live)) => format!("; {} is left on its release", fs_name(live)),
        (None, None) => String::new(),
    }
}

/// Whether the fs instance the update starts is active: trivially so when the update starts none.
pub(super) fn is_active(c: &Ctx<'_, '_>, record: &Record) -> Result<bool, Done> {
    match record.fs_next {
        None => Ok(true),
        Some(next) => Ok(c.ports.fs_units.state(next)? == UnitState::Active),
    }
}

/// Starts `next`, which takes the mount from the instance that runs, and checks that its unit is
/// active, which a notify unit is only after it said `READY=1`: after the take. Returns the words
/// the detail of `cutover` adds.
///
/// `next` is started only while `live` runs: a new instance that finds nobody to ask makes a
/// mount afresh, and over a mount that is held that is a collision. So a `live` that is
/// activating or deactivating defers the step (it runs again); a `live` that is failed or
/// inactive holds nothing (its unit, and with it its descriptor store, is gone) and a fresh mount
/// is the only way, which the detail says.
///
/// A start that fails, or leaves the unit anything but active, has taken nothing and the old
/// instance resumed: `next` is not stopped (it may be a `Restart=on-failure` retry that is taking
/// the mount), its marker is removed so that systemd does not start it again, and the step fails.
/// A start that succeeded and a look at the unit that failed leaves the marker: the unit may hold
/// the mount, as its only holder.
pub(super) fn take(c: &Ctx<'_, '_>, next: Instance) -> Result<String, Done> {
    if c.ports.fs_units.state(next)? == UnitState::Active {
        return Ok(String::new());
    }
    let live = next.other();
    let note = match c.ports.fs_units.state(live)? {
        UnitState::Active => String::new(),
        UnitState::Activating | UnitState::Deactivating => {
            return Err(Done::deferred(
                Reason::Draining,
                format!(
                    "{} is restarting or stopping: {} is not started over it, and cutover runs \
                     again",
                    fs_name(live),
                    fs_name(next)
                ),
                DRAIN_RETRY_SECS,
            ));
        }
        UnitState::Inactive | UnitState::Failed => format!(
            "; {} does not run, so {} makes the mount afresh",
            fs_name(live),
            fs_name(next)
        ),
    };
    c.ports.fs_markers.set(next)?;
    let failure = match c.ports.fs_units.start(next) {
        Err(error) => not_taken(next, Done::from(error)),
        Ok(()) => match c.ports.fs_units.state(next) {
            Ok(UnitState::Active) => return Ok(note),
            Ok(state) => Done::fail(
                Reason::NotReady,
                format!(
                    "{} is {state:?}, not active: it did not take the mount; {KEPT_SERVING}",
                    fs_name(next)
                ),
            ),
            // The unit may hold the mount, and its marker is what keeps it startable.
            Err(error) => return Err(Done::from(error)),
        },
    };
    c.ports.fs_markers.clear(next)?;
    Err(failure)
}

/// What the detail of a failed take says of the marker and the old instance.
const KEPT_SERVING: &str =
    "its marker was removed so that it is not started again, and the old instance serves on";

/// `failure`, the report of a start that failed, with the words of a take that did not happen.
fn not_taken(next: Instance, mut failure: Done) -> Done {
    let said = failure.report.detail.take().unwrap_or_default();
    failure.report.detail = Some(format!(
        "{said}; {} did not take the mount: {KEPT_SERVING}",
        fs_name(next)
    ));
    failure
}

/// `verify` and `cleanup` of the fs instance the update starts: active, else `retry` says how a
/// state that is only `activating` is told.
pub(super) fn check_active(c: &Ctx<'_, '_>, record: &Record, retry: u64) -> Result<(), Done> {
    let Some(next) = record.fs_next else {
        return Ok(());
    };
    match c.ports.fs_units.state(next)? {
        UnitState::Active => Ok(()),
        UnitState::Activating => Err(Done::deferred(
            Reason::NotReady,
            format!("{} is still activating", fs_name(next)),
            retry,
        )),
        other => Err(Done::fail(
            Reason::NotReady,
            format!("{} is {other:?}, not active", fs_name(next)),
        )),
    }
}

/// The end of a committed update that handed the fs process over: the old fs instance (the other
/// letter of `fs_next`) has its marker removed, whether it still drains or has exited, and is
/// never stopped. Returns it when it still runs: it ends by itself (an fs process of a release
/// that frees its whole state before it ends can take minutes).
pub(super) fn retire(c: &Ctx<'_, '_>, record: &Record) -> Result<Option<Instance>, Done> {
    let Some(next) = record.fs_next else {
        return Ok(None);
    };
    let old = next.other();
    let drains = c.ports.fs_units.state(old)?.running();
    c.ports.fs_markers.clear(old)?;
    Ok(drains.then_some(old))
}

/// The fs part of `rollback`, before the store's: the instance that served before the update, the
/// other letter of `next`, serves again. `next` is the one the update started.
///
/// - `next` holds the mount (active, or deactivating after it gave the mount away) and `live`
///   runs: one of them drains, and the step waits;
/// - `next` holds the mount and `live` does not run: `live` is started and takes the mount back
///   from `next`, which ends by itself; the step waits for that;
/// - `next` is only activating (a start that failed and retries, or a take in progress): it is not
///   stopped, its marker goes so that it is not started again, and the step waits for it to end by
///   itself: failed (the next run finds it holding nothing), or active after it took the mount
///   (the next run takes the mount back);
/// - `next` is failed or inactive: it holds nothing;
/// - `live` is started when it does not run and nothing holds the mount.
///
/// The marker of `next` goes once it is out.
pub(super) fn give_back(c: &Ctx<'_, '_>, next: Instance) -> Result<(), Done> {
    let live = next.other();
    let state = c.ports.fs_units.state(next)?;
    if state == UnitState::Activating {
        c.ports.fs_markers.clear(next)?;
        return Err(Done::deferred(
            Reason::Draining,
            format!(
                "{} is still activating and is not stopped: its marker is removed so that it is \
                 not started again, and the rollback waits for it to end",
                fs_name(next)
            ),
            DRAIN_RETRY_SECS,
        ));
    }
    let holds = matches!(state, UnitState::Active | UnitState::Deactivating);
    let live_up = c.ports.fs_units.state(live)?.running();
    if holds && live_up {
        return Err(draining(live, next));
    }
    if !live_up {
        c.ports.fs_markers.set(live)?;
        c.ports.fs_units.start(live)?;
    }
    if holds && c.ports.fs_units.state(next)?.running() {
        return Err(draining(live, next));
    }
    c.ports.fs_markers.clear(next)?;
    Ok(())
}

fn draining(live: Instance, next: Instance) -> Done {
    Done::deferred(
        Reason::Draining,
        format!(
            "{} and {} both run, and one of them drains: the rollback waits",
            fs_name(live),
            fs_name(next)
        ),
        DRAIN_RETRY_SECS,
    )
}

/// The fs instance the boot step starts: the one the record says holds the mount.
fn desired(record: &Record) -> Option<Instance> {
    if record.updating && record.has(Marker::Swapped) {
        record.fs_next.or(record.fs_live)
    } else {
        record.fs_live
    }
}

/// The fs part of `restore`, after the store's: the instance the record names runs, with its
/// marker, and the other does not start again. The other is stopped when it is active or
/// deactivating while the one the record names is not active, as it would race the first for the
/// mount. It is left running, with its marker removed, when it is only activating (it may be
/// taking the mount), and when the one the record names is active: that one holds the mount, and
/// the other is the old instance of a hand-over, which drains and ends by itself (the boot step
/// runs at every start of hostkeeper, not only after a reboot).
pub(super) fn restore(c: &Ctx<'_, '_>, record: Option<&Record>) -> Result<String, Done> {
    let Some(wanted) = record.and_then(desired) else {
        return Ok(String::new());
    };
    let other = wanted.other();
    let state = c.ports.fs_units.state(other)?;
    let mut drains = String::new();
    if state.running() && state != UnitState::Activating {
        if c.ports.fs_units.state(wanted)? == UnitState::Active {
            drains = format!("; {} still drains and ends by itself", fs_name(other));
        } else {
            c.ports.fs_units.stop(other)?;
        }
    }
    c.ports.fs_markers.clear(other)?;
    c.ports.fs_markers.set(wanted)?;
    c.ports.fs_units.start(wanted)?;
    Ok(format!("; {} started{drains}", fs_name(wanted)))
}

/// The fs part of `uninstall`: each instance that runs is stopped, and both markers go.
pub(super) fn uninstall(c: &Ctx<'_, '_>) -> Result<(), Done> {
    for instance in Instance::ALL {
        if c.ports.fs_units.state(instance)?.running() {
            c.ports.fs_units.stop(instance)?;
        }
    }
    for instance in Instance::ALL {
        c.ports.fs_markers.clear(instance)?;
    }
    Ok(())
}
