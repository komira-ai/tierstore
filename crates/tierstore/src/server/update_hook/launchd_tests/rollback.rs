//! `rollback` on launchd: after a wrong build, after a failed cutover, before any cutover, past
//! the commit, and of a first install, which has nothing to go back to.

use std::collections::BTreeSet;

use super::fake::{
    Fake, LABEL, NEW, NEW_VERSION, OLD, assert_failed, assert_ok, in_flight, params_for, path,
};
use crate::server::update_hook::{LAUNCHD_PLIST, Marker, Reason, Record, Step};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

fn bootout() -> String {
    format!("launchd.bootout {LABEL}")
}

fn bootstrap() -> String {
    format!("launchd.bootstrap {LAUNCHD_PLIST}")
}

/// Asserts that a build gauge that is not the target's fails `verify` with `wrong_build` (exit
/// 1), and that the `rollback` that follows, once hostkeeper has put `current` back, boots the
/// job out and in again so that the previous release serves, and leaves the record at rest with
/// exit 0. Catches a rollback that leaves the new release loaded, or one that does not wait for
/// the previous release.
#[test]
fn a_wrong_build_fails_verify_and_rollback_restores_the_previous_release() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.lie(NEW, "9.9.9");
    let verify = fake.host_run(Verify);
    assert_failed(&verify, Reason::WrongBuild);
    assert!(!fake.record().has(Marker::Verified));
    let before = fake.launchd_calls().len();
    fake.switch_current(OLD);
    let done = fake.run(Rollback);
    assert_ok(&done);
    assert_eq!(fake.launchd_calls()[before..], [bootout(), bootstrap()]);
    let end = fake.snapshot();
    assert_eq!(end.job, path(OLD));
    assert_eq!(end.record, Some(Record::idle(None)));
    assert!(fake.strays().is_empty());
}

/// Asserts that a `cutover` that failed its probe is undone by `rollback`: the previous release
/// serves again and nothing is in flight. Catches a rollback that cannot start from the state a
/// failed cutover leaves.
#[test]
fn rollback_after_a_failed_cutover_restores_the_previous_release() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    fake.lie(NEW, "9.9.9");
    assert_failed(&fake.host_run(Cutover), Reason::ProbeFailed);
    let done = fake.host_run(Rollback);
    assert_ok(&done);
    assert_eq!(fake.snapshot().job, path(OLD));
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that a `rollback` after `prepare` alone, before any cutover, boots the job out and in
/// again on the previous release and clears the update. Catches a rollback that refuses an
/// update that has no marker yet.
#[test]
fn rollback_after_prepare_alone_restarts_the_previous_release() {
    let fake = Fake::new();
    fake.advance(&[Prepare]);
    assert_ok(&fake.host_run(Rollback));
    assert_eq!(fake.launchd_calls(), [bootout(), bootstrap()]);
    assert_eq!(fake.snapshot().job, path(OLD));
    assert_eq!(fake.record(), Record::idle(None));
}

/// Asserts that a rollback whose previous release never answers fails with `not_ready` and keeps
/// the update on record, so that hostkeeper can run it again. Catches a rollback that clears the
/// record before the release it went back to serves.
#[test]
fn rollback_keeps_the_update_on_record_until_the_previous_release_serves() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    fake.mute(OLD);
    let done = fake.host_run(Rollback);
    assert_failed(&done, Reason::NotReady);
    assert!(fake.record().updating);
}

/// Asserts that a `rollback` run again after it finished says nothing is in flight and calls
/// launchd not at all. Catches a re-run that restarts the job once more.
#[test]
fn rollback_with_nothing_in_flight_does_nothing() {
    let fake = Fake::new();
    let done = fake.host_run(Rollback);
    assert_ok(&done);
    assert!(fake.launchd_calls().is_empty());
    fake.advance(&[Prepare, Cutover, Rollback]);
    let before = fake.launchd_calls().len();
    assert_ok(&fake.host_run(Rollback));
    assert_eq!(fake.launchd_calls().len(), before);
}

/// Asserts that an update record whose params name no previous release is refused by `rollback`
/// with `bad_params` before it touches the job. Catches a rollback that restarts a job without
/// knowing which version it must serve.
#[test]
fn rollback_without_a_previous_version_in_the_params_is_refused() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover]);
    let before = fake.launchd_calls().len();
    let done = fake.run_with(Rollback, &params_for(NEW, NEW_VERSION, None), LABEL);
    assert_failed(&done, Reason::BadParams);
    assert_eq!(fake.launchd_calls().len(), before);
}

/// Asserts that `rollback` after the commit fails with `committed`, reports `reversible` false
/// and calls launchd not at all. Catches a rollback that undoes a committed update.
#[test]
fn rollback_after_the_commit_fails_committed() {
    let fake = Fake::new();
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    let before = fake.launchd_calls().len();
    let done = fake.host_run(Rollback);
    assert_failed(&done, Reason::Committed);
    assert!(!done.report.reversible);
    assert_eq!(fake.launchd_calls().len(), before);
}

/// Asserts that a failed first install is rolled back by booting the job out and failing with
/// `no_previous_release` (exit 1): no job is left loaded, the update stays on record without
/// markers, and a second run says the same, boots out again and loads nothing. Catches a first
/// install rollback that claims success, or one that tries to start a previous release.
#[test]
fn a_failed_first_install_rolls_back_to_nothing_and_says_so_every_time() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare, Cutover, Verify]);
    assert_eq!(fake.record().markers.len(), 2);
    let first = fake.host_run(Rollback);
    assert_failed(&first, Reason::NoPreviousRelease);
    assert_eq!(fake.launchd_calls().last(), Some(&bootout()));
    assert_eq!(fake.snapshot().job, None);
    assert_eq!(fake.record(), in_flight(true));
    let calls = fake.launchd_calls().len();
    let snapshot = fake.snapshot();
    let again = fake.host_run(Rollback);
    assert_failed(&again, Reason::NoPreviousRelease);
    assert_eq!(fake.launchd_calls()[calls..], [bootout()]);
    assert_eq!(fake.snapshot(), snapshot);
    assert_eq!(again.report.detail, first.report.detail);
}

/// Asserts that after a failed first install is rolled back, a new `prepare` and `cutover` start
/// afresh and load the job. Catches a record left in a state no later prepare can replace.
#[test]
fn a_first_install_can_start_again_after_its_rollback() {
    let fake = Fake::first_install();
    fake.advance(&[Prepare, Cutover]);
    assert_failed(&fake.host_run(Rollback), Reason::NoPreviousRelease);
    fake.advance(&[Prepare, Cutover, Verify, Cleanup]);
    assert_eq!(fake.snapshot().job, path(NEW));
    assert_eq!(
        fake.record().markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping])
    );
}
