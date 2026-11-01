//! `prepare`.

use std::path::PathBuf;

use super::steps::READY_ATTEMPTS;
use super::testing::{Fake, NEW, NEWER, OLD};
use super::{Class, Conntrack, Mode, Outcome, Reason, Record, Step, guard_ruleset};

const UNIT_2: &str = "units.start tierstore@9302.service";

fn path(p: &str) -> PathBuf {
    PathBuf::from(p)
}

/// Asserts what `prepare` leaves: the record of the update with `serving` on the idle port and
/// `previous` on the running one, the guard loaded (and no redirect applied), the idle slot
/// linked to the staged release and started, and both instances running with the old one still
/// holding the lock and the rule. Asserts the order of the changes: the record before anything
/// that can be seen. It would catch a `prepare` that moves the rule, or one that starts the
/// instance before it has recorded that it did.
#[test]
fn prepare_starts_the_idle_slot_beside_the_running_one() {
    let fake = Fake::new();
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(done.report.reversible && !done.report.promotion_pending);
    let snap = fake.snapshot();
    let record = snap.record.unwrap();
    assert_eq!(
        (record.serving, record.previous, record.mode),
        (9302, Some(9301), Mode::Handover)
    );
    assert!(record.markers.is_empty());
    assert_eq!(snap.redirect, Some(9301));
    assert!(snap.guard);
    assert_eq!(snap.holder, Some(9301));
    assert_eq!(snap.running.get(&9301), Some(&path(OLD)));
    assert_eq!(snap.running.get(&9302), Some(&path(NEW)));
    assert_eq!(snap.links.get(&9302), Some(&path(NEW)));
    assert_eq!(
        fake.changes(),
        [
            "nft.apply kbf_guard_tierstore",
            "state.save",
            "slots.point 9302",
            "units.start tierstore@9302.service"
        ]
    );
    assert_eq!(fake.applied(), [guard_ruleset(9300, [9301, 9302])]);
    assert_eq!(fake.live_locks(), 0);
}

/// Asserts that a second `prepare` ends in the state of the first: one started idle unit, one
/// link, one guard table, the same record. It would catch a re-run that starts a second
/// instance or loses the record.
#[test]
fn prepare_twice_leaves_the_state_of_prepare_once() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    let once = fake.snapshot();
    fake.advance(&[Step::Prepare]);
    assert_eq!(fake.snapshot(), once);
    assert_eq!(once.running.len(), 2);
}

/// Asserts that either release lacking `handover-v1` gives `restart` mode and that `prepare` then
/// starts nothing: the running release has no lock, so a second instance would write beside it.
/// It would catch a hand-over between releases that cannot do one.
#[test]
fn a_release_without_the_capability_makes_it_a_restart() {
    for fake in [Fake::legacy(), Fake::new()] {
        fake.set_caps(NEW, &[]);
        fake.advance(&[Step::Prepare]);
        let snap = fake.snapshot();
        assert_eq!(snap.record.unwrap().mode, Mode::Restart);
        assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9301]);
        assert_eq!(snap.links.get(&9302), Some(&path(NEW)));
        assert!(!fake.log().iter().any(|c| c == UNIT_2));
    }
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare]);
    assert_eq!(fake.record().mode, Mode::Restart);
}

/// Asserts that the host checks give exit 75 with the class `environment`, the retry delay, and
/// nothing changed: a conntrack established timeout that is not longer than the idle bound plus
/// the margin (120 s), and a table filled to the share the params allow (80 percent). Asserts the
/// boundaries. It would catch a `prepare` that goes on with a host that would reset an idle
/// keep-alive connection after the swap.
#[test]
fn the_conntrack_bounds_defer_the_update() {
    let defers = |ct: Conntrack, why: &str| {
        let fake = Fake::new();
        fake.set_conntrack(ct);
        let before = fake.snapshot();
        let done = fake.run(Step::Prepare);
        assert_eq!(done.outcome, Outcome::Deferred, "{why}: {done:?}");
        assert_eq!(done.outcome.code(), 75);
        assert_eq!(done.report.class, Class::Environment);
        assert_eq!(done.report.reason, Some(Reason::HostNotReady));
        assert_eq!(done.report.retry_after_secs, Some(300));
        // The guard is loaded first and the same text replaces the one that was there.
        assert_eq!(fake.changes(), ["nft.apply kbf_guard_tierstore"], "{why}");
        assert_eq!(fake.snapshot(), before);
    };
    let passes = |ct: Conntrack, why: &str| {
        let fake = Fake::new();
        fake.set_conntrack(ct);
        assert_eq!(fake.run(Step::Prepare).outcome, Outcome::Success, "{why}");
    };
    let ct = |timeout, count| Conntrack {
        tcp_timeout_established_secs: timeout,
        count,
        max: 1000,
    };
    defers(ct(120, 0), "timeout equal to the bound");
    defers(ct(60, 0), "timeout below the bound");
    passes(ct(121, 0), "timeout one above the bound");
    defers(ct(432_000, 800), "table at 80 percent");
    defers(ct(432_000, 1000), "table full");
    passes(ct(432_000, 799), "table just under 80 percent");
}

/// Asserts that a guard table that cannot be loaded is a deferral (75, `environment`) with
/// nothing recorded, linked or started. It would catch a `prepare` that starts an instance whose
/// private port nothing guards.
#[test]
fn a_guard_that_cannot_be_loaded_defers_the_update() {
    let fake = Fake::new();
    fake.reject_guard();
    let before = fake.snapshot();
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.report.class, Class::Environment);
    assert!(
        done.report
            .detail
            .unwrap()
            .contains("guard table cannot be loaded")
    );
    // The rejected load is a call, but it is one transaction that left nothing, and nothing
    // else was done: no record, link, unit or removal.
    assert_eq!(fake.changes(), ["nft.apply kbf_guard_tierstore"]);
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that a new instance that never answers `/readyz` is exit 1 with the class `release`
/// after the counted number of looks. It would catch a `prepare` that goes on with an instance
/// that is not up, or one that waits without a bound.
#[test]
fn an_instance_that_never_becomes_ready_fails_the_release() {
    let fake = Fake::new();
    fake.mute(9302);
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(fake.pauses(), READY_ATTEMPTS);
    assert!(done.report.reversible);
}

/// Asserts that a new instance that says `read-write` while the old one runs is refused: two
/// writers on one directory. It would catch a `prepare` that accepts any ready answer.
#[test]
fn a_new_instance_that_holds_the_lock_beside_a_running_one_is_refused() {
    let fake = Fake::new();
    fake.drop_lock();
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::UnexpectedMode));
    assert_eq!(done.report.class, Class::Release);
}

/// Asserts that `prepare` after the cutover changes nothing and says `out_of_order`. It would
/// catch a `prepare` that re-points the idle link under a rule that already names it.
#[test]
fn prepare_after_the_cutover_is_refused() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let before = fake.snapshot();
    let mark = fake.changes().len();
    let done = fake.run(Step::Prepare);
    assert_eq!(done.report.reason, Some(Reason::OutOfOrder));
    assert_eq!(fake.changes().len(), mark);
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that a second update, after the first has finished, uses the slot the first retired:
/// the ports alternate, the retired slot's link is made again, and the record is written afresh
/// with no markers. It would catch a `prepare` that finds the retired slot's state in the
/// finished record.
#[test]
fn the_second_update_uses_the_other_slot() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    let params = fake.params_for(NEWER, "0.5.0");
    let done = super::run_step(Step::Prepare, Some(&params), &fake.ports());
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let snap = fake.snapshot();
    let record = snap.record.unwrap();
    assert_eq!((record.serving, record.previous), (9301, Some(9302)));
    assert!(record.markers.is_empty());
    assert_eq!(snap.links.get(&9301), Some(&path(NEWER)));
    assert_eq!(snap.links.get(&9302), Some(&path(NEW)));
    assert_eq!(snap.running.get(&9301), Some(&path(NEWER)));
}

/// Asserts that a current slot with no link is `no_slot` with the class `environment` and
/// nothing changed, and that a record that was written before is not left half. It would catch a
/// `prepare` that guesses the running release.
#[test]
fn a_current_slot_without_a_link_is_refused() {
    let fake = Fake::new();
    fake.drop_link(9301);
    let done = fake.run(Step::Prepare);
    assert_eq!(done.report.reason, Some(Reason::NoSlot));
    assert_eq!(done.report.class, Class::Environment);
    assert!(fake.changes().is_empty());
    assert_eq!(fake.record(), Record::idle(9301));
}

fn full_table() -> Conntrack {
    Conntrack {
        tcp_timeout_established_secs: 432_000,
        count: 900,
        max: 1000,
    }
}

/// Asserts that a deferral after the guard was loaded leaves no guard when there was none before:
/// the table is loaded, the host is found not ready, the table is removed, the exit is 75, and
/// the host is as it was. It would catch a deferred `prepare` that leaves a table behind.
#[test]
fn a_deferral_removes_the_guard_it_placed() {
    let fake = Fake::new();
    fake.remove_guard();
    fake.set_conntrack(full_table());
    let before = fake.snapshot();
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(
        fake.changes(),
        [
            "nft.apply kbf_guard_tierstore",
            "nft.delete kbf_guard_tierstore"
        ]
    );
    assert!(!fake.snapshot().guard);
    assert_eq!(fake.snapshot(), before);
}

/// Asserts that a deferral leaves a guard that was there before the update, and still exits 75.
/// It would catch a deferred `prepare` that removes state it did not place, which unguards the
/// private ports of a serving host.
#[test]
fn a_deferral_leaves_a_guard_that_was_there() {
    let fake = Fake::new();
    fake.set_conntrack(full_table());
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert!(fake.snapshot().guard);
    assert!(!fake.log().iter().any(|c| c.starts_with("nft.delete")));
}

/// Asserts that a removal that fails turns the deferral into exit 1 with the class
/// `environment`: the host is not as it was, and the updater's `rollback` must run. It would
/// catch a deferral that says nothing was done when a table is left.
#[test]
fn a_removal_that_fails_is_exit_1() {
    let reference = Fake::new();
    reference.remove_guard();
    reference.set_conntrack(full_table());
    reference.advance_to_deferral();
    let n = reference.calls();
    let fake = Fake::new();
    fake.remove_guard();
    fake.set_conntrack(full_table());
    fake.fail_at(Some(n));
    let done = fake.run(Step::Prepare);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NftFailed));
    assert_eq!(done.report.class, Class::Environment);
}

/// Asserts that after a `prepare` that placed the guard and a re-run, the record still says the
/// update placed it, so that `rollback` removes it. It would catch a re-run that sees its own
/// guard as one that was there before.
#[test]
fn a_re_run_remembers_that_the_update_placed_the_guard() {
    let fake = Fake::new();
    fake.remove_guard();
    fake.advance(&[Step::Prepare, Step::Prepare]);
    assert!(fake.record().guard_placed);
}

/// Asserts that nothing is changed before the guard is in place: the calls that precede the guard's
/// are all reads. It would catch a `prepare` that links or starts before the private port is
/// guarded.
#[test]
fn nothing_is_applied_before_the_guard() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare]);
    assert_eq!(fake.changes()[0], "nft.apply kbf_guard_tierstore");
}
