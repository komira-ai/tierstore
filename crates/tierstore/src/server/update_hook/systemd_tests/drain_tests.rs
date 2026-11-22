//! An update whose old instances drain for longer than hostkeeper runs `cleanup` again: the
//! update succeeds at its first `cleanup`, and every step after it is right while the old
//! instances still drain and after they ended. The fake's old instances drain for as many looks
//! as a test gives them, and end when a test says so: no step waits on a clock.

use std::path::PathBuf;

use crate::server::update_hook::{Instance, Marker, Mode, Reason, Step, UnitState};

use super::fake::{
    NEW, NEWER, OLD, SystemdFake, assert_deferred, assert_ok, detail, params, version_of,
};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

/// More looks than every step of these tests makes: the old instance drains through all of them.
const FOREVER: u32 = 10_000;

/// hostkeeper's runs of `cleanup` in one update: the first, and `cleanup_retries` (3) more.
const CLEANUP_RUNS: usize = 4;

/// A node with an fs process whose update from `OLD` to `NEW` has run `cleanup`, the old store
/// and fs instances (a) still draining.
fn drained_update(store_drains: bool) -> SystemdFake {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_drain_looks(FOREVER);
    if store_drains {
        fake.set_drain_looks(FOREVER);
    }
    fake.complete(&params(NEW, Some(OLD)));
    fake
}

/// The old instances end, as they do by themselves when their last request and open file are
/// done.
fn drains_end(fake: &SystemdFake) {
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
}

/// Asserts that an update whose old fs instance drains past every run of `cleanup` hostkeeper
/// makes succeeds at the first: `cleanup` exits 0, the record is at rest with b as `live` and
/// `fs_live`, the old fs instance is never stopped and still drains, and only the markers of b
/// are left. Catches a `cleanup` that defers while the old fs instance drains, which hostkeeper
/// runs four times and then reports as a failed update.
#[test]
fn an_update_whose_old_fs_instance_drains_past_every_cleanup_retry_succeeds() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_drain_looks(FOREVER);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    let done = fake.run(Cleanup, &p);
    assert_ok(&done);
    assert!(!done.report.reversible);
    assert_eq!(
        detail(&done),
        "committed: tierstore@b serves; tierstore-fs@a still drains and ends by itself"
    );
    let record = fake.record();
    assert!(!record.updating);
    assert!(record.committed());
    assert_eq!(
        (record.live, record.next, record.fs_live, record.fs_next),
        (Some(Instance::B), None, Some(Instance::B), None)
    );
    assert_eq!(record.from, None);
    assert_eq!(record.mode, None);
    assert_eq!(fake.peek_fs(Instance::A), UnitState::Deactivating);
    assert_eq!(fake.fs_running(), [Instance::A, Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);
    assert!(
        !fake
            .fs_actions()
            .iter()
            .any(|a| a.starts_with("fs_units.stop"))
    );
    // Every later run hostkeeper could make finds nothing left to do.
    for _ in 1..CLEANUP_RUNS {
        let again = fake.run(Cleanup, &p);
        assert_ok(&again);
        assert_eq!(
            detail(&again),
            "nothing left to clean: the update is committed and finished"
        );
    }
    assert_eq!(fake.record(), record);
}

/// Asserts that with the old store and fs instances both draining, `cleanup` names both in its
/// detail and stops neither. Catches a detail that hides one of them, and a stop of either.
#[test]
fn cleanup_names_every_old_instance_that_still_drains() {
    let fake = SystemdFake::fs_installed();
    fake.set_drain_looks(FOREVER);
    fake.set_fs_drain_looks(FOREVER);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    let done = fake.run(Cleanup, &p);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        "committed: tierstore@b serves; tierstore@a and tierstore-fs@a still drain and end by \
         themselves"
    );
    assert_eq!(fake.running(), [Instance::A, Instance::B]);
    assert!(!fake.actions().iter().any(|a| a.starts_with("units.stop")));
    assert!(
        !fake
            .fs_actions()
            .iter()
            .any(|a| a.starts_with("fs_units.stop"))
    );
}

/// Asserts that the record is finished at the commit and stays so when the old instances exit:
/// their exit changes nothing on record, and a `cleanup` after it has nothing left. Catches a
/// record that waits on the exit of the old instance to say that b serves.
#[test]
fn the_record_is_finished_at_the_commit_and_stays_finished_when_the_old_instances_exit() {
    let fake = drained_update(true);
    let finished = fake.record();
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));
    assert_eq!(finished.fs_live, Some(Instance::B));
    drains_end(&fake);
    let done = fake.run(Cleanup, &params(NEW, Some(OLD)));
    assert_eq!(
        detail(&done),
        "nothing left to clean: the update is committed and finished"
    );
    assert_eq!(fake.record(), finished);
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);
}

/// Asserts that the `prepare` of the next update while the old fs instance still drains is
/// deferred (`host_not_ready`, again in 30 seconds) and records nothing, that the `rollback`
/// hostkeeper runs after it has nothing to roll back and touches no unit, and that once the old
/// instance has ended the same `prepare` goes on and the update hands both processes back to a.
/// Catches a `prepare` that starts an fs instance of a letter that still drains, and one that
/// fails for good.
#[test]
fn a_prepare_while_the_old_fs_instance_drains_waits_and_then_proceeds() {
    let fake = drained_update(false);
    let at_rest = fake.record();
    let next = params(NEWER, Some(NEW));

    let waits = fake.run(Prepare, &next);
    assert_deferred(&waits, Reason::HostNotReady, 30);
    assert!(waits.report.reversible);
    assert!(detail(&waits).contains("tierstore-fs@a and tierstore-fs@b run"));
    assert_eq!(fake.record(), at_rest);
    let acts = fake.actions().len() + fake.fs_actions().len();
    let back = fake.run(Rollback, &next);
    assert_ok(&back);
    assert!(detail(&back).starts_with("nothing to roll back"));
    assert_eq!(fake.actions().len() + fake.fs_actions().len(), acts);
    assert_eq!(fake.record(), at_rest);

    drains_end(&fake);
    fake.drive(&next, &[Prepare]);
    let prepared = fake.record();
    assert_eq!(
        (
            prepared.live,
            prepared.next,
            prepared.fs_live,
            prepared.fs_next
        ),
        (
            Some(Instance::B),
            Some(Instance::A),
            Some(Instance::B),
            Some(Instance::A)
        )
    );
    fake.drive(&next, &[Cutover, Verify, Cleanup]);
    let record = fake.record();
    assert_eq!(record.live, Some(Instance::A));
    assert_eq!(record.fs_live, Some(Instance::A));
    assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(NEWER)));
}

/// Asserts that the boot step after a reboot in the drain window starts only the new letter of
/// each unit (the old ones have no marker), and that the boot step at a start of hostkeeper with
/// no reboot leaves the draining old instances running, stops nothing, and keeps only the markers
/// of b. Catches a boot step that stops an old instance in its drain, which cuts the open files
/// of running jobs, and one that starts an old letter again.
#[test]
fn the_boot_step_in_the_drain_window_starts_only_the_new_letters() {
    let fake = drained_update(true);
    let at_rest = fake.record();
    let p = params(NEW, None);
    let done = fake.run(Restore, &p);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        "tierstore@b started; tierstore-fs@b started; tierstore-fs@a still drains and ends by \
         itself; tierstore@a still drains and ends by itself"
    );
    assert!(!fake.actions().iter().any(|a| a.starts_with("units.stop")));
    assert!(
        !fake
            .fs_actions()
            .iter()
            .any(|a| a.starts_with("fs_units.stop"))
    );
    assert_eq!(fake.running(), [Instance::A, Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::A, Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);
    assert_eq!(fake.record(), at_rest);

    fake.reboot();
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::B]);
    assert_ok(&fake.run(Restore, &p));
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::B]);
    assert_eq!(fake.record(), at_rest);
}

/// Asserts that `uninstall` in the drain window stops every instance that runs, the draining old
/// ones included, and removes every marker: it ends the service. Catches an `uninstall` that
/// leaves an old instance running because the record no longer names it.
#[test]
fn uninstall_in_the_drain_window_stops_the_old_instances_too() {
    let fake = drained_update(true);
    assert_ok(&fake.run(Uninstall, &params(NEW, None)));
    assert!(fake.running().is_empty());
    assert!(fake.fs_running().is_empty());
    assert!(fake.markers().is_empty());
    assert!(fake.fs_markers().is_empty());
    assert_eq!(fake.record().live, None);
}

/// The record a `cleanup` of an earlier release leaves when it gives up while the old fs instance
/// drains, key for key: committed, still in flight, a as `live` and `fs_live`, b as `next` and
/// `fs_next`; the old instances have since ended and only the markers of b are left.
fn record_left_in_flight() -> SystemdFake {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    let bytes = format!(
        r#"{{
  "from": "{OLD}",
  "fs_live": "a",
  "fs_next": "b",
  "kind": "single",
  "live": "a",
  "markers": [
    "swapped",
    "verified",
    "stopping"
  ],
  "mode": "handover",
  "next": "b",
  "schema": 1,
  "to": "{NEW}",
  "updating": true,
  "version": "{}"
}}
"#,
        version_of(NEW)
    );
    fake.set_record_bytes(Some(bytes.as_bytes()));
    fake.set_marker(Instance::A, false);
    drains_end(&fake);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.fs_markers(), [Instance::B]);
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::B]);
    fake
}

/// Asserts that `cleanup` run again with the params of the update on record finishes the record
/// an earlier release's `cleanup` left in flight: at rest, b as `live` and `fs_live`, nothing
/// started or stopped. It is the step an operator runs, with a params file naming that update,
/// on a node whose installed release is the one that left the record.
#[test]
fn cleanup_finishes_the_record_an_earlier_cleanup_left_in_flight() {
    let fake = record_left_in_flight();
    let (acts, fs_acts) = (fake.actions().len(), fake.fs_actions().len());
    let done = fake.run(Cleanup, &params(NEW, Some(OLD)));
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        "committed: tierstore@b serves, the old instance is retired"
    );
    let record = fake.record();
    assert!(!record.updating);
    assert!(record.committed());
    assert_eq!(
        (record.live, record.next, record.fs_live, record.fs_next),
        (Some(Instance::B), None, Some(Instance::B), None)
    );
    assert_eq!(fake.actions()[acts..], ["markers.clear a"]);
    assert_eq!(fake.fs_actions()[fs_acts..], ["fs_markers.clear a"]);
}

/// Asserts that the `prepare` of the next update finishes the record an earlier release's
/// `cleanup` left in flight and goes on: the update to the next release prepares from b, and runs
/// to its end. Catches a `prepare` that refuses that record (`out_of_order`), which left a node
/// that no update could reach.
#[test]
fn prepare_finishes_the_record_an_earlier_cleanup_left_in_flight_and_goes_on() {
    let fake = record_left_in_flight();
    let next = params(NEWER, Some(NEW));
    let done = fake.run(Prepare, &next);
    assert_ok(&done);
    assert!(done.report.reversible);
    assert!(
        detail(&done).starts_with(&format!(
            "finished the committed update to {}, the old instance is retired; prepared {}",
            version_of(NEW),
            version_of(NEWER)
        )),
        "{}",
        detail(&done)
    );
    let prepared = fake.record();
    assert!(prepared.updating);
    assert!(prepared.markers.is_empty());
    assert_eq!(prepared.from, Some(PathBuf::from(NEW)));
    assert_eq!(
        (
            prepared.live,
            prepared.next,
            prepared.fs_live,
            prepared.fs_next
        ),
        (
            Some(Instance::B),
            Some(Instance::A),
            Some(Instance::B),
            Some(Instance::A)
        )
    );
    assert_eq!(prepared.mode, Some(Mode::Handover));
    fake.drive(&next, &[Cutover, Verify, Cleanup]);
    assert_eq!(fake.record().live, Some(Instance::A));
    assert_eq!(fake.record().fs_live, Some(Instance::A));
}

/// Asserts that a `rollback` of another update, which hostkeeper runs after a `prepare` that
/// failed or was deferred, finds nothing to roll back in the record an earlier `cleanup` left in
/// flight, and touches nothing. Catches a `rollback` that fails it as `bad_params` after the
/// commit, which hostkeeper reports as a failed update.
#[test]
fn rollback_of_another_update_leaves_the_record_left_in_flight_alone() {
    let fake = record_left_in_flight();
    let left = fake.record();
    let done = fake.run(Rollback, &params(NEWER, Some(NEW)));
    assert_ok(&done);
    assert!(done.report.reversible);
    assert!(
        detail(&done).ends_with(&format!(
            "the record is committed, and still in flight, on the last update ({NEW} {})",
            version_of(NEW)
        )),
        "{}",
        detail(&done)
    );
    assert_eq!(fake.record(), left);
}

/// Asserts that the boot step finishes the record an earlier `cleanup` left in flight: b of each
/// unit runs and the record is at rest on b. Catches a boot step that leaves the update in flight
/// for ever.
#[test]
fn the_boot_step_finishes_the_record_an_earlier_cleanup_left_in_flight() {
    let fake = record_left_in_flight();
    let done = fake.run(Restore, &params(NEW, None));
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        "tierstore@b started; tierstore-fs@b started; the committed update is finished"
    );
    let record = fake.record();
    assert!(!record.updating);
    assert!(record.has(Marker::Stopping));
    assert_eq!(record.live, Some(Instance::B));
    assert_eq!(record.fs_live, Some(Instance::B));
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.fs_running(), [Instance::B]);
}
