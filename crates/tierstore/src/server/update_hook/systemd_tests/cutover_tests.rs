//! `cutover`, `verify` and `cleanup`: the modes, the runs again, and every way they fail.

use crate::server::update_hook::{
    Clock, Instance, Marker, Mode, Params, READY_ATTEMPTS, Reason, Record, Step, UnitState,
};

use super::fake::{
    CONFIG, NEW, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail, params,
    params_with, version_of,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

fn restart_params() -> Params {
    params_with(
        NEW,
        Some(OLD),
        &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
    )
}

/// Asserts that an update in restart mode stops the old instance, and waits for its drain, before
/// it lays down the marker of the new one and starts it, and that `cleanup` then only removes the
/// old marker. Catches a restart that starts the new instance while the old one still serves.
#[test]
fn a_restart_update_stops_the_old_instance_before_it_starts_the_new() {
    let fake = SystemdFake::installed();
    let p = restart_params();
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().mode, Some(Mode::Restart));
    fake.drive(&p, &[Cutover]);
    assert_eq!(
        fake.actions(),
        ["units.stop a", "markers.set b", "units.start b"]
    );
    assert_eq!(fake.peek(Instance::A), UnitState::Inactive);
    fake.drive(&p, &[Verify, Cleanup]);
    assert_eq!(
        fake.actions().last().map(String::as_str),
        Some("markers.clear a")
    );
    assert_eq!(fake.running(), [Instance::B]);
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts that a hand-over the instances cannot carry out (the new one cannot bind) fails the
/// `cutover` with `probe_failed` while the old instance still serves, still reversible, and that
/// the `rollback` that follows finds the old instance serving and only clears the new marker.
#[test]
fn a_hand_over_that_cannot_bind_leaves_the_old_instance_serving() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    fake.set_config(None);
    let done = fake.run(Cutover, &p);
    assert_failed(&done, Reason::ProbeFailed);
    assert!(done.report.reversible);
    assert_eq!(fake.peek(Instance::A), UnitState::Active);
    assert_eq!(fake.peek(Instance::B), UnitState::Failed);
    assert!(!fake.record().has(Marker::Swapped));

    let before = fake.actions().len();
    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(fake.actions()[before..], ["markers.clear b"]);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
    assert_eq!(fake.running(), [Instance::A]);
}

/// Asserts that a new release that never answers `/readyz` fails the `cutover` with `not_ready`
/// after the looks it allows, and that the `rollback` hostkeeper then runs puts the previous
/// release back on a. Catches a cutover that reports success without a probe.
#[test]
fn a_release_that_never_answers_fails_cutover_with_not_ready_and_can_be_rolled_back() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    fake.mute(NEW);
    let done = fake.run_as_hostkeeper(Cutover, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(done.report.reversible);
    assert_eq!(Clock::now_ms(&fake), u64::from(READY_ATTEMPTS) * 1000);
    assert!(!fake.record().has(Marker::Swapped));

    assert_ok(&fake.run_as_hostkeeper(Rollback, &p));
    assert_eq!(fake.running(), [Instance::A]);
    assert_eq!(fake.markers(), [Instance::A]);
    assert_eq!(fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that a new release that answers as another build fails the `cutover` with
/// `probe_failed` and does not record `swapped`.
#[test]
fn a_release_that_answers_as_another_build_fails_cutover_with_probe_failed() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    fake.lie(NEW, "9.9.9+0123456789ab");
    let done = fake.run(Cutover, &p);
    assert_failed(&done, Reason::ProbeFailed);
    assert!(!fake.record().has(Marker::Swapped));
}

/// Asserts that a `cutover` run again after it succeeded finds the new instance serving and does
/// nothing: no unit or marker call and no write.
#[test]
fn a_cutover_run_again_finds_the_new_instance_serving_and_does_nothing() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let before = fake.log().len();
    let done = fake.run(Cutover, &p);
    assert_ok(&done);
    assert!(detail(&done).contains("already serves"), "{done:?}");
    let added = &fake.log()[before..];
    assert!(!added.iter().any(|line| line.starts_with("units.start")
        || line.starts_with("units.stop")
        || line.starts_with("markers")
        || line == "state.save"));
}

/// Asserts that a `cutover` run again after the new instance died starts it again, and one run
/// again after it came up as another build redoes the start and reports the probe: neither
/// writes `swapped` a second time.
#[test]
fn a_cutover_run_again_starts_a_new_instance_that_died_or_is_the_wrong_build() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Failed, None, false);
    let before = fake.log().len();
    assert_ok(&fake.run(Cutover, &p));
    assert_eq!(fake.peek(Instance::B), UnitState::Active);
    assert!(
        fake.log()[before..]
            .iter()
            .any(|line| line == "units.start b")
    );
    assert!(!fake.log()[before..].iter().any(|line| line == "state.save"));

    fake.lie(NEW, "9.9.9+0123456789ab");
    assert_failed(&fake.run(Cutover, &p), Reason::ProbeFailed);
    let starts = fake
        .actions()
        .iter()
        .filter(|line| *line == "units.start b")
        .count();
    assert_eq!(starts, 3);
}

/// Asserts that each failing port of `cutover` is reported with the reason of its kind: a unit
/// that cannot be started or stopped is `unit_failed`, a marker or the record that cannot be
/// written is `state_io`.
#[test]
fn a_cutover_whose_ports_fail_reports_the_port() {
    let cases = [
        ("units.start", Reason::UnitFailed, false),
        ("markers.set", Reason::StateIo, false),
        ("units.stop", Reason::UnitFailed, true),
        ("state.save", Reason::StateIo, false),
    ];
    for (prefix, reason, restart) in cases {
        let fake = SystemdFake::installed();
        let p = if restart {
            restart_params()
        } else {
            params(NEW, Some(OLD))
        };
        fake.drive(&p, &[Prepare]);
        fake.fail_on(Some(prefix));
        assert_failed(&fake.run_as_hostkeeper(Cutover, &p), reason);
    }
}

/// Asserts that `verify` after the commit watches the promotion: it reads the build and says
/// `promotion_pending` while the cache is read-only, and fails `wrong_build` or `not_ready`
/// when the build is another or nothing answers.
#[test]
fn verify_after_the_commit_watches_the_promotion() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    fake.set_read_only(true);
    let pending = fake.run(Verify, &p);
    assert_ok(&pending);
    assert!(pending.report.promotion_pending);
    assert!(!pending.report.reversible);
    fake.set_read_only(false);
    let promoted = fake.run(Verify, &p);
    assert_ok(&promoted);
    assert!(!promoted.report.promotion_pending);

    fake.lie(NEW, "9.9.9+0123456789ab");
    assert_failed(&fake.run(Verify, &p), Reason::WrongBuild);
    fake.mute(NEW);
    assert_failed(&fake.run(Verify, &p), Reason::NotReady);
}

/// Asserts that `verify` before the commit records `verified` and passes the pending promotion
/// on, and run again writes nothing more.
#[test]
fn verify_records_verified_and_run_again_writes_nothing() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_read_only(true);
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert!(done.report.promotion_pending);
    assert!(fake.record().has(Marker::Verified));
    assert!(detail(&done).contains(&version_of(NEW)));

    let before = fake.log().len();
    assert_ok(&fake.run(Verify, &p));
    assert!(!fake.log()[before..].iter().any(|line| line == "state.save"));
}

/// Asserts that `verify` needs the new instance to be active and the release to answer: an
/// instance that is only activating, or a release that is mute, is `not_ready`; ports that fail
/// are `unit_failed` and `state_io`.
#[test]
fn verify_fails_when_the_new_instance_is_not_active_or_not_answering() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Activating, Some(NEW), true);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::NotReady);
    assert!(detail(&done).contains("Activating"), "{done:?}");

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.mute(NEW);
    assert_failed(&fake.run(Verify, &p), Reason::NotReady);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.fail_on(Some("units.state"));
    assert_failed(&fake.run(Verify, &p), Reason::UnitFailed);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.fail_on(Some("state.save"));
    assert_failed(&fake.run(Verify, &p), Reason::StateIo);
}

/// Asserts that `cleanup` before a successful `verify` fails with `not_verified` and commits
/// nothing.
#[test]
fn cleanup_before_verify_fails_with_not_verified() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover]);
    let done = fake.run(Cleanup, &p);
    assert_failed(&done, Reason::NotVerified);
    assert!(done.report.reversible);
    assert!(!fake.record().committed());
}

/// Asserts that a `cleanup` that cannot confirm afresh that the new release serves (the instance
/// is not active, the release is mute, or it reports another build) fails with `not_confirmed`,
/// writes no `stopping` and stays reversible.
#[test]
fn cleanup_that_cannot_confirm_commits_nothing() {
    let p = params(NEW, Some(OLD));
    for case in 0..3 {
        let fake = SystemdFake::installed();
        fake.drive(&p, &[Prepare, Cutover, Verify]);
        match case {
            0 => fake.set_unit(Instance::B, UnitState::Failed, None, false),
            1 => fake.mute(NEW),
            _ => fake.lie(NEW, "9.9.9+0123456789ab"),
        }
        let done = fake.run(Cleanup, &p);
        assert_failed(&done, Reason::NotConfirmed);
        assert!(done.report.reversible, "{case}");
        assert!(!fake.record().committed(), "{case}");
        assert!(
            !fake
                .actions()
                .iter()
                .any(|line| line.starts_with("markers.clear"))
        );
    }
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.lie(NEW, "9.9.9+0123456789ab");
    assert!(detail(&fake.run(Cleanup, &p)).contains("was not seen serving"));
}

/// Asserts that a write of `stopping` that fails before it reaches the disk leaves the update
/// reversible (the record read back says no commit), and one that fails after it reached the
/// disk is the commit it is: `reversible` false, the record says `stopping`.
#[test]
fn a_failed_write_of_stopping_is_read_back_to_decide_the_commit() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.fail_on(Some("state.save"));
    let lost = fake.run(Cleanup, &p);
    assert_failed(&lost, Reason::StateIo);
    assert!(lost.report.reversible);
    assert!(!fake.record().committed());

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.late_on(Some("state.save"));
    let made = fake.run(Cleanup, &p);
    assert_failed(&made, Reason::StateIo);
    assert!(!made.report.reversible);
    assert!(fake.record().committed());
    fake.late_on(None);
    assert_ok(&fake.run(Cleanup, &p));
    assert!(!fake.record().updating);
}

/// Asserts that a `cleanup` that cannot remove the old marker, or cannot look at the old unit,
/// fails after the commit with `reversible` false, and that run again it finishes.
#[test]
fn cleanup_after_the_commit_finishes_when_run_again() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.fail_on(Some("markers.clear"));
    let stuck = fake.run(Cleanup, &p);
    assert_failed(&stuck, Reason::StateIo);
    assert!(!stuck.report.reversible);
    assert!(fake.record().updating);
    fake.fail_on(Some("units.state"));
    let blind = fake.run(Cleanup, &p);
    assert_failed(&blind, Reason::UnitFailed);
    assert!(!blind.report.reversible);
    fake.fail_on(None);
    assert_ok(&fake.run(Cleanup, &p));
    assert_eq!(fake.record().live, Some(Instance::B));
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts that a `cleanup` run after it finished says there is nothing left to clean, and
/// stays irreversible.
#[test]
fn cleanup_run_after_it_finished_has_nothing_left_to_clean() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    let done = fake.run(Cleanup, &p);
    assert_ok(&done);
    assert!(detail(&done).contains("nothing left to clean"), "{done:?}");
    assert!(!done.report.reversible);
}

/// Asserts that on the committed record of a finished update `verify` and `cleanup` succeed and
/// `rollback` fails `committed`, and all three report `reversible` false. Catches a flag taken
/// from the in-flight state instead of the record as it stands.
#[test]
fn steps_run_on_a_finished_update_report_it_irreversible() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    let verify = fake.run(Verify, &p);
    assert_ok(&verify);
    assert!(!verify.report.reversible);
    let cleanup = fake.run(Cleanup, &p);
    assert_ok(&cleanup);
    assert!(!cleanup.report.reversible);
    let rollback = fake.run(Rollback, &p);
    assert_failed(&rollback, Reason::Committed);
    assert!(!rollback.report.reversible);
}

/// Asserts that while the old instance still drains `cleanup` defers (exit 75, reason draining,
/// not reversible) and a later run finishes. Catches a cleanup that removes the marker of an
/// instance that still runs.
#[test]
fn cleanup_defers_while_the_old_instance_drains_and_a_later_run_finishes() {
    let fake = SystemdFake::installed();
    fake.set_drain_looks(2);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);

    let first = fake.run(Cleanup, &p);
    assert_deferred(&first, Reason::Draining, 10);
    assert!(!first.report.reversible);
    assert!(fake.record().committed());
    assert!(fake.record().updating);
    assert_eq!(fake.markers(), [Instance::A, Instance::B]);
    assert_failed(&fake.run(Rollback, &p), Reason::Committed);

    let second = fake.run(Cleanup, &p);
    assert_deferred(&second, Reason::Draining, 10);
    assert_ok(&fake.run(Cleanup, &p));
    let finished = fake.record();
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.running(), [Instance::B]);
}
