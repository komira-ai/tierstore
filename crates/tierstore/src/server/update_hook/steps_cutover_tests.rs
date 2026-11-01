//! `cutover`.

use super::steps::PROBE_ATTEMPTS;
use super::testing::{Fake, NEW, OLD};
use super::{Class, Marker, Outcome, Reason, Step, redirect_ruleset};

/// Asserts the happy path: the rule names the idle port, applied in one `nft` transaction (one
/// call, the text of the swap) before the record says `swapped`, both instances still run, and
/// the record's `serving` stays on the new port. It would catch a swap in two calls, which leaves
/// a moment with no rule, or a `swapped` written before the swap.
#[test]
fn cutover_swaps_the_rule_in_one_transaction_then_records_it() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    let mark = fake.changes().len();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.reversible);
    assert_eq!(
        fake.changes()[mark..],
        ["nft.apply kbf_forward_tierstore", "state.save"]
    );
    assert_eq!(
        fake.applied().last().unwrap(),
        &redirect_ruleset(9300, 9302)
    );
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9302));
    let record = snap.record.unwrap();
    assert_eq!((record.serving, record.previous), (9302, Some(9301)));
    assert!(record.markers.contains(&Marker::Swapped));
    assert_eq!(snap.running.len(), 2);
}

/// Asserts that a second `cutover` changes no state and only applies the rule again, without a
/// probe. It would catch a re-run that probes again and swaps back when the probe is slow.
#[test]
fn cutover_twice_leaves_one_rule_naming_the_idle_port() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let once = fake.snapshot();
    let mark = fake.log().len();
    fake.advance(&[Step::Cutover]);
    assert_eq!(fake.snapshot(), once);
    assert!(!fake.log()[mark..].iter().any(|c| c.starts_with("get 9300")));
}

/// Asserts that a probe through the public port that is never answered by the new instance puts
/// the old rule back, exits 1 with the class `release`, records nothing, and gives up after the
/// counted number of looks. It would catch a cutover that leaves traffic on an instance the
/// probe could not see.
#[test]
fn a_probe_that_never_succeeds_swaps_back() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    fake.lie(9302, "9.9.9");
    let before = fake.pauses();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(done.report.class, Class::Release);
    assert!(done.report.reversible);
    assert_eq!(fake.pauses() - before, PROBE_ATTEMPTS);
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9301));
    assert!(snap.record.unwrap().markers.is_empty());
    assert_eq!(
        fake.applied().last().unwrap(),
        &redirect_ruleset(9300, 9301)
    );
}

/// Asserts that a build that names no slot is never taken for the new instance. It would catch a
/// probe that matches on the version alone, which cannot tell the two instances apart when a
/// release is installed over itself.
#[test]
fn a_gauge_without_a_slot_never_matches() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    fake.no_slot_label();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(fake.snapshot().redirect, Some(9301));
}

/// Asserts that an idle instance that does not answer `/readyz` fails the step with nothing
/// changed. It would catch a swap to an instance that is not up (after a reboot, for one).
#[test]
fn cutover_to_an_instance_that_is_not_ready_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    let before = fake.snapshot();
    fake.mute(9302);
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that a failing `nft` leaves the old rule: exit 1 with the class `environment`, no
/// `swapped`, and that the step then succeeds when run again. It would catch a half-applied swap
/// and a record that claims one.
#[test]
fn a_failing_nft_leaves_the_old_rule() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    fake.reject_redirect_once();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::NftFailed));
    assert_eq!(done.report.class, Class::Environment);
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9301));
    assert!(snap.record.unwrap().markers.is_empty());
    fake.advance(&[Step::Cutover]);
    assert_eq!(fake.snapshot().redirect, Some(9302));
}

/// Asserts that `cutover` without a `prepare`, and after the commit, is `out_of_order` and
/// changes nothing, and that after the commit the report says it is not reversible. It would
/// catch a cutover that swaps to a slot nothing prepared.
#[test]
fn cutover_out_of_order_changes_nothing() {
    let fake = Fake::new();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert!(done.report.reversible);
    assert!(fake.changes().is_empty());
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    let mark = fake.changes().len();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert!(!done.report.reversible);
    assert_eq!(fake.changes().len(), mark);
}

/// Asserts that a new instance that already holds the lock (the old one died during the overlap)
/// is accepted: the swap goes on. It would catch a cutover that insists on `read-only` and so can
/// never finish after a crash of the old instance.
#[test]
fn a_new_instance_that_took_the_lock_is_accepted() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    fake.crash(9301);
    fake.advance(&[Step::Cutover]);
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9302));
    assert_eq!(snap.holder, Some(9302));
    let again = fake.run(Step::Verify);
    assert_eq!(again.report.reason, Some(Reason::PreviousDown));
}

/// Asserts the restart: v-current is stopped and v-next started and waited for before the swap,
/// in that order, and the lock moves with them. It would catch a restart that swaps before the
/// new instance is up, which is a refused connection for every client.
#[test]
fn in_restart_mode_the_old_instance_stops_before_the_new_one_starts() {
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare]);
    let mark = fake.changes().len();
    fake.advance(&[Step::Cutover]);
    assert_eq!(
        fake.changes()[mark..],
        [
            "units.stop tierstore@9301.service",
            "units.start tierstore@9302.service",
            "nft.apply kbf_forward_tierstore",
            "state.save"
        ]
    );
    let snap = fake.snapshot();
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9302]);
    assert_eq!(snap.holder, Some(9302));
    assert_eq!(snap.running.get(&9302).unwrap().to_str(), Some(NEW));
}

/// Asserts that a restart whose probe fails stops the new instance first, then starts the old one
/// and points the rule back at it, in that order, with no moment of two instances of the release
/// that has no lock. It would catch a failed restart that leaves no instance serving, or starts
/// the old release beside the new: two writers.
#[test]
fn a_failed_restart_brings_the_old_instance_back() {
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare]);
    fake.lie(9302, "9.9.9");
    let mark = fake.changes().len();
    let done = fake.run(Step::Cutover);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(
        fake.changes()[mark..],
        [
            "units.stop tierstore@9301.service",
            "units.start tierstore@9302.service",
            "nft.apply kbf_forward_tierstore",
            "units.stop tierstore@9302.service",
            "units.start tierstore@9301.service",
            "nft.apply kbf_forward_tierstore"
        ]
    );
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9301));
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9301]);
    assert_eq!(snap.running.get(&9301).unwrap().to_str(), Some(OLD));
    assert!(fake.errors().is_empty(), "{:?}", fake.errors());
}
