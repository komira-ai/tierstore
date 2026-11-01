//! `restore`, the boot entry.

use std::collections::BTreeSet;

use super::testing::Fake;
use super::{Class, Marker, Outcome, Reason, Record, Step};

fn slots(fake: &Fake) -> Vec<u16> {
    fake.snapshot().running.keys().copied().collect()
}

/// Asserts that with no update in flight a reboot starts the serving slot only, loads the guard
/// and points the rule at it, in that order. It would catch a boot that starts both slots, or
/// leaves the private ports open before the guard is loaded.
#[test]
fn restore_starts_the_serving_slot() {
    let fake = Fake::new();
    fake.reboot();
    let done = fake.run(Step::Restore);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        fake.changes(),
        [
            "nft.apply kbf_guard_tierstore",
            "nft.apply kbf_forward_tierstore",
            "units.start tierstore@9301.service"
        ]
    );
    let snap = fake.snapshot();
    assert_eq!((snap.redirect, snap.guard), (Some(9301), true));
    assert_eq!(slots(&fake), [9301]);
}

/// Asserts that a reboot with an update in flight and no `stopping` is an abort: only the
/// previous slot is started and named in the rule, the record is not changed, and the other slot
/// does not run. It would catch a boot that starts the new slot, whose instance would then find
/// the lock free and write while the updater rolls the update back.
#[test]
fn a_reboot_before_the_commit_restores_the_previous_slot() {
    for steps in [
        vec![Step::Prepare],
        vec![Step::Prepare, Step::Cutover],
        vec![Step::Prepare, Step::Cutover, Step::Verify],
    ] {
        let fake = Fake::new();
        fake.advance(&steps);
        let record = fake.record();
        fake.reboot();
        fake.advance(&[Step::Restore]);
        let snap = fake.snapshot();
        assert_eq!(snap.redirect, Some(9301), "{steps:?}");
        assert_eq!(slots(&fake), [9301]);
        assert_eq!(snap.record.unwrap(), record);
    }
}

/// Asserts that a reboot after `stopping` restores the new slot, finished or half done. It would
/// catch a boot that restarts the instance `cleanup` was retiring.
#[test]
fn a_reboot_after_stopping_restores_the_new_slot() {
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]);
    fake.reboot();
    fake.advance(&[Step::Restore]);
    assert_eq!(slots(&fake), [9302]);
    assert_eq!(fake.snapshot().redirect, Some(9302));
    let fake = Fake::new();
    fake.advance(&[Step::Prepare, Step::Cutover, Step::Verify]);
    let half = Record {
        markers: BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        ..fake.record()
    };
    fake.set_record(&half);
    fake.reboot();
    fake.advance(&[Step::Restore]);
    assert_eq!(slots(&fake), [9302]);
    assert_eq!(fake.record(), half);
}

/// Asserts that `restore` twice leaves the same state, and repairs a ruleset lost to a flush.
/// It would catch a boot entry that is not safe to run again.
#[test]
fn restore_twice_changes_nothing_and_repairs_the_rules() {
    let fake = Fake::new();
    fake.reboot();
    fake.advance(&[Step::Restore]);
    let once = fake.snapshot();
    fake.advance(&[Step::Restore]);
    assert_eq!(fake.snapshot(), once);
}

/// Asserts that with no record there is nothing to restore and no call is made, and that a slot
/// with no link is refused before any rule is touched. It would catch a boot that starts a slot
/// nothing installed.
#[test]
fn restore_with_no_record_or_no_link_does_nothing() {
    let fake = Fake::new();
    fake.set_record_bytes(None);
    let done = fake.run(Step::Restore);
    assert_eq!(done.outcome, Outcome::Success);
    assert!(fake.changes().is_empty());
    let fake = Fake::new();
    fake.drop_link(9301);
    fake.reboot();
    let done = fake.run(Step::Restore);
    assert_eq!(done.report.reason, Some(Reason::NoSlot));
    assert_eq!(done.report.class, Class::Environment);
    assert!(fake.changes().is_empty());
}
