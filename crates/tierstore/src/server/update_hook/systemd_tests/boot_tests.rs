//! `restore`, the boot step, and `uninstall`; and the unit file's condition the boot rests on.

use crate::server::update_hook::{Instance, Reason, Record, Step, UnitState, marker_path};

use super::fake::{
    NEW, OLD, SystemdFake, assert_failed, assert_ok, detail, params, unit_condition,
};

use Step::{Cutover, Prepare, Restore, Uninstall};

/// Asserts that after a reboot of a host whose update finished, `restore` starts exactly one
/// unit, the one the record names, and that the other has no marker.
#[test]
fn restore_after_a_reboot_starts_exactly_one_unit() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    fake.reboot();
    assert_eq!(fake.running(), [Instance::B]);

    let before = fake.actions().len();
    let done = fake.run(Restore, &p);
    assert_ok(&done);
    assert_eq!(detail(&done), "tierstore@b started");
    assert_eq!(
        fake.actions()[before..],
        ["markers.clear a", "markers.set b", "units.start b"]
    );
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts that `restore` with no record, or with a record that names no serving instance (a
/// host after `uninstall`), starts nothing.
#[test]
fn restore_with_nothing_recorded_starts_nothing() {
    let p = params(NEW, None);
    let fake = SystemdFake::empty();
    assert_ok(&fake.run(Restore, &p));
    assert!(fake.actions().is_empty());

    let fake = SystemdFake::installed();
    assert_ok(&fake.run(Uninstall, &p));
    let done = fake.run(Restore, &p);
    assert_ok(&done);
    assert!(detail(&done).contains("nothing recorded"), "{done:?}");
    assert!(fake.running().is_empty());
}

/// Asserts that `restore` stops an instance that runs although the record names the other, and
/// starts the recorded one.
#[test]
fn restore_stops_the_instance_the_record_does_not_name() {
    let fake = SystemdFake::installed();
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    fake.set_unit(Instance::B, UnitState::Active, Some(OLD), true);
    assert_ok(&fake.run(Restore, &params(NEW, None)));
    assert_eq!(
        fake.actions(),
        [
            "units.stop b",
            "markers.clear b",
            "markers.set a",
            "units.start a"
        ]
    );
    assert_eq!(fake.running(), [Instance::A]);
}

/// Asserts that during an update the boot step follows the record: after `cutover` it keeps the
/// new instance, and after `prepare` alone the old one.
#[test]
fn restore_during_an_update_keeps_the_instance_the_record_says_serves() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.reboot();
    assert_ok(&fake.run(Restore, &p));
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare]);
    fake.reboot();
    assert_ok(&fake.run(Restore, &p));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
}

/// Asserts that each failing port of `restore` is reported with the reason of its kind.
#[test]
fn restore_reports_the_port_that_fails() {
    for (prefix, reason) in [
        ("units.state", Reason::UnitFailed),
        ("markers.clear", Reason::StateIo),
        ("markers.set", Reason::StateIo),
        ("units.start", Reason::UnitFailed),
    ] {
        let fake = SystemdFake::installed();
        fake.fail_on(Some(prefix));
        assert_failed(&fake.run(Restore, &params(NEW, None)), reason);
    }
    let fake = SystemdFake::installed();
    fake.set_unit(Instance::B, UnitState::Active, Some(OLD), true);
    fake.fail_on(Some("units.stop"));
    assert_failed(&fake.run(Restore, &params(NEW, None)), Reason::UnitFailed);
}

/// Asserts that the unit file's `ConditionPathExists=` names the marker the hook lays down,
/// for each instance: the file the package ships and `marker_path` agree.
#[test]
fn the_unit_files_condition_names_the_marker_of_each_instance() {
    assert_eq!(unit_condition(), "/var/lib/tierstore/run-%i");
    for instance in Instance::ALL {
        assert_eq!(
            std::path::PathBuf::from(unit_condition().replace("%i", instance.letter())),
            marker_path(instance)
        );
    }
}

/// Asserts that at boot, with both instances enabled, only the instance the record names starts:
/// the unit file's condition keeps the other down.
#[test]
fn at_boot_only_the_instance_with_a_marker_starts() {
    let fake = SystemdFake::installed();
    fake.complete(&params(NEW, Some(OLD)));
    fake.reboot();
    assert_eq!(fake.running(), [Instance::B]);
    assert!(fake.log().iter().any(|line| line == "skipped a: no marker"));
    assert_eq!(fake.peek(Instance::A), UnitState::Inactive);
}

/// Asserts that `restore` repairs a marker that disagrees with the record: with the marker on
/// the wrong instance the host boots the wrong one, and after `restore` only the recorded one
/// has a marker and runs.
#[test]
fn restore_repairs_a_marker_that_disagrees_with_the_record() {
    let fake = SystemdFake::installed();
    fake.set_marker(Instance::A, false);
    fake.set_marker(Instance::B, true);
    fake.reboot();
    assert_eq!(fake.running(), [Instance::B]);

    assert_ok(&fake.run(Restore, &params(NEW, None)));
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.peek(Instance::A), UnitState::Active);
}

/// Asserts that `uninstall` stops both instances and removes both markers, makes no other call
/// than those and the record's (the data directory, the configuration and the record directory
/// are out of its reach), and leaves a record that says nothing is in flight and nothing serves.
#[test]
fn uninstall_stops_both_instances_and_removes_only_the_markers() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.log().len();
    assert_ok(&fake.run(Uninstall, &p));
    assert_eq!(
        fake.actions()[fake.actions().len() - 4..],
        [
            "units.stop a",
            "units.stop b",
            "markers.clear a",
            "markers.clear b"
        ]
    );
    let calls = &fake.log()[before..];
    assert!(calls.iter().all(|line| line.starts_with("units.")
        || line.starts_with("markers.")
        || line.starts_with("state.")));
    assert!(fake.running().is_empty());
    assert!(fake.markers().is_empty());
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that each failing port of `uninstall` is reported with the reason of its kind.
#[test]
fn uninstall_reports_the_port_that_fails() {
    for (prefix, reason) in [
        ("units.stop", Reason::UnitFailed),
        ("markers.clear", Reason::StateIo),
        ("state.save", Reason::StateIo),
    ] {
        let fake = SystemdFake::installed();
        fake.fail_on(Some(prefix));
        assert_failed(&fake.run(Uninstall, &params(NEW, None)), reason);
    }
}
