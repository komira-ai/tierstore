//! The **reload** class of a config package on systemd: a staged file that differs from the one
//! that serves in nothing but the keys a running process takes, or in nothing at all, is applied
//! in place by the instance that serves, and no other instance starts. The one key a running
//! process takes is `tree_list_ram_bytes`, a param of the `fs` interface (`fs.tree_list_ram_bytes`),
//! which the control socket of that interface sets (`set tree_list_ram_bytes <bytes>`,
//! `treefs/control.rs`); a byte-identical file is the case with nothing to set.
//!
//! - `prepare` records the update with `reload` and no `next` or `mode`, when the params declare no
//!   restart, exactly one instance runs, and the config hook's record has no other update in
//!   flight; otherwise the steps are the arm's, as for any config update.
//! - `cutover` reloads the guard when the package changes its file, has the control socket set the
//!   staged value, and records `swapped` with the mount the socket named.
//! - `verify` observes the build of the instance that serves, as the arm does (and, before the
//!   commit, defers while its cache is read-only), and reads the value back from that mount's
//!   gauge `tierstore_fs_tree_list_ram_bytes` on the public port (deferred, `not_ready`, while it
//!   shows another), then records `verified`.
//! - `rollback`, before the commit, reloads the guard again (when the package changed it and is
//!   not the node's first) and sets the value of the file that served, unless the instance no
//!   longer runs (a start reads the file hostkeeper put back).
//! - `cleanup` confirms afresh that the instance runs and serves, then records the commit and the
//!   update at rest, `live` unchanged.

use std::convert::identity;

use super::super::PUBLIC_PORT;
use super::super::args::Step;
use super::super::look::{Look, verified, writable};
use super::super::params::Params;
use super::super::ports::{Control, Guard, Instance, Lock, StateDir, SystemdPorts, UnitState};
use super::super::probe::tree_list_ram_bytes_of;
use super::super::record::{ListCap, Marker, Record, Reload};
use super::super::report::{Done, Outcome, Reason};
use super::super::shape::{FS_INTERFACE, fs_interface};
use crate::server::local_cache::tree_lists::CAP;
use crate::server::treefs::DEFAULT_TREE_LIST_RAM_BYTES;

/// The param of the `fs` interface that the instance that serves applies in place.
const TREE_LIST_RAM_BYTES: &str = "tree_list_ram_bytes";

/// The table of a file with the key applied in place taken out of its first `fs` interface, the
/// one the hook reads the control socket of (a params table left empty goes too), and that key's
/// value (the default when it names none; `None` when the file has no such interface). `None` when
/// the text is not TOML or the key is not a whole number of bytes, which a file the release accepts
/// never has.
fn split(text: &str) -> Option<(toml::Table, Option<u64>)> {
    let mut table: toml::Table = text.parse().ok()?;
    let entry = table
        .get_mut("interface")
        .and_then(toml::Value::as_array_mut)
        .and_then(|entries| {
            entries
                .iter_mut()
                .filter_map(toml::Value::as_table_mut)
                .find(|entry| entry.get("type").and_then(toml::Value::as_str) == Some(FS_INTERFACE))
        });
    let Some(entry) = entry else {
        return Some((table, None));
    };
    let params = entry.get_mut("params").and_then(toml::Value::as_table_mut);
    let bytes = match params.and_then(|params| params.remove(TREE_LIST_RAM_BYTES)) {
        None => DEFAULT_TREE_LIST_RAM_BYTES,
        Some(value) => u64::try_from(value.as_integer()?).ok()?,
    };
    if entry
        .get("params")
        .and_then(toml::Value::as_table)
        .is_some_and(toml::Table::is_empty)
    {
        let _ = entry.remove("params");
    }
    Some((table, Some(bytes)))
}

/// What the change from the file `serving` to `staged` applies in place, the guard aside; `None`
/// when the two differ in anything else, which needs another instance.
pub(super) fn of(serving: &str, staged: &str) -> Option<Reload> {
    let (serving_table, from) = split(serving)?;
    let (staged_table, to) = split(staged)?;
    if serving_table != staged_table {
        return None;
    }
    // The tables are alike, so both files have an `fs` interface or neither has, and it names the
    // same control socket in both.
    let tree_list_ram_bytes = match (from, to, fs_interface(staged)) {
        (Some(from), Some(to), Some(fs)) if from != to => Some(ListCap {
            from,
            to,
            control_socket: fs.control_socket,
            mount: None,
        }),
        _ => None,
    };
    Some(Reload {
        guard: false,
        tree_list_ram_bytes,
    })
}

/// What a reload changes, in words.
fn said(reload: &Reload) -> String {
    let mut parts = Vec::new();
    if let Some(cap) = &reload.tree_list_ram_bytes {
        parts.push(format!("fs.tree_list_ram_bytes {} to {}", cap.from, cap.to));
    }
    if reload.guard {
        parts.push("the guard's file".to_owned());
    }
    if parts.is_empty() {
        "nothing a running process cannot apply".to_owned()
    } else {
        parts.join(" and ")
    }
}

fn unit_name(instance: Instance) -> String {
    format!("tierstore@{instance}")
}

/// The config hook's record, held to its rules; `None` when there is none.
fn read(state: &dyn StateDir) -> Result<Option<Record>, Done> {
    let Some(bytes) = state.load()? else {
        return Ok(None);
    };
    let record = Record::decode(&bytes)?;
    record.check()?;
    Ok(Some(record))
}

/// `prepare` of a reload over the params `params` of the config package, whose staged file the
/// instance that serves `build` applies as `reload` says. `Ok(None)` leaves the step to the arm:
/// no instance runs, or both do (one of them drains), or the config hook's record has an update of
/// the arm in flight (the arm finishes a committed one, and refuses one past its cutover). A `prepare` run again over the reload it
/// recorded keeps it: hostkeeper may have installed the staged file since, which the change would
/// then be read from as none.
pub(super) fn prepare(
    params: &Params,
    ports: &SystemdPorts<'_>,
    build: &str,
    reload: Reload,
) -> Result<Option<Done>, Done> {
    let _lock = ports.state.lock()?;
    let record = read(ports.state)?;
    if let Some(record) = &record
        && record.updating
    {
        if record.reload.is_none() {
            return Ok(None);
        }
        if !record.markers.is_empty() {
            return Err(Done::fail(
                Reason::OutOfOrder,
                "an update is past its cutover: finish or roll it back first",
            ));
        }
        if let (Some(reload), Some(live)) = (&record.reload, record.live)
            && record.to.as_ref() == Some(&params.staged_release)
            && record.version.as_deref() == Some(build)
        {
            return Ok(Some(Done::ok(format!(
                "prepared in place already: {} applies {}; no other instance starts",
                unit_name(live),
                said(reload)
            ))));
        }
    }
    let mut running = Vec::new();
    for instance in Instance::ALL {
        if ports.units.state(instance)?.running() {
            running.push(instance);
        }
    }
    let [live] = running.as_slice() else {
        return Ok(None);
    };
    let detail = format!(
        "prepared in place: {} applies {}; no other instance starts",
        unit_name(*live),
        said(&reload)
    );
    let fresh = Record {
        updating: true,
        to: Some(params.staged_release.clone()),
        version: Some(build.to_owned()),
        reload: Some(reload),
        ..Record::idle(Some(*live))
    };
    ports.state.save(&fresh.encode())?;
    Ok(Some(Done::ok(detail)))
}

/// The config hook's record of a reload, read under the lock it is held with.
pub(super) struct Opened {
    _lock: Lock,
    record: Record,
    reload: Reload,
}

/// Takes the lock and reads the config hook's record; `None` (and the lock let go) when it is not
/// of a reload, whose steps are then the arm's.
pub(super) fn open(state: &dyn StateDir) -> Result<Option<Opened>, Done> {
    let lock = state.lock()?;
    let Some(record) = read(state)? else {
        return Ok(None);
    };
    let Some(reload) = record.reload.clone() else {
        return Ok(None);
    };
    Ok(Some(Opened {
        _lock: lock,
        record,
        reload,
    }))
}

/// What the steps of a reload act with.
pub(super) struct Act<'a> {
    /// The arm's ports.
    pub ports: &'a SystemdPorts<'a>,
    /// The network guard.
    pub guard: &'a dyn Guard,
    /// The control socket of the instance that serves.
    pub control: &'a dyn Control,
    /// The params as the arm takes them: the build that serves as the version.
    pub params: &'a Params,
    /// Whether the package is the node's first config package: nothing to go back to.
    pub first: bool,
}

/// Runs `step` (`cutover`, `verify`, `rollback` or `cleanup`; the caller passes no other) of the
/// reload `opened`. Nothing is reversible once the record is committed.
pub(super) fn run(step: Step, opened: &Opened, act: &Act<'_>) -> Done {
    let ran = if let Some(detail) = (step == Step::Rollback)
        .then(|| opened.record.nothing_to_roll_back(act.params))
        .flatten()
    {
        Ok(Done::ok(detail))
    } else if step == Step::Rollback
        && let Some(judged) = another_in_flight(&opened.record, act)
    {
        // The record is then at rest: nothing of the package the params name is in flight.
        judged
            .and_then(|()| roll_back_stale(opened, act))
            .map(|note| {
                Done::ok(format!(
                    "{note}; nothing to roll back: no update is in flight"
                ))
            })
    } else if !opened.record.is_of(act.params) {
        // A record of a reload is in flight or committed (`Record::check`): of one update.
        Err(Done::fail(
            Reason::BadParams,
            format!(
                "the params name {} {}, and the update on record is {:?} {:?}",
                act.params.staged_release.display(),
                act.params.target_version,
                opened.record.to,
                opened.record.version,
            ),
        ))
    } else {
        match step {
            Step::Cutover => cutover(opened, act),
            Step::Verify => verify(opened, act),
            Step::Rollback => rollback(opened, act),
            _ => cleanup(opened, act),
        }
    };
    let mut done = ran.unwrap_or_else(identity);
    // As the arm's: a `rollback` of another package finds nothing of this one committed.
    let committed = opened.record.commits(step, act.params)
        || (step == Step::Cleanup && done.outcome == Outcome::Success);
    done.report.reversible &= !committed;
    done
}

/// Whether the reload on record is of another package, in flight and not committed, that `act`
/// rolls back: hostkeeper gave that package up and moved `current` back, and the updater that
/// started it is gone, so its rollback never ran. `None` when the record is at rest, committed or
/// of the package `act` names; `Some(Err)`, with why, for one it keeps, which the step refuses as
/// before: `act` is a node's first config package (whether the package on record was one too, and
/// so whether the guard is reloaded, is not on record), or `act` names that package's own staged
/// release as the one `current` names, as an update still in its cutover does (hostkeeper moved
/// `current` to it). The record of a reload names no `from`, so the release it returns to is the
/// one `current` names, which is what its own rollback would find.
fn another_in_flight(record: &Record, act: &Act<'_>) -> Option<Result<(), Done>> {
    if !record.updating || record.committed() || record.is_of(act.params) {
        return None;
    }
    let kept = |why: String| {
        Some(Err(Done::fail(
            Reason::BadParams,
            format!(
                "the params name {} {}, and the update on record is {:?} {:?}: {why}",
                act.params.staged_release.display(),
                act.params.target_version,
                record.to.as_deref(),
                record.version.as_deref(),
            ),
        )))
    };
    if act.first {
        return kept(
            "the params are of a node's first config package, and the package on record is \
             rolled back only with the params it was written under"
                .to_owned(),
        );
    }
    let current = act.params.previous.as_ref().map(|p| p.release.as_path());
    if current == record.to.as_deref() {
        return kept(
            "the params name its staged release as the one the host runs: that update is still \
             in its cutover"
                .to_owned(),
        );
    }
    Some(Ok(()))
}

/// Rolls the reload on record back, as its own `rollback` does: it needs nothing of the params
/// but whether the package is a node's first. The note that leads the step's detail.
fn roll_back_stale(opened: &Opened, act: &Act<'_>) -> Result<String, Done> {
    let whose = format!(
        "the record is of the update to {} {}, not {} {}",
        opened.record.to.clone().unwrap_or_default().display(),
        opened.record.version.as_deref().unwrap_or_default(),
        act.params.staged_release.display(),
        act.params.target_version
    );
    let back = rollback(opened, act).map_err(|mut done| {
        let rest = done.report.detail.take().unwrap_or_default();
        done.report.detail = Some(format!(
            "{whose}, and its rollback with its own target and releases is not done; {rest}"
        ));
        done
    })?;
    Ok(format!(
        "{whose}: rolled back with its own target and releases ({})",
        back.report.detail.as_deref().unwrap_or_default()
    ))
}

/// `prepare` over the record of a reload of another package that is past its cutover and that
/// `act` rolls back ([`another_in_flight`]): it is rolled back first, and the note that leads the
/// step's detail is returned. `None` for any other record: one before its cutover is recorded
/// over, as before, and one past it that is kept is refused by the step.
pub(super) fn roll_back_before_prepare(act: &Act<'_>) -> Result<Option<String>, Done> {
    let Some(opened) = open(act.ports.state)? else {
        return Ok(None);
    };
    if opened.record.markers.is_empty() {
        return Ok(None);
    }
    match another_in_flight(&opened.record, act) {
        Some(Ok(())) => roll_back_stale(&opened, act).map(Some),
        _ => Ok(None),
    }
}

/// The instance that serves, which the record of a reload names.
fn live_of(record: &Record) -> Result<Instance, Done> {
    record.live.ok_or_else(|| {
        Done::fail(
            Reason::StateInvalid,
            "the record of the update in place names no instance that serves",
        )
    })
}

/// Has the control socket of `cap` set the cap `bytes`; the id of the mount it names.
fn set(control: &dyn Control, cap: &ListCap, bytes: u64) -> Result<String, Done> {
    let request = format!("set {TREE_LIST_RAM_BYTES} {bytes}");
    let line = control.ask(&cap.control_socket, &request)?;
    let answer: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    match (answer["ok"].as_bool(), answer["mount"].as_str()) {
        (Some(true), Some(mount)) => Ok(mount.to_owned()),
        _ => Err(Done::fail(
            Reason::HostFailed,
            format!(
                "the control socket {} did not take {request:?}: it answered {line}",
                cap.control_socket.display()
            ),
        )),
    }
}

fn cutover(opened: &Opened, act: &Act<'_>) -> Result<Done, Done> {
    let record = &opened.record;
    // A record of a reload that is not committed has one in flight (`Record::check`).
    if record.committed() {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "cutover follows a prepare, before cleanup",
        ));
    }
    let live = live_of(record)?;
    if opened.reload.guard {
        act.guard.reload()?;
    }
    let mut swapped = record.with(Marker::Swapped);
    if let Some(cap) = &opened.reload.tree_list_ram_bytes {
        let mount = set(act.control, cap, cap.to)?;
        swapped.reload = Some(Reload {
            tree_list_ram_bytes: Some(ListCap {
                mount: Some(mount),
                ..cap.clone()
            }),
            ..opened.reload.clone()
        });
    }
    // `swapped` follows its act: the value is set. Set again, it changes nothing.
    if &swapped != record {
        act.ports.state.save(&swapped.encode())?;
    }
    Ok(Done::ok(format!(
        "applied in place: {} took {}; no other instance started",
        unit_name(live),
        said(&opened.reload)
    )))
}

fn verify(opened: &Opened, act: &Act<'_>) -> Result<Done, Done> {
    let record = &opened.record;
    // A record of a reload that is not committed has one in flight (`Record::check`).
    let committed = record.committed();
    if !committed && !record.has(Marker::Swapped) {
        return Err(Done::fail(
            Reason::OutOfOrder,
            "verify follows a cutover that succeeded",
        ));
    }
    let live = live_of(record)?;
    let retry = act.params.verify_retry_secs;
    if !committed {
        let state = act.ports.units.state(live)?;
        if state != UnitState::Active {
            return Err(Done::fail(
                Reason::NotReady,
                format!("{} is {state:?}, not active", unit_name(live)),
            ));
        }
    }
    let target = &act.params.target_version;
    let look = Look {
        instances: act.ports.instances,
        clock: act.ports.clock,
    };
    let mode = look.observed(target).map_err(|done| {
        if done.report.reason == Some(Reason::NotReady) {
            Done::deferred(
                Reason::NotReady,
                format!("{} does not answer /readyz yet", unit_name(live)),
                retry,
            )
        } else {
            done
        }
    })?;
    // Before the commit the instance must be the cache's writer, as the arm's `verify` requires:
    // until it is, the node's jobs cannot fill their trees.
    let mode = if committed {
        mode
    } else {
        writable(mode, &unit_name(live), retry)?
    };
    let mut first = String::new();
    if let Some(cap) = &opened.reload.tree_list_ram_bytes {
        let mount = cap.mount.as_deref().ok_or_else(|| {
            Done::fail(
                Reason::StateInvalid,
                "the record of the update in place names no mount whose cap was set",
            )
        })?;
        let shown = act
            .ports
            .instances
            .get(PUBLIC_PORT, "/metricz")
            .ok()
            .filter(|reply| reply.status == 200)
            .and_then(|reply| tree_list_ram_bytes_of(&reply.body, mount));
        if shown != Some(cap.to) {
            let shown = shown.map_or_else(|| "nothing".to_owned(), |bytes| bytes.to_string());
            return Err(Done::deferred(
                Reason::NotReady,
                format!(
                    "{CAP}{{mount=\"{mount}\"}} on port {PUBLIC_PORT} shows {shown}, not {}",
                    cap.to
                ),
                retry,
            ));
        }
        first = format!("fs.tree_list_ram_bytes is {} in place; ", cap.to);
    }
    // `verified` follows its act: everything above was observed.
    if !committed && !record.has(Marker::Verified) {
        act.ports
            .state
            .save(&record.with(Marker::Verified).encode())?;
    }
    let mut done = verified(mode, target);
    done.report.detail = Some(format!(
        "{first}{}",
        done.report.detail.take().unwrap_or_default()
    ));
    Ok(done)
}

fn rollback(opened: &Opened, act: &Act<'_>) -> Result<Done, Done> {
    let record = &opened.record;
    // A record of a reload that is not committed has one in flight (`Record::check`).
    if record.committed() {
        return Err(Done::fail(
            Reason::Committed,
            "cleanup has begun: nothing can be rolled back",
        ));
    }
    let live = live_of(record)?;
    // A node's first config package has no files to go back to: its guard is not reloaded.
    if opened.reload.guard && !act.first {
        act.guard.reload()?;
    }
    let mut back = String::new();
    if let Some(cap) = &opened.reload.tree_list_ram_bytes {
        // An instance that does not run reads the file hostkeeper put back when it starts.
        if act.ports.units.state(live)?.running() {
            let _ = set(act.control, cap, cap.from)?;
        }
        back = format!(" on fs.tree_list_ram_bytes {}", cap.from);
    }
    act.ports.state.save(&Record::idle(Some(live)).encode())?;
    if act.first {
        return Err(super::steps::no_previous(Some(live)));
    }
    Ok(Done::ok(format!(
        "rolled back in place: {} serves{back} as before",
        unit_name(live)
    )))
}

fn cleanup(opened: &Opened, act: &Act<'_>) -> Result<Done, Done> {
    let record = &opened.record;
    // A record of a reload with none in flight is committed (`Record::check`).
    if !record.updating {
        return Ok(Done::ok(
            "nothing left to clean: the update is committed and finished",
        ));
    }
    if !record.has(Marker::Verified) {
        return Err(Done::fail(
            Reason::NotVerified,
            "cleanup follows a verify that succeeded",
        ));
    }
    let live = live_of(record)?;
    // Afresh, before the commit, as the arm's `cleanup` confirms: the instance runs and serves.
    let not_confirmed = |why: String| Done::fail(Reason::NotConfirmed, why);
    let state = act.ports.units.state(live)?;
    if state != UnitState::Active {
        return Err(not_confirmed(format!(
            "{} is {state:?}, not active",
            unit_name(live)
        )));
    }
    let look = Look {
        instances: act.ports.instances,
        clock: act.ports.clock,
    };
    let target = &act.params.target_version;
    look.observed(target).map_err(|failed| {
        not_confirmed(format!(
            "{target} was not seen serving: {}",
            failed.report.detail.unwrap_or_default()
        ))
    })?;
    // The commit and the end in one write: there is no old instance to retire.
    let mut finished = record.with(Marker::Stopping);
    finished.updating = false;
    act.ports.state.save(&finished.encode())?;
    Ok(Done::ok(format!(
        "committed: {} serves the new file, applied in place",
        unit_name(live)
    )))
}
