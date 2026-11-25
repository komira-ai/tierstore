//! `cutover`, `verify` and `cleanup`: the modes, the runs again, and every way they fail.

use std::ffi::OsString;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

use crate::server::update_hook::{
    Clock, Instance, Invocation, Machine, Marker, Mode, Params, READY_ATTEMPTS, Reason, Record,
    Step, UnitState, run,
};

use super::fake::{
    CONFIG, NEW, OLD, SystemdFake, assert_deferred, assert_failed, assert_ok, detail, params,
    params_text, params_with, version_of,
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
/// `promotion_pending` while the cache is read-only, fails `wrong_build` when the build is
/// another, and defers `not_ready` when nothing answers `/readyz`. This is an intended change: a
/// look not answered after the commit failed `not_ready`.
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
    assert_deferred(&fake.run(Verify, &p), Reason::NotReady, 5);
}

/// Asserts the `verify` that hostkeeper runs again for a pending promotion after `cleanup`, while
/// the old instance still drains and answers `/readyz` with 503 on the sockets the two share:
/// that look defers `not_ready` with `verify_retry_secs`, not reversible, the update stays
/// committed, and the next `verify` passes. Catches the update that failed at its `verify` after
/// `cleanup` (`port 9300 does not answer /readyz`) while the new release served.
#[test]
fn verify_after_cleanup_defers_past_the_draining_503() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.complete(&p);
    assert!(fake.record().committed());
    fake.set_read_only(true);
    fake.answer_draining(1);
    let done = fake.run(Verify, &p);
    assert_deferred(&done, Reason::NotReady, 5);
    assert_eq!(
        detail(&done),
        "port 9300 does not answer /readyz yet",
        "{done:?}"
    );
    assert!(!done.report.reversible);
    assert!(fake.record().committed());
    let again = fake.run(Verify, &p);
    assert_ok(&again);
    assert!(again.report.promotion_pending);
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

/// Asserts that `verify` defers, and does not fail, while the new instance is only activating
/// (it waits for the cache lock the old instance holds until that drains) or the release does
/// not answer yet: the answer is `not_ready` with the params' `verify_retry_secs`, 5 by default
/// and as set when given, and the record is left as it was. This is an intended change: it
/// failed `not_ready` and rolled the update back.
#[test]
fn verify_defers_while_the_new_instance_is_activating_or_not_answering() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.set_unit(Instance::B, UnitState::Activating, Some(NEW), true);
    let before = fake.record();
    let done = fake.run(Verify, &p);
    assert_deferred(&done, Reason::NotReady, 5);
    assert!(detail(&done).contains("activating"), "{done:?}");
    assert_eq!(fake.record(), before);

    let tuned = params_with(NEW, Some(OLD), "verify_retry_secs = 9\n");
    assert_deferred(&fake.run(Verify, &tuned), Reason::NotReady, 9);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover]);
    fake.mute(NEW);
    let done = fake.run(Verify, &p);
    assert_deferred(&done, Reason::NotReady, 5);
    assert!(detail(&done).contains("/readyz"), "{done:?}");
    assert!(!fake.record().has(Marker::Verified));
}

/// Asserts that `verify` still fails `not_ready` when the new instance's unit is `failed`,
/// `inactive` or `deactivating`, which no wait repairs, and that ports that fail are
/// `unit_failed` and `state_io`.
#[test]
fn verify_fails_when_the_new_instance_is_failed_inactive_or_stopping() {
    let p = params(NEW, Some(OLD));
    for state in [
        UnitState::Failed,
        UnitState::Inactive,
        UnitState::Deactivating,
    ] {
        let fake = SystemdFake::installed();
        fake.drive(&p, &[Prepare, Cutover]);
        fake.set_unit(Instance::B, state, Some(NEW), true);
        let done = fake.run(Verify, &p);
        assert_failed(&done, Reason::NotReady);
        assert!(detail(&done).contains("not active"), "{done:?}");
    }

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

/// Asserts that while the old instance still drains `cleanup` commits and finishes at once (exit
/// 0, not reversible): the record is at rest with b live, the marker of a is gone while a drains
/// on, a is never stopped, a `rollback` is refused, and a run again has nothing left to clean.
/// Catches a `cleanup` whose verdict waits for the drain, which hostkeeper retries only
/// `cleanup_retries` times and then reports as a failed update.
#[test]
fn cleanup_finishes_while_the_old_instance_drains_and_never_stops_it() {
    let fake = SystemdFake::installed();
    fake.set_drain_looks(50);
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare, Cutover, Verify]);

    let first = fake.run(Cleanup, &p);
    assert_ok(&first);
    assert_eq!(
        detail(&first),
        "committed: tierstore@b serves; tierstore@a still drains and ends by itself"
    );
    assert!(!first.report.reversible);
    let finished = fake.record();
    assert!(finished.committed());
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));
    assert_eq!(finished.next, None);
    assert_eq!(fake.markers(), [Instance::B]);
    assert_eq!(fake.peek(Instance::A), UnitState::Deactivating);
    assert!(!fake.actions().iter().any(|a| a.starts_with("units.stop")));
    assert_failed(&fake.run(Rollback, &p), Reason::Committed);
    assert_eq!(
        detail(&fake.run(Cleanup, &p)),
        "nothing left to clean: the update is committed and finished"
    );
    assert_eq!(fake.record(), finished);
}

/// Asserts that `cleanup` confirms past the old instance of a hand-over, which still drains on
/// the same sockets and answers `/readyz` with 503: it looks again until the new instance
/// answers, then commits and finishes. Catches a confirm that takes one 503 from the draining
/// instance as its verdict and fails `not_confirmed`, which rolled a healthy update back.
#[test]
fn cleanup_looks_past_the_503_of_the_draining_old_instance_and_commits() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.answer_draining(2);
    let before = fake.log().len();
    let done = fake.run(Cleanup, &p);
    assert_ok(&done);
    let looks = fake.log()[before..]
        .iter()
        .filter(|line| *line == "get /readyz")
        .count();
    assert_eq!(looks, 3);
    let finished = fake.record();
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));
    assert_eq!(fake.markers(), [Instance::B]);
}

/// Asserts the bound of that wait: a draining instance that answers 503 to one look fewer than
/// `READY_ATTEMPTS` still lets `cleanup` commit, and one that answers every look fails it
/// `not_confirmed` naming `/readyz`, reversible, with nothing committed and no marker removed.
#[test]
fn cleanup_waits_for_readyz_at_most_ready_attempts_looks() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.answer_draining(READY_ATTEMPTS - 1);
    assert_ok(&fake.run(Cleanup, &p));
    assert!(!fake.record().updating);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.answer_draining(READY_ATTEMPTS);
    let done = fake.run(Cleanup, &p);
    assert_failed(&done, Reason::NotConfirmed);
    assert_eq!(
        detail(&done),
        format!(
            "{} was not seen serving: port 9300 does not answer /readyz",
            version_of(NEW)
        )
    );
    assert!(done.report.reversible);
    assert!(!fake.record().committed());
    assert_eq!(fake.markers(), [Instance::A, Instance::B]);
}

/// Runs `tierstore update-hook <step> --params <file>` as hostkeeper does, through the verb's
/// command line and params file, with the systemd steps over `fake`: the exit status and the
/// report line.
pub(super) fn on_the_command_line(
    fake: &SystemdFake,
    step: &str,
    name: &str,
) -> (ExitCode, serde_json::Value) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let file = dir.path().join("params.toml");
    let text = params_text(NEW, Some(OLD), &format!("config_file = \"{CONFIG}\"\n"));
    std::fs::write(&file, text).unwrap();
    let mut args: Vec<OsString> = ["tierstore", "update-hook", step, "--params"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(file.into_os_string());
    let step = |invocation: &Invocation, params: &Params| fake.run(invocation.step, params);
    let machine = Machine { step: &step };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err, &machine);
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    let out = String::from_utf8(out).unwrap();
    (code, serde_json::from_str(out.trim_end()).unwrap())
}

/// Asserts the same two ends through the verb hostkeeper runs, `update-hook cleanup --params`:
/// past two 503 answers of the draining old instance it exits 0 with a report that is not
/// reversible and the update committed and finished; when the draining instance answers every
/// look it exits 1 with `not_confirmed`, reversible, and nothing committed.
#[test]
fn update_hook_cleanup_on_the_command_line_commits_past_the_draining_503() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.answer_draining(2);
    let (code, line) = on_the_command_line(&fake, "cleanup", "tierstore-hook-cleanup-503");
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(0)));
    assert!(line.get("reason").is_none(), "{line}");
    assert_eq!(line["reversible"], false);
    let finished = fake.record();
    assert!(finished.committed());
    assert!(!finished.updating);
    assert_eq!(finished.live, Some(Instance::B));

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify]);
    fake.answer_draining(READY_ATTEMPTS);
    let (code, line) = on_the_command_line(&fake, "cleanup", "tierstore-hook-cleanup-mute");
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
    assert_eq!(line["reason"], "not_confirmed");
    assert_eq!(line["reversible"], true);
    assert!(!fake.record().committed());
}

/// Asserts the fs hand-over of `cutover` through the verb hostkeeper runs, `update-hook cutover
/// --params`, over the cache lock as the server holds it: with the old store draining 13 s and the
/// new one taking the lock 28 s after it exits (node 02's times), the one run exits 0 with a
/// reversible report and no reason, `tierstore-fs@b` is started only once the store is read-write,
/// and the record says `swapped`; with a store that stays read-only through the wait it exits 75
/// with `draining`, retry after 10 seconds, no fs unit touched and no `swapped`. Catches a cutover
/// that exits 75 on the first read-only answer, which hostkeeper rolls back and never runs again.
#[test]
fn update_hook_cutover_on_the_command_line_waits_for_the_cache_lock_before_the_fs_take() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::fs_installed();
    fake.drive(&p, &[Prepare]);
    fake.set_drain_looks(13);
    fake.hold_cache_lock(28);
    let (code, line) = on_the_command_line(&fake, "cutover", "tierstore-hook-cutover-lock");
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(0)));
    assert!(line.get("reason").is_none(), "{line}");
    assert_eq!(line["reversible"], true);
    assert_eq!(
        line["detail"],
        format!(
            "tierstore@b serves {} (hand-over); tierstore-fs@b holds the mount; the store took \
             the cache lock after 39 s",
            version_of(NEW)
        )
    );
    assert!(!fake.fs_started_over_read_only());
    assert_eq!(
        fake.fs_actions(),
        ["fs_markers.set b", "fs_units.start b", "fs_markers.clear a"]
    );
    assert!(fake.record().has(Marker::Swapped));

    let fake = SystemdFake::fs_installed();
    fake.drive(&p, &[Prepare]);
    fake.set_read_only(true);
    let (code, line) = on_the_command_line(&fake, "cutover", "tierstore-hook-cutover-read-only");
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(75)));
    assert_eq!(line["reason"], "draining");
    assert_eq!(line["retry_after_secs"], 10);
    assert_eq!(line["reversible"], true);
    assert!(fake.fs_actions().is_empty(), "{:?}", fake.fs_actions());
    assert!(!fake.record().has(Marker::Swapped));
}

/// Asserts that a hand-over whose new instance opens its cache for 65 looks (as a build node's
/// read-only open of 493589 objects took 65 s), while the old instance answers `/readyz` and the
/// build gauge with the previous version, cuts over: the new instance serves the target version,
/// the old one drains, `swapped` is recorded, and the wait took 65 looks and the probe 3. Catches
/// the cutover that ended its wait at the old instance's `/readyz` and gave the start only the
/// 60 looks of the probe, which failed this start with `probe_failed`.
#[test]
fn a_hand_over_whose_new_instance_opens_its_cache_slowly_cuts_over() {
    let fake = SystemdFake::installed();
    let p = params(NEW, Some(OLD));
    fake.drive(&p, &[Prepare]);
    assert_eq!(fake.record().mode, Some(Mode::Handover));
    fake.set_takeover_looks(65);
    let done = fake.run_as_hostkeeper(Cutover, &p);
    assert_ok(&done);
    assert_eq!(
        detail(&done),
        format!("tierstore@b serves {} (hand-over)", version_of(NEW))
    );
    // 65 pauses of the wait, then two between the probe's three hits.
    assert_eq!(Clock::now_ms(&fake), 67 * 1000);
    assert_eq!(fake.peek(Instance::A), UnitState::Deactivating);
    assert_eq!(fake.peek(Instance::B), UnitState::Active);
    assert!(fake.record().has(Marker::Swapped));
    fake.drive(&p, &[Verify, Cleanup]);
    assert_eq!(fake.record().live, Some(Instance::B));
}

/// Asserts the bound of that wait: a new instance that takes the sockets after one look fewer
/// than `READY_ATTEMPTS` (its first answer is the last look) still cuts over, and one that has not
/// taken them after `READY_ATTEMPTS` looks fails the `cutover` with `probe_failed`, reversible,
/// with nothing recorded, the old instance still serving and the new one still running; the
/// `rollback` hostkeeper runs then waits (`draining`) while both run.
#[test]
fn cutover_waits_for_a_hand_over_at_most_ready_attempts_looks() {
    let p = params(NEW, Some(OLD));
    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare]);
    fake.set_takeover_looks(READY_ATTEMPTS - 1);
    assert_ok(&fake.run_as_hostkeeper(Cutover, &p));
    assert!(fake.record().has(Marker::Swapped));

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare]);
    fake.set_takeover_looks(READY_ATTEMPTS);
    let done = fake.run_as_hostkeeper(Cutover, &p);
    assert_failed(&done, Reason::ProbeFailed);
    assert_eq!(
        detail(&done),
        format!(
            "no request to port 9300 was answered by {}",
            version_of(NEW)
        )
    );
    assert!(done.report.reversible);
    assert!(!fake.record().has(Marker::Swapped));
    assert_eq!(Clock::now_ms(&fake), u64::from(READY_ATTEMPTS) * 1000);
    assert_eq!(fake.peek(Instance::A), UnitState::Active);
    assert_eq!(fake.peek(Instance::B), UnitState::Active);
    assert_deferred(&fake.run_as_hostkeeper(Rollback, &p), Reason::Draining, 10);
}
