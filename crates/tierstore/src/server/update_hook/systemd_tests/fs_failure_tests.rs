//! The second unit when it does not do what the hook asked: a take that fails, a unit that is not
//! active, a marker that cannot be removed, a new instance a rollback finds activating.

use std::path::PathBuf;

use crate::server::update_hook::{Instance, Marker, Reason, Record, Step, UnitState};

use super::fake::{
    FsTake, NEW, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail, params,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// The record of a node at rest on `OLD`, a and fs a serving.
fn at_rest() -> Record {
    let mut record = Record::idle(Some(Instance::A));
    record.fs_live = Some(Instance::A);
    record
}

/// Asserts that an fs instance whose start fails (it could not take the mount, the old instance
/// resumed, the unit waits to be started again) fails `cutover` with the unit's failure before
/// `swapped`, with its marker removed (so that it is not started again) and the instance not
/// stopped (it may be taking the mount in a retry), the old fs instance still serving its release
/// and never stopped; and that the `rollback` then defers (`draining`) while that instance still
/// activates, and once it has ended returns the store to a, starts and stops no fs unit, and
/// leaves the record idle on a with `fs_live` a. Catches a stop of an instance that may be taking
/// the mount, and an instance left able to start again.
#[test]
fn a_refused_take_fails_cutover_leaves_the_old_fs_instance_serving_and_rolls_back() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_take(FsTake::Refused);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let done = fake.run(Cutover, &p);
    assert_failed(&done, Reason::UnitFailed);
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear b",]
    );
    assert!(detail(&done).contains("marker was removed so that it is not started again"));
    assert_eq!(fake.fs_markers(), [Instance::A]);
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Activating);
    assert_eq!(fake.peek_fs(Instance::A), UnitState::Active);
    assert_eq!(fake.fs_release(Instance::A), Some(PathBuf::from(OLD)));
    assert!(!fake.record().has(Marker::Swapped));
    assert!(done.report.reversible);

    let units = |fake: &SystemdFake| {
        fake.fs_actions()
            .iter()
            .filter(|a| a.starts_with("fs_units."))
            .count()
    };
    let before = units(&fake);
    assert_deferred(&fake.run(Rollback, &p), Reason::Draining, 10);
    fake.set_fs_unit(Instance::B, UnitState::Failed, None);
    fake.drive(&p, &[Rollback]);
    assert_eq!(units(&fake), before, "no fs unit is started or stopped");
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.fs_running(), [Instance::A]);
    assert_eq!(fake.record(), at_rest());
}

/// Asserts that an fs instance whose start returns but whose unit is `failed` fails `cutover`
/// with `not_ready` naming `tierstore-fs@b`, removes its marker without a stop (nothing runs),
/// and leaves a serving. Catches a cutover that goes on, and a stop of a unit that is not
/// running.
#[test]
fn an_fs_unit_that_is_not_active_after_its_start_fails_cutover() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_take(FsTake::Dud);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let done = fake.run(Cutover, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(detail(&done).contains("tierstore-fs@b is Failed, not active"));
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear b"]
    );
    assert_eq!(fake.fs_running(), [Instance::A]);
    fake.drive(&p, &[Rollback]);
    assert_eq!(fake.record(), at_rest());
}

/// Asserts that a take that fails and whose marker cannot be removed reports the marker's
/// failure. Catches a failed take that reports success.
#[test]
fn a_take_that_fails_reports_a_marker_that_cannot_be_removed() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_take(FsTake::Refused);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    fake.fail_on(Some("fs_markers.clear"));
    assert_failed(&fake.run(Cutover, &p), Reason::StateIo);
}

/// Asserts that a `cutover` run again after `swapped`, which finds the store serving but the fs
/// instance down, starts the fs instance again and succeeds. Catches a re-run that reports
/// "already serves" while the fs unit is gone.
#[test]
fn a_cutover_run_again_takes_the_fs_mount_when_the_unit_is_gone() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).contains("already serves"));
    assert!(detail(&done).ends_with("; tierstore-fs@b holds the mount"));
    let starts = fake
        .fs_actions()
        .iter()
        .filter(|a| a.starts_with("fs_units.start"))
        .count();
    assert_eq!(starts, 1, "a unit that is active is not started again");

    fake.set_fs_unit(Instance::B, UnitState::Inactive, None);
    assert_ok(&fake.run(Cutover, &p));
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
}

/// Asserts that `verify` defers (`not_ready`, the params' retry) while the new fs instance is
/// activating, fails `not_ready` naming it when it is failed, and succeeds when it is active; and
/// that after the commit it looks at the build only. Catches a verify that blesses an update
/// whose fs process never took the mount.
#[test]
fn verify_needs_the_new_fs_instance_to_be_active() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_fs_unit(Instance::B, UnitState::Activating, Some(NEW));
    let done = fake.run(Verify, &p);
    assert_deferred(&done, Reason::NotReady, p.verify_retry_secs);
    assert!(detail(&done).contains("tierstore-fs@b is still activating"));
    fake.set_fs_unit(Instance::B, UnitState::Failed, None);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(detail(&done).contains("tierstore-fs@b is Failed, not active"));
    assert!(!fake.record().has(Marker::Verified));
    fake.set_fs_unit(Instance::B, UnitState::Active, Some(NEW));
    assert_ok(&fake.run(Verify, &p));
    assert!(fake.record().has(Marker::Verified));
    fake.drive(&p, &[Cleanup]);
    fake.set_fs_unit(Instance::B, UnitState::Failed, None);
    assert_ok(&fake.run(Verify, &p));
}

/// Asserts that `cleanup` fails `not_confirmed` naming the fs instance, and commits nothing, when
/// the new fs instance is not active. Catches a commit over an fs process that is down.
#[test]
fn cleanup_does_not_commit_over_an_fs_instance_that_is_down() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.set_fs_unit(Instance::B, UnitState::Inactive, None);
    let done = fake.run(Cleanup, &p);
    assert_failed(&done, Reason::NotConfirmed);
    assert!(detail(&done).contains("tierstore-fs@b is Inactive, not active"));
    assert!(!fake.record().committed());
    assert!(done.report.reversible);
}

/// Asserts that a read of a record whose `fs_next` names no `fs_live` is refused by every step
/// (`state_invalid`) before any unit is touched. Catches an update that goes on over a record the
/// hook could not have written.
#[test]
fn a_record_that_names_fs_next_without_fs_live_is_refused() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let mut record = fake.record();
    record.fs_live = None;
    fake.set_record(&record);
    assert_failed(&fake.run(Cutover, &p), Reason::StateInvalid);
    assert!(fake.fs_actions().is_empty());
}

/// Asserts that a `rollback` finding the new fs instance only activating (a retry, or a take in
/// progress) does not stop it: it removes its marker and defers with `draining`, and nothing else
/// of the fs unit is touched. Catches a stop of an instance that may be taking the mount, which
/// aborts it when the take completes.
#[test]
fn a_rollback_defers_over_a_new_fs_instance_that_is_only_activating() {
    let fake = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_fs_unit(Instance::B, UnitState::Activating, Some(NEW));
    let before = fake.fs_actions().len();
    let done = fake.run(Rollback, &p);
    assert_deferred(&done, Reason::Draining, 10);
    assert!(detail(&done).contains("tierstore-fs@b is still activating and is not stopped"));
    assert_eq!(fake.fs_actions()[before..], ["fs_markers.clear b"]);
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Activating);
}

/// Asserts that a `rollback` run again after the activating new fs instance ended finishes: when
/// it failed (it was not started again) the old instance serves and nothing is stopped; when it
/// took the mount (active) and the old instance ended, the old instance is started and takes the
/// mount back; and the record is idle on a. Catches a rollback that cannot finish after either
/// end of a take that was left to itself.
#[test]
fn a_rollback_finishes_after_the_activating_new_fs_instance_ended_either_way() {
    for took in [false, true] {
        let fake = SystemdFake::fs_installed();
        let p = params(NEW, Some(OLD));
        fake.drive(&p, &[Prepare, Cutover]);
        fake.set_fs_unit(Instance::B, UnitState::Activating, Some(NEW));
        assert_deferred(&fake.run(Rollback, &p), Reason::Draining, 10);
        if took {
            fake.set_fs_unit(Instance::A, UnitState::Inactive, None);
            fake.set_fs_unit(Instance::B, UnitState::Active, Some(NEW));
        } else {
            fake.set_fs_unit(Instance::B, UnitState::Failed, None);
        }
        fake.drive(&p, &[Rollback]);
        assert!(
            !fake
                .fs_actions()
                .iter()
                .any(|action| action.starts_with("fs_units.stop")),
            "no fs instance is stopped"
        );
        assert_eq!(fake.fs_running(), [Instance::A]);
        assert_eq!(fake.fs_markers(), [Instance::A]);
        let mut idle = Record::idle(Some(Instance::A));
        idle.fs_live = Some(Instance::A);
        assert_eq!(fake.record(), idle);
    }
}

/// Asserts that a `cutover` finding the old fs instance activating or deactivating starts no fs
/// instance and sets no marker, defers with `draining` (10 s) saying it runs again, and that once
/// the old instance is active the run after it hands the mount over. Catches a new instance
/// started over an old one that has not got the mount, which would make a second mount.
#[test]
fn a_cutover_defers_while_the_old_fs_instance_is_restarting_or_stopping() {
    for state in [UnitState::Activating, UnitState::Deactivating] {
        let fake = SystemdFake::fs_installed();
        let p = params(NEW, Some(OLD));
        fake.drive(&p, &[Prepare]);
        if state == UnitState::Deactivating {
            fake.set_fs_deactivating(Instance::A, OLD, 3);
        } else {
            fake.set_fs_unit(Instance::A, state, Some(OLD));
        }
        let done = fake.run(Cutover, &p);
        assert_deferred(&done, Reason::Draining, 10);
        assert!(detail(&done).contains("tierstore-fs@a is restarting or stopping"));
        assert!(detail(&done).contains("cutover runs again"));
        assert!(fake.fs_actions().is_empty());
        assert!(!fake.record().has(Marker::Swapped));
        fake.set_fs_unit(Instance::A, UnitState::Active, Some(OLD));
        assert_ok(&fake.run(Cutover, &p));
        assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
    }
}

/// Asserts that a `cutover` finding the old fs instance inactive or failed (its unit holds
/// nothing) starts the new instance, which makes the mount afresh and is active, and says so in
/// the detail. Catches a cutover that refuses to start over a unit that is gone, leaving the
/// node with no mount.
#[test]
fn a_cutover_makes_the_mount_afresh_when_the_old_fs_instance_does_not_run() {
    for state in [UnitState::Inactive, UnitState::Failed] {
        let fake = SystemdFake::fs_installed();
        let p = params(NEW, Some(OLD));
        fake.drive(&p, &[Prepare]);
        fake.set_fs_unit(Instance::A, state, None);
        let done = fake.run(Cutover, &p);
        assert_ok(&done);
        assert!(
            detail(&done).ends_with(
                "; tierstore-fs@a does not run, so tierstore-fs@b makes the mount afresh"
            )
        );
        assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
        assert_eq!(fake.fs_markers(), [Instance::B]);
    }
}

/// Asserts that a `cutover` run again, with the store down and the new fs instance already
/// active, starts no fs unit again and sets no marker. Catches a re-run that starts the unit of
/// an instance that holds the mount, or defers over its old instance that is still leaving.
#[test]
fn a_cutover_run_again_leaves_an_active_new_fs_instance_alone() {
    let fake = SystemdFake::fs_installed();
    fake.set_fs_drain_looks(5);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Inactive, None, false);
    assert_ok(&fake.run(Cutover, &p));
    let starts = fake
        .fs_actions()
        .iter()
        .filter(|action| action.starts_with("fs_units.start"))
        .count();
    assert_eq!(starts, 1);
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
}

/// Asserts that when the start of the new fs instance succeeded and the look at its unit then
/// fails, `cutover` reports that failure (`unit_failed`) and leaves the marker of the new
/// instance, which may hold the mount as its only holder; no stop is made. Catches a marker
/// removed from an instance that may serve.
#[test]
fn a_take_whose_look_at_the_unit_fails_keeps_the_marker() {
    let clean = SystemdFake::fs_installed();
    let p = params(NEW, Some(OLD));
    clean.drive(&p, &[Prepare]);
    let (calls, logged) = (clean.calls(), clean.log().len());
    assert_ok(&clean.run(Cutover, &p));
    let log = clean.log();
    let start = log[logged..]
        .iter()
        .position(|line| line == "fs_units.start b")
        .expect("the fs start is logged");
    assert_eq!(clean.calls() - calls, log.len() - logged);

    let fake = SystemdFake::fs_installed();
    fake.drive(&p, &[Prepare]);
    // The call after the start is the look at the unit.
    fake.halt_at(Some(start + 2));
    let done = fake.run(Cutover, &p);
    fake.halt_at(None);
    assert_failed(&done, Reason::UnitFailed);
    assert!(fake.fs_markers().contains(&Instance::B));
    assert_eq!(fake.peek_fs(Instance::B), UnitState::Active);
    assert!(
        !fake
            .fs_actions()
            .iter()
            .any(|action| action.starts_with("fs_units.stop") || action == "fs_markers.clear b")
    );
}
