//! `rollback`.

use std::collections::BTreeSet;

use super::testing::{Fake, NEWER, OLD};
use super::{Marker, Outcome, Reason, Record, Step};

/// Asserts that a rollback after `prepare`, after `cutover` and after `verify` each returns the
/// world to the state before the update: the old rule and instance, the new instance stopped and
/// its slot link removed, the record with no update in flight. It would catch a rollback that
/// leaves the new slot's link behind, or the new instance running.
#[test]
fn rollback_returns_the_host_to_where_it_was() {
    let start = Fake::new().snapshot();
    for steps in [
        vec![Step::Prepare],
        vec![Step::Prepare, Step::Cutover],
        vec![Step::Prepare, Step::Cutover, Step::Verify],
    ] {
        let fake = Fake::new();
        fake.advance(&steps);
        let mark = fake.changes().len();
        let done = fake.run(Step::Rollback);
        assert_eq!(done.outcome, Outcome::Success, "{steps:?}: {done:?}");
        assert!(done.report.reversible);
        assert_eq!(fake.snapshot(), start, "{steps:?}");
        assert_eq!(
            fake.changes()[mark..],
            [
                "units.start tierstore@9301.service",
                "nft.apply kbf_forward_tierstore",
                "units.stop tierstore@9302.service",
                "slots.remove 9302",
                "state.save"
            ],
            "{steps:?}"
        );
    }
}

/// Asserts that the link of the instance the restored rule does not name is removed after that
/// instance is stopped, and that the removal of an absent link is not an error (a second
/// rollback, and a rollback after `prepare` crashed before it made the link). It would catch a
/// rollback that leaves a retired slot able to start.
#[test]
fn rollback_removes_the_stopped_slots_link_and_an_absent_link_is_fine() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.drop_link(9302);
    let done = fake.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let log = fake.log();
    let stop = log
        .iter()
        .position(|c| c == "units.stop tierstore@9302.service")
        .unwrap();
    let remove = log.iter().position(|c| c == "slots.remove 9302").unwrap();
    assert!(stop < remove);
    assert!(!fake.snapshot().links.contains_key(&9302));
}

/// Asserts that a second rollback has nothing to do and touches nothing. It would catch a
/// rollback that stops the serving instance when run on a host with no update in flight.
#[test]
fn rollback_twice_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Rollback]);
    let once = fake.snapshot();
    let mark = fake.changes().len();
    let done = fake.run(Step::Rollback);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(fake.snapshot(), once);
    assert_eq!(fake.changes().len(), mark);
}

/// Asserts that after `stopping` a rollback changes nothing and exits 1 with `"reversible":false`
/// (the updater reads it as a failure after the commit, not a halt): with `stopping` just written,
/// with `cleanup` half done, and with it finished. It would catch a rollback that swaps back to a
/// port the old instance has stopped listening on.
#[test]
fn no_rollback_after_stopping() {
    let all = BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]);
    let mid = Record {
        markers: all.clone(),
        previous: Some(9301),
        ..Record::idle(9302)
    };
    let done_record = Record {
        markers: all,
        ..Record::idle(9302)
    };
    for record in [mid, done_record] {
        let fake = Fake::new();
        fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
        fake.set_record(&record);
        let before = fake.snapshot();
        let mark = fake.changes().len();
        let done = fake.run(Step::Rollback);
        assert_eq!(done.outcome, Outcome::Failure);
        assert_eq!(done.report.reason, Some(Reason::Committed));
        assert!(!done.report.reversible);
        assert_eq!(fake.changes().len(), mark);
        assert_eq!(fake.snapshot(), before);
    }
}

/// Asserts that a rollback after the old instance died during the overlap starts it again before
/// it moves the rule. It would catch a rollback that points the rule at a port nothing listens on.
#[test]
fn rollback_starts_an_old_instance_that_died() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    fake.crash(9301);
    fake.advance(&[Step::Rollback]);
    let snap = fake.snapshot();
    assert_eq!(snap.running.get(&9301).unwrap().to_str(), Some(OLD));
    assert_eq!(snap.running.len(), 1);
    assert_eq!(snap.redirect, Some(9301));
}

/// Asserts that a restart-mode rollback, which has no old instance running, starts it, and that
/// the record keeps the mode. It would catch a rollback of a restart that leaves nothing serving.
#[test]
fn rollback_of_a_restart_starts_the_old_instance() {
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    assert_eq!(
        fake.snapshot().running.keys().copied().collect::<Vec<_>>(),
        [9302]
    );
    fake.advance(&[Step::Rollback]);
    let snap = fake.snapshot();
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9301]);
    assert_eq!(snap.redirect, Some(9301));
    assert_eq!(snap.record.as_ref().unwrap().previous, None);
}

/// Asserts a rollback of an update that goes the other way (from 9302 to 9301). It would catch a
/// rollback that assumes the first slot is the old one.
#[test]
fn rollback_works_in_both_directions() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    let params = fake.params_for(NEWER, "0.5.0");
    let ports = fake.ports();
    for step in [Step::Prepare, Step::Cutover] {
        let done = super::run_step(step, Some(&params), &ports);
        assert_eq!(done.outcome, Outcome::Success, "{step}: {done:?}");
    }
    let done = super::run_step(Step::Rollback, Some(&params), &ports);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let snap = fake.snapshot();
    assert_eq!(snap.redirect, Some(9302));
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9302]);
    assert!(!snap.links.contains_key(&9301));
    assert_eq!(snap.record.unwrap().serving, 9302);
}

/// Asserts the order of the calls of a restart-mode rollback: the new unit is stopped first, then
/// the old one started, then the rule moved, and the host never has two instances of the release
/// that has no lock running together. It would catch a rollback that starts the old release
/// beside the new one, which are then two writers on one cache directory.
#[test]
fn a_restart_rollback_stops_the_new_unit_before_it_starts_the_old_one() {
    let fake = Fake::legacy();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let mark = fake.changes().len();
    fake.advance(&[Step::Rollback]);
    assert_eq!(
        fake.changes()[mark..],
        [
            "units.stop tierstore@9302.service",
            "units.start tierstore@9301.service",
            "nft.apply kbf_forward_tierstore",
            "units.stop tierstore@9302.service",
            "slots.remove 9302",
            "state.save"
        ]
    );
    assert!(fake.errors().is_empty(), "{:?}", fake.errors());
}

/// Asserts that a handover-mode rollback still starts the old unit first (both run side by side
/// by design there, and the new one is read-only), and that the fake records no error. It would
/// catch the restart-mode rule applied to a hand-over, which would leave a gap.
#[test]
fn a_handover_rollback_starts_the_old_unit_first() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover]);
    let mark = fake.changes().len();
    fake.advance(&[Step::Rollback]);
    assert!(fake.changes()[mark].starts_with("units.start"));
    assert!(fake.errors().is_empty());
}

/// Asserts that rollback removes the guard table when this update placed it, and leaves one that
/// was there before. It would catch a rollback that strips the guard from a host that had it, or
/// leaves an unguarded private port open on one that did not.
#[test]
fn rollback_removes_the_guard_only_if_the_update_placed_it() {
    let fake = Fake::new();
    fake.remove_guard();
    fake.advance(&[Step::Prepare]);
    assert!(fake.record().guard_placed && fake.snapshot().guard);
    fake.advance(&[Step::Rollback]);
    assert!(!fake.snapshot().guard);
    assert!(!fake.record().guard_placed);
    let kept = Fake::new();
    kept.advance(&[Step::Prepare, Step::Rollback]);
    assert!(kept.snapshot().guard);
    assert!(!kept.log().iter().any(|c| c.starts_with("nft.delete")));
}
