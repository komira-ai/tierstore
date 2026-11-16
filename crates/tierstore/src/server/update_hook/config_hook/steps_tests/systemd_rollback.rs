//! The rollback, `uninstall` and `restore` of the config hook on systemd.

use std::collections::BTreeSet;

use super::super::super::{Instance, Marker, Reason, Record, Step, UnitState};
use super::{Linux, committed_record, first, in_flight, lin, update};
use Step::{Cutover, Prepare, Rollback, Uninstall};

/// Asserts that the rollback of a config package that has a previous one reloads the guard and
/// hands back to the instance that served, which is then alone.
#[test]
fn rollback_with_a_previous_package_reloads_the_guard_and_hands_back() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    assert_eq!(node.reloads(), 1);
    lin::assert_ok(&node.step(Rollback, &p));
    assert_eq!(node.reloads(), 2);
    assert_eq!(node.fake.running(), [Instance::A]);
    assert_eq!(node.fake.markers(), [Instance::A]);
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a guard that fails to reload fails the rollback with unit_failed and starts
/// nothing.
#[test]
fn a_guard_that_fails_to_reload_fails_the_rollback_before_any_instance_starts() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    node.guard.broken.set(true);
    let before = node.fake.actions().len();
    lin::assert_failed(&node.step(Rollback, &p), Reason::UnitFailed);
    assert_eq!(node.fake.actions().len(), before);
}

/// Asserts that the rollback of the node's first config package leaves the running instance
/// serving, clears the other's marker, writes an idle record and fails no_previous_release,
/// without touching the guard.
#[test]
fn the_first_packages_rollback_leaves_the_instance_that_runs_serving() {
    let node = Linux::alike();
    let p = first();
    node.go(&p, &[Prepare, Cutover]);
    let done = node.step(Rollback, &p);
    lin::assert_failed(&done, Reason::NoPreviousRelease);
    assert!(lin::detail(&done).contains("tierstore@b keeps serving"));
    assert_eq!(node.fake.running(), [Instance::B]);
    assert_eq!(node.fake.markers(), [Instance::B]);
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::B)));
    assert_eq!(node.reloads(), 1);
}

/// Asserts that the same rollback after `prepare` alone finds the first instance serving.
#[test]
fn the_first_packages_rollback_after_prepare_keeps_the_first_instance() {
    let node = Linux::alike();
    let p = first();
    node.go(&p, &[Prepare]);
    let done = node.step(Rollback, &p);
    lin::assert_failed(&done, Reason::NoPreviousRelease);
    assert!(lin::detail(&done).contains("tierstore@a keeps serving"));
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
    assert_eq!(node.fake.markers(), [Instance::A]);
}

/// Asserts that with no instance running the rollback takes the instance the record says served.
#[test]
fn the_first_packages_rollback_with_nothing_running_names_the_recorded_instance() {
    let node = Linux::alike();
    let p = first();
    node.go(&p, &[Prepare]);
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);
    let done = node.step(Rollback, &p);
    lin::assert_failed(&done, Reason::NoPreviousRelease);
    assert!(lin::detail(&done).contains("tierstore@a keeps serving"));
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that with nothing running and no instance recorded the failure says only that there
/// is no earlier package.
#[test]
fn the_first_packages_rollback_with_no_instance_known_names_none() {
    let node = Linux::alike();
    node.fake
        .set_unit(Instance::A, UnitState::Inactive, None, false);
    node.fake.set_record(&in_flight(None));
    let done = node.step(Rollback, &first());
    lin::assert_failed(&done, Reason::NoPreviousRelease);
    assert_eq!(
        lin::detail(&done),
        "nothing to go back to: no earlier config package"
    );
    assert_eq!(node.fake.record(), Record::idle(None));
}

/// Asserts that the first package's rollback is deferred as draining while both instances run,
/// and changes nothing.
#[test]
fn the_first_packages_rollback_waits_while_both_instances_run() {
    let node = Linux::alike();
    let p = first();
    node.fake.set_drain_looks(3);
    node.go(&p, &[Prepare, Cutover]);
    let before = node.fake.snapshot();
    lin::assert_deferred(&node.step(Rollback, &p), Reason::Draining, 10);
    assert_eq!(node.fake.snapshot(), before);
}

/// Asserts that the first package's rollback succeeds with nothing recorded.
#[test]
fn the_first_packages_rollback_with_no_record_succeeds() {
    let node = Linux::alike();
    node.fake.set_record_bytes(None);
    lin::assert_ok(&node.step(Rollback, &first()));
    assert!(node.fake.actions().is_empty());
}

/// Asserts that the first package's rollback succeeds when nothing is in flight.
#[test]
fn the_first_packages_rollback_with_nothing_in_flight_succeeds() {
    let node = Linux::alike();
    lin::assert_ok(&node.step(Rollback, &first()));
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that the first package's rollback fails committed once cleanup has begun.
#[test]
fn the_first_packages_rollback_after_cleanup_began_fails_committed() {
    let node = Linux::alike();
    node.fake.set_record(&committed_record(Instance::A));
    lin::assert_failed(&node.step(Rollback, &first()), Reason::Committed);
}

/// Asserts that the first package's rollback refuses a record that cannot be decoded or that
/// breaks the record's rules, with state_invalid.
#[test]
fn the_first_packages_rollback_refuses_a_record_that_is_not_valid() {
    let node = Linux::alike();
    node.fake.set_record_bytes(Some(b"not json"));
    lin::assert_failed(&node.step(Rollback, &first()), Reason::StateInvalid);
    node.fake.set_record(&Record {
        markers: BTreeSet::from([Marker::Swapped]),
        ..Record::idle(Some(Instance::A))
    });
    lin::assert_failed(&node.step(Rollback, &first()), Reason::StateInvalid);
}

/// Asserts that the first package's rollback fails locked while another step holds the lock.
#[test]
fn the_first_packages_rollback_fails_locked_while_another_step_runs() {
    let node = Linux::alike();
    node.fake.set_busy(true);
    lin::assert_failed(&node.step(Rollback, &first()), Reason::Locked);
}

/// Asserts that a failing port in the first package's rollback fails the step with the port's
/// reason: unit_failed for the units, state_io for the markers and the record.
#[test]
fn the_first_packages_rollback_reports_the_port_that_failed() {
    for (prefix, reason) in [
        ("units.state", Reason::UnitFailed),
        ("markers.clear", Reason::StateIo),
        ("state.save", Reason::StateIo),
        ("state.load", Reason::StateIo),
    ] {
        let node = Linux::alike();
        let p = first();
        node.go(&p, &[Prepare]);
        node.fake.fail_on(Some(prefix));
        lin::assert_failed(&node.step(Rollback, &p), reason);
    }
}

/// Asserts that `uninstall` succeeds and changes nothing: no unit, marker, record or guard.
#[test]
fn uninstall_changes_nothing_and_succeeds() {
    let node = Linux::alike();
    let p = update();
    node.go(&p, &[Prepare, Cutover]);
    let (snapshot, service, reloads) = (node.fake.snapshot(), node.service.get(), node.reloads());
    let done = node.step(Uninstall, &p);
    lin::assert_ok(&done);
    assert_eq!(
        lin::detail(&done),
        "uninstall: the service keeps the configuration it read"
    );
    assert_eq!(node.fake.snapshot(), snapshot);
    assert_eq!(node.service.get(), service);
    assert_eq!(node.reloads(), reloads);
}

/// Asserts that `restore` is not a step of the config hook: bad_args.
#[test]
fn restore_is_refused_as_not_a_step() {
    let node = Linux::alike();
    let before = node.fake.snapshot();
    let done = node.step(Step::Restore, &update());
    lin::assert_failed(&done, Reason::BadArgs);
    assert!(lin::detail(&done).contains("restore is not a step of config-hook"));
    assert_eq!(node.fake.snapshot(), before);
}
