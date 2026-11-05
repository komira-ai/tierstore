//! `rollback`: the way back in each mode, the waits for a drain, and what it refuses.

use crate::server::update_hook::{Instance, Reason, Record, Step, UnitState};

use super::fake::{
    CONFIG, NEW, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail, params,
    params_with,
};

use Step::{Cutover, Prepare, Rollback, Verify};

/// Asserts that in restart mode the rollback stops the new instance, lays down the marker of the
/// old one and starts it (it runs the previous release, which hostkeeper has linked again), and
/// records nothing in flight with the old instance live.
#[test]
fn a_restart_rollback_stops_the_new_instance_and_starts_the_old() {
    let fake = SystemdFake::installed();
    let p = params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.actions().len();
    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(
        fake.actions()[before..],
        [
            "units.stop b",
            "markers.set a",
            "units.start a",
            "markers.clear b"
        ]
    );
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
}

/// Asserts that a rollback after `prepare` alone (nothing started) finds the old instance
/// serving, starts nothing and clears the record.
#[test]
fn a_rollback_after_prepare_starts_nothing() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert!(!fake.actions().iter().any(|line| line.starts_with("units.")));
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a hand-over rollback waits while the old instance still drains (exit 75, reason
/// draining, still reversible), then starts it again, waits while the new one drains in turn,
/// and finishes with the old release serving on the old letter and nothing in flight. Catches a
/// rollback that starts an instance on top of one that still runs.
#[test]
fn a_hand_over_rollback_waits_for_both_drains_and_then_finishes() {
    let fake = SystemdFake::installed();
    fake.set_drain_looks(3);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);

    let mut waits = 0;
    let done = loop {
        let done = fake.run_as_hostkeeper(Rollback, &p);
        if done.outcome.code() != 75 {
            break done;
        }
        assert_deferred(&done, Reason::Draining, 10);
        assert!(done.report.reversible);
        waits += 1;
        assert!(waits < 20, "the rollback never finished");
    };
    assert_ok(&done);
    assert!(waits >= 2, "both drains were waited for: {waits}");
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.peek(Instance::B), UnitState::Inactive);
}

/// Asserts that when the new instance died and the old one runs but does not serve (it handed
/// over to an instance that died, and exits), the rollback defers with `draining`.
#[test]
fn a_rollback_defers_while_the_old_instance_runs_but_does_not_serve() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Failed, None, false);
    fake.set_unit(Instance::A, UnitState::Active, Some(OLD), false);
    let done = fake.run_as_hostkeeper(Rollback, &p);
    assert_deferred(&done, Reason::Draining, 10);
    assert!(done.report.reversible);
    assert!(fake.record().updating);
}

/// Asserts that when neither instance runs the rollback cold-starts the old one on the previous
/// release.
#[test]
fn a_rollback_with_nothing_running_cold_starts_the_old_instance() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::A, UnitState::Inactive, None, false);
    fake.set_unit(Instance::B, UnitState::Failed, None, false);
    let done = fake.run_as_hostkeeper(Rollback, &p);
    assert_ok(&done);
    assert!(detail(&done).contains("tierstore@a"), "{done:?}");
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a rollback whose previous release does not serve after it started (the release
/// is mute) fails with `not_ready` and leaves the update on record.
#[test]
fn a_rollback_whose_previous_release_does_not_serve_fails_with_not_ready() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.mute(OLD);
    let done = fake.run_as_hostkeeper(Rollback, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(fake.record().updating);
}

/// Asserts that a rollback of an update whose params name no previous release is `bad_params`.
#[test]
fn a_rollback_without_a_previous_release_in_the_params_is_bad_params() {
    let fake = SystemdFake::installed();
    fake.drive(&params(NEW, Some(OLD)), &[Prepare]);
    assert_failed(&fake.run(Rollback, &params(NEW, None)), Reason::BadParams);
}

/// Asserts that each failing port of `rollback` is reported with the reason of its kind.
#[test]
fn a_rollback_whose_ports_fail_reports_the_port() {
    let cases = [
        ("units.start", Reason::UnitFailed),
        ("markers.set", Reason::StateIo),
        ("markers.clear", Reason::StateIo),
        ("state.save", Reason::StateIo),
        ("units.state", Reason::UnitFailed),
    ];
    for (prefix, reason) in cases {
        let fake = SystemdFake::installed();
        let p = params(NEW, Some(OLD));
        fake.drive(&p, &[Prepare, Cutover]);
        fake.fail_on(Some(prefix));
        assert_failed(&fake.run_as_hostkeeper(Rollback, &p), reason);
    }
    let fake = SystemdFake::installed();
    let p = params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    );
    fake.drive(&p, &[Prepare, Cutover]);
    fake.fail_on(Some("units.stop"));
    assert_failed(&fake.run_as_hostkeeper(Rollback, &p), Reason::UnitFailed);
}

/// Asserts that the rollback of a first install reports the ports that fail: the stop of the
/// instance, the removal of its marker and the write of the cleared record.
#[test]
fn the_rollback_of_a_first_install_reports_the_port_that_fails() {
    for (prefix, reason) in [
        ("units.stop", Reason::UnitFailed),
        ("markers.clear", Reason::StateIo),
        ("state.save", Reason::StateIo),
    ] {
        let fake = SystemdFake::empty();
        let p = params(NEW, None);
        fake.drive(&p, &[Prepare, Cutover]);
        fake.fail_on(Some(prefix));
        assert_failed(&fake.run(Rollback, &p), reason);
    }
}

/// Asserts that an update that began on a host where nothing served (no record, no instance)
/// is rolled back onto the other letter than the one `cutover` started: it has no live instance
/// to return to, so the previous release starts on b.
#[test]
fn a_rollback_of_an_update_that_began_with_no_live_instance_starts_the_other_letter() {
    let fake = SystemdFake::empty();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    assert_eq!(fake.record().live, None);
    assert_eq!(fake.record().next, Some(Instance::A));

    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(fake.record(), Record::idle(Some(Instance::B)));
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
}
