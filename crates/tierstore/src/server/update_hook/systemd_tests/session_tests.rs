//! What every step does before it acts: the lock, the record, and the params that must be those
//! of the update on record.

use std::path::PathBuf;

use crate::server::update_hook::{Instance, Marker, Mode, Reason, Record, Step};

use super::fake::{
    NEW, NEWER, OLD, SystemdFake, assert_failed, assert_ok, detail, params, version_of,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// Asserts that every step finds the lock taken and fails with `locked` (class environment)
/// having made no other call.
#[test]
fn every_step_fails_with_locked_when_another_step_holds_the_lock() {
    for step in Step::ALL {
        let fake = SystemdFake::installed();
        fake.set_busy(true);
        let done = fake.run(step, &params(NEW, Some(OLD)));
        assert_failed(&done, Reason::Locked);
        assert_eq!(fake.log(), ["state.lock"], "{step}");
    }
}

/// Asserts that a record that is not JSON of this hook's schema is `state_invalid` for every
/// step that reads it, and that `uninstall`, which replaces the record, does not read it.
#[test]
fn a_corrupt_record_fails_every_step_but_uninstall() {
    for step in Step::ALL {
        let fake = SystemdFake::installed();
        fake.set_record_bytes(Some(b"{ not json"));
        let done = fake.run(step, &params(NEW, Some(OLD)));
        if step == Step::Uninstall {
            assert_ok(&done);
            assert_eq!(fake.record(), Record::idle(None));
        } else {
            assert_failed(&done, Reason::StateInvalid);
        }
    }
}

/// Asserts that a record that breaks its own rules (`verified` without `swapped`) is
/// `state_invalid`, and that a record that cannot be read from the state directory is
/// `state_io`.
#[test]
fn a_record_out_of_rule_and_a_record_that_cannot_be_read_are_refused() {
    let fake = SystemdFake::installed();
    let mut record = Record::idle(Some(Instance::A));
    record.markers.insert(Marker::Verified);
    fake.set_record(&record);
    assert_failed(
        &fake.run(Cutover, &params(NEW, Some(OLD))),
        Reason::StateInvalid,
    );

    let fake = SystemdFake::installed();
    fake.fail_on(Some("state.load"));
    assert_failed(&fake.run(Cutover, &params(NEW, Some(OLD))), Reason::StateIo);
}

/// Asserts that a record of an update in flight that names no instance to start is
/// `state_invalid` for `cutover` and `rollback`, which start or stop it.
#[test]
fn an_update_on_record_without_a_next_instance_is_state_invalid() {
    for step in [Cutover, Rollback] {
        let fake = SystemdFake::installed();
        let mut record = Record::idle(Some(Instance::A));
        record.updating = true;
        record.from = Some(PathBuf::from(OLD));
        record.to = Some(PathBuf::from(NEW));
        record.version = Some(version_of(NEW));
        record.mode = Some(Mode::Handover);
        fake.set_record(&record);
        assert_failed(
            &fake.run(step, &params(NEW, Some(OLD))),
            Reason::StateInvalid,
        );
    }
}

/// Asserts that `cutover`, `verify` and `cleanup` with no update on record, or with one that
/// has not reached them, fail with `out_of_order`; and that `rollback` with nothing recorded
/// succeeds with nothing to do.
#[test]
fn steps_out_of_their_order_fail_with_out_of_order() {
    let p = params(NEW, Some(OLD));
    for step in [Cutover, Verify, Cleanup] {
        let fake = SystemdFake::empty();
        assert_failed(&fake.run(step, &p), Reason::OutOfOrder);
        let fake = SystemdFake::installed();
        assert_failed(&fake.run(step, &p), Reason::OutOfOrder);
    }

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare]);
    assert_failed(&fake.run(Verify, &p), Reason::OutOfOrder);

    for fake in [SystemdFake::empty(), SystemdFake::installed()] {
        let done = fake.run(Rollback, &p);
        assert_ok(&done);
        assert!(detail(&done).contains("nothing to roll back"), "{done:?}");
        assert!(fake.actions().is_empty());
    }
}

/// Asserts that a committed update refuses a `cutover`: `out_of_order`.
#[test]
fn cutover_after_the_commit_is_out_of_order() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    assert_failed(&fake.run(Cutover, &p), Reason::OutOfOrder);

    let fake = SystemdFake::installed();
    fake.set_drain_looks(1);
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    assert_eq!(fake.run(Cleanup, &p).outcome.code(), 75);
    assert_failed(&fake.run(Cutover, &p), Reason::OutOfOrder);
}

/// Asserts that a step after `prepare` whose params name another release, or the same release
/// with another version, than the update on record fails with `bad_params` and does nothing,
/// while the update is in flight and after it is committed.
#[test]
fn params_of_another_update_are_refused_with_bad_params() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    let mut other_version = p.clone();
    other_version.target_version = "9.9.9".to_owned();
    for other in [params(NEWER, Some(OLD)), other_version] {
        for step in [Cutover, Verify, Rollback, Cleanup] {
            let before = fake.log().len();
            assert_failed(&fake.run(step, &other), Reason::BadParams);
            assert!(fake.actions().is_empty(), "{step}");
            assert!(!fake.log()[before..].iter().any(|line| line == "state.save"));
        }
    }

    let fake = SystemdFake::installed();
    fake.complete(&p);
    assert_failed(
        &fake.run(Verify, &params(NEWER, Some(NEW))),
        Reason::BadParams,
    );
}
