//! `verify`.

use super::steps::PROBE_HITS;
use super::testing::{Fake, Promotion};
use super::{Class, Marker, Outcome, Reason, Step, redirect_ruleset};

/// Asserts the gate before the commit: it succeeds, says the promotion is pending (the old
/// instance holds the lock), records `verified` and changes nothing else. It would catch a gate
/// that reports no pending promotion while the lock is still the old instance's.
#[test]
fn verify_before_the_commit_passes_with_the_promotion_pending() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let mark = fake.changes().len();
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.promotion_pending);
    assert!(done.report.reversible);
    assert_eq!(fake.changes()[mark..], ["state.save"]);
    let markers = fake.record().markers;
    assert!(markers.contains(&Marker::Swapped) && markers.contains(&Marker::Verified));
}

/// Asserts that a second `verify` changes nothing and says the same. It would catch a gate that
/// is not safe to run again and again, which the updater does.
#[test]
fn verify_twice_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
    let once = fake.snapshot();
    let mark = fake.changes().len();
    let done = fake.run(Step::Verify);
    assert!(done.report.promotion_pending);
    assert_eq!(fake.snapshot(), once);
    assert_eq!(fake.changes().len(), mark);
}

/// Asserts that a public port answered by an instance with the wrong version, or by the old
/// instance, is `wrong_build` with the class `release`, and that `verified` stays unset. It
/// would catch a gate that lets `cleanup` retire the only instance that serves the right build.
#[test]
fn verify_fails_when_the_public_port_is_not_answered_by_the_new_build() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.lie(9302, "9.9.9");
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::WrongBuild));
    assert_eq!(done.report.class, Class::Release);
    assert!(!fake.record().markers.contains(&Marker::Verified));
}

/// Asserts that a redirect that names the old slot fails the gate with `redirect_wrong`. It would
/// catch a gate that trusts the record over the rules.
#[test]
fn verify_fails_when_the_rule_names_the_old_slot() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.ports()
        .nft
        .apply(&redirect_ruleset(9300, 9301))
        .unwrap();
    let done = fake.run(Step::Verify);
    assert_eq!(done.report.reason, Some(Reason::RedirectWrong));
}

/// Asserts that a new instance that stops answering `/readyz`, and an old instance that is gone,
/// fail the gate before the commit. It would catch a gate that passes with nothing serving the
/// old build to fall back to.
#[test]
fn verify_fails_when_an_instance_is_gone_before_the_commit() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.mute(9302);
    assert_eq!(fake.run(Step::Verify).report.reason, Some(Reason::NotReady));
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.crash(9301);
    assert_eq!(
        fake.run(Step::Verify).report.reason,
        Some(Reason::PreviousDown)
    );
}

/// Asserts that `verify` with no cutover behind it is `out_of_order`, with an update not begun,
/// begun and not swapped. It would catch a gate that passes before anything was swapped.
#[test]
fn verify_before_a_cutover_is_out_of_order() {
    let fake = Fake::new();
    assert_eq!(
        fake.run(Step::Verify).report.reason,
        Some(Reason::OutOfOrder)
    );
    fake.advance(&[Step::Prepare]);
    assert_eq!(
        fake.run(Step::Verify).report.reason,
        Some(Reason::OutOfOrder)
    );
}

/// Asserts that after the commit a late promotion clears the flag: `verify` waits for
/// `read-write` for the counted looks (one pause between two), exits 0 with no pending flag,
/// changes nothing, and says it is not reversible. It would catch a post-commit gate that does
/// not wait for the lock.
#[test]
fn verify_after_the_commit_waits_for_the_promotion() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
    fake.set_promotion(Promotion::AfterPolls(5));
    fake.advance(&[Step::Cleanup]);
    let (pauses, mark) = (fake.pauses(), fake.changes().len());
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.promotion_pending);
    assert!(!done.report.reversible);
    // The probe through the public port pauses between its answers; the wait pauses between looks.
    assert_eq!(fake.pauses() - pauses, PROBE_HITS - 1 + 4);
    assert_eq!(fake.changes().len(), mark);
    assert_eq!(fake.snapshot().holder, Some(9302));
}

/// Asserts that with the lock never taken `verify` exits 0 with the promotion pending after
/// exactly the bound's worth of looks, and does so with the updater's ceiling in the params file.
/// It would catch a hook that exits 1 at the ceiling or on the bound: the updater applies the
/// ceiling itself and ends the watch as a failure after the commit.
#[test]
fn verify_after_the_commit_reports_a_pending_promotion_and_never_fails_for_it() {
    let fake = Fake::new();
    assert_eq!(fake.params().promotion_ceiling_secs, Some(3600));
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
    fake.set_promotion(Promotion::Stuck);
    fake.advance(&[Step::Cleanup]);
    let pauses = fake.pauses();
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.promotion_pending && !done.report.reversible);
    assert_eq!(fake.pauses() - pauses, PROBE_HITS - 1 + 30);
    let readyz = fake
        .log()
        .iter()
        .filter(|c| *c == "get 9302 /readyz")
        .count();
    assert!(readyz >= 31);
    let again = fake.run(Step::Verify);
    assert!(again.report.promotion_pending);
}

/// Asserts that a pending promotion's detail says it is pending and names no holder of the cache
/// lock, before the commit and after it (when the previous instance is stopped and the lock may be
/// held by nobody yet). It would catch a detail that promises the holder, which no instance
/// reports.
#[test]
fn a_pending_promotion_names_no_holder_of_the_lock() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let before = fake.run(Step::Verify);
    fake.set_promotion(Promotion::Stuck);
    fake.advance(&[Step::Cleanup]);
    let after = fake.run(Step::Verify);
    for done in [before, after] {
        assert!(done.report.promotion_pending, "{done:?}");
        let detail = done.report.detail.unwrap();
        assert_eq!(detail, "9302 serves read-only: the promotion is pending");
        assert!(!detail.contains("hold"), "{detail}");
    }
}

/// Asserts that after the commit an instance that answers wrongly through the public port is
/// `wrong_build` (exit 1, the updater reads it as a failure after the commit), and one that stops
/// answering `/readyz` is `not_ready`; both say they are not reversible. It would catch a
/// post-commit gate that reports success for a new instance that is down.
#[test]
fn verify_after_the_commit_fails_for_a_fault_of_its_own_observation() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    fake.lie(9302, "9.9.9");
    let done = fake.run(Step::Verify);
    assert_eq!(done.report.reason, Some(Reason::WrongBuild));
    assert!(!done.report.reversible);
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    fake.mute(9302);
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert!(!done.report.reversible);
}

/// Asserts that in restart mode the gate has no old instance to look at and waits for the
/// promotion as it does after the commit; the new instance took the lock at its start, so the
/// flag is clear. It would catch a gate that looks for an old instance that a restart stopped.
#[test]
fn verify_in_restart_mode_does_not_look_for_the_old_instance() {
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let done = fake.run(Step::Verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.promotion_pending);
    assert!(done.report.reversible);
    assert!(fake.record().markers.contains(&Marker::Verified));
}

/// Asserts that `verified` is written with the boot of the host, that a `verify` in the same boot
/// writes nothing more, and that one in a later boot (after a `restore` the new instance was seen
/// again) records the new boot. It would catch a `verified` that outlives a reboot.
#[test]
fn verify_records_the_boot_it_observed() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
    assert_eq!(fake.record().boot_id.as_deref(), Some("boot-1"));
    let mark = fake.changes().len();
    fake.advance(&[Step::Verify]);
    assert_eq!(fake.changes().len(), mark);
    fake.new_boot();
    fake.advance(&[Step::Verify]);
    assert_eq!(fake.record().boot_id.as_deref(), Some("boot-2"));
    assert_eq!(fake.changes().len(), mark + 1);
}

/// Asserts that after the commit a public port that answers 503, and one that does not answer at
/// all, are each taken to mean that the new instance does not serve: `wrong_build`, exit 1, not
/// reversible. It would catch a probe that takes any answer of the public port for the new
/// instance's.
#[test]
fn verify_fails_when_the_public_port_does_not_answer_or_answers_503() {
    for spoil in [Fake::public_busy as fn(&Fake), |f| f.crash(9302)] {
        let fake = Fake::new();
        fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
        spoil(&fake);
        let done = fake.run(Step::Verify);
        assert_eq!(done.outcome, Outcome::Failure);
        assert_eq!(done.report.reason, Some(Reason::WrongBuild));
        assert!(!done.report.reversible);
    }
}
