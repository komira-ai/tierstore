//! `cleanup`.

use super::testing::{Fake, NEWER};
use super::{Class, Marker, Outcome, Reason, Record, Step, redirect_ruleset};

const PRIOR: [Step; 3] = [Step::Prepare, Step::Cutover, Step::Verify];

fn index(log: &[String], what: &str) -> usize {
    log.iter()
        .position(|c| c == what)
        .unwrap_or_else(|| panic!("no {what} in {log:?}"))
}

/// Asserts that `cleanup` without a `verified` marker exits 1, stops nothing and changes nothing:
/// before the cutover, after it, and with no update at all. It would catch a `cleanup` that
/// commits on the updater's word.
#[test]
fn cleanup_without_verified_stops_nothing() {
    for steps in [
        vec![],
        vec![Step::Prepare],
        vec![Step::Prepare, Step::Cutover],
    ] {
        let fake = Fake::new();
        fake.advance(&steps);
        let before = fake.snapshot();
        let mark = fake.changes().len();
        let done = fake.run(Step::Cleanup);
        assert_eq!(done.outcome, Outcome::Failure, "{steps:?}");
        assert_eq!(done.report.reason, Some(Reason::NotVerified));
        assert_eq!(done.report.class, Class::Release);
        assert!(done.report.reversible);
        assert_eq!(fake.changes().len(), mark);
        assert_eq!(fake.snapshot(), before);
    }
}

/// Asserts the commit: `stopping` is written first, then the previous slot's link is removed,
/// then its unit is stopped, then `previous` is cleared, and nothing else is done to a unit (no
/// mask: the list of changes is exact). The end state is link absent, unit stopped, `previous`
/// cleared, the new instance running alone, and the report says it is not reversible. It would
/// catch a stop before the link is gone (a restart could bring the old slot back) and a mask,
/// which the hook has no right to.
#[test]
fn cleanup_removes_the_link_then_stops_then_clears_previous() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    let mark = fake.changes().len();
    let done = fake.run(Step::Cleanup);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!done.report.reversible);
    assert_eq!(
        fake.changes()[mark..],
        [
            "state.save",
            "slots.remove 9301",
            "units.stop tierstore@9301.service",
            "state.save"
        ]
    );
    let snap = fake.snapshot();
    assert!(!snap.links.contains_key(&9301));
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9302]);
    let record = snap.record.unwrap();
    assert_eq!((record.serving, record.previous), (9302, None));
    assert!(record.markers.contains(&Marker::Stopping));
    let log = fake.log();
    assert!(index(&log, "slots.remove 9301") < index(&log, "units.stop tierstore@9301.service"));
}

/// Asserts that a second `cleanup` changes nothing and still says it is not reversible. It would
/// catch a re-run that fails because there is nothing left to stop.
#[test]
fn cleanup_twice_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    fake.advance(&[Step::Cleanup]);
    let once = fake.snapshot();
    let mark = fake.changes().len();
    let done = fake.run(Step::Cleanup);
    assert_eq!(done.outcome, Outcome::Success);
    assert!(!done.report.reversible);
    assert_eq!(fake.snapshot(), once);
    assert_eq!(fake.changes().len(), mark);
}

/// The number (from 1, among the calls of the step) of the first call of `cleanup` that starts
/// with `prefix`, found by a clean run on a twin host.
fn call_number(prefix: &str) -> usize {
    let twin = Fake::new();
    twin.advance(&PRIOR);
    let base = twin.log().len();
    twin.advance(&[Step::Cleanup]);
    twin.log()[base..]
        .iter()
        .position(|c| c.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix}"))
        + 1
}

/// Asserts that a stop that fails after `stopping` was written exits 1 with `"reversible":false`
/// and the class `environment`, leaves the record committed, and that a re-run goes forward to
/// the same end state as a clean run. It would catch a failure after the commit that reports the
/// update as still undoable.
#[test]
fn a_stop_that_fails_after_the_commit_is_not_reversible_and_is_re_run_forward() {
    let reference = Fake::new();
    reference.advance(&PRIOR);
    reference.advance(&[Step::Cleanup]);
    let fake = Fake::new();
    fake.advance(&PRIOR);
    let base = fake.log().len();
    fake.fail_at(Some(call_number("units.stop")));
    let done = fake.run(Step::Cleanup);
    assert!(fake.log()[base..].last().unwrap().starts_with("units.stop"));
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::UnitFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!done.report.reversible);
    assert!(fake.record().committed());
    fake.fail_at(None);
    fake.advance(&[Step::Cleanup]);
    assert_eq!(fake.snapshot(), reference.snapshot());
}

/// Asserts that a failure before `stopping` is written is still reversible and leaves the record
/// without it. It would catch a `cleanup` that reports a commit it did not make, which the updater
/// would record.
#[test]
fn a_failure_before_stopping_is_reversible() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    fake.fail_at(Some(call_number("state.save")));
    let done = fake.run(Step::Cleanup);
    assert_eq!(done.report.reason, Some(Reason::StateIo));
    assert!(done.report.reversible);
    assert!(!fake.record().committed());
    assert_eq!(fake.run(Step::Rollback).outcome, Outcome::Success);
}

/// Asserts that after a reboot between `verify` and `cleanup` the commit is refused: `restore`
/// starts only the previous slot and leaves the record, still `verified`, naming the new slot, and
/// a `cleanup` that stopped the previous unit would leave nothing serving. The step exits 1 with
/// its own reason, `"reversible":true`, changes nothing, and a `rollback` still works. It would
/// catch a `cleanup` that trusts `verified` over what it sees.
#[test]
fn cleanup_after_a_reboot_refuses_and_changes_nothing() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    fake.reboot();
    fake.advance(&[Step::Restore]);
    let before = fake.snapshot();
    let mark = fake.changes().len();
    let done = fake.run(Step::Cleanup);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NotConfirmed));
    assert_eq!(done.report.class, Class::Release);
    assert!(done.report.reversible);
    assert_eq!(fake.changes().len(), mark);
    assert_eq!(fake.snapshot(), before);
    assert_eq!(
        fake.snapshot().running.keys().copied().collect::<Vec<_>>(),
        [9301]
    );
    fake.advance(&[Step::Rollback]);
    assert_eq!(fake.record(), Record::idle(9301));
}

/// Asserts each thing `cleanup` looks at again, one at a time, with everything else as `verify`
/// left it: the boot of the host, the guard table, the redirect, the readiness of the new
/// instance and the build that answers through the public port. Each gives exit 1,
/// `not_confirmed`, `"reversible":true`, and no change. It would catch a `cleanup` that skips
/// one of them.
#[test]
fn cleanup_looks_again_at_each_thing_it_depends_on() {
    type Spoil = fn(&Fake);
    let cases: [(&str, Spoil); 5] = [
        ("a new boot", Fake::new_boot),
        ("no guard", Fake::remove_guard),
        ("the rule on the old slot", |f| {
            f.ports().nft.apply(&redirect_ruleset(9300, 9301)).unwrap();
        }),
        ("a new instance that does not answer", |f| f.mute(9302)),
        ("a wrong build", |f| f.lie(9302, "9.9.9")),
    ];
    for (what, spoil) in cases {
        let fake = Fake::new();
        fake.advance(&PRIOR);
        spoil(&fake);
        let before = fake.snapshot();
        let mark = fake.changes().len();
        let done = fake.run(Step::Cleanup);
        assert_eq!(
            done.report.reason,
            Some(Reason::NotConfirmed),
            "{what}: {done:?}"
        );
        assert!(done.report.reversible, "{what}");
        assert_eq!(fake.changes().len(), mark, "{what}");
        assert_eq!(fake.snapshot(), before, "{what}");
    }
}

/// Asserts that a `verify` run again in the new boot records that boot, which lets `cleanup`
/// go on, and that a re-run of `cleanup` after `stopping` does not look again (the commit is
/// made, and a new boot must not stop it going forward). It would catch a `cleanup` retry that
/// refuses to finish a commit.
#[test]
fn a_verify_in_the_new_boot_confirms_and_a_commit_goes_forward_in_any_boot() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    fake.new_boot();
    assert_eq!(
        fake.run(Step::Cleanup).report.reason,
        Some(Reason::NotConfirmed)
    );
    fake.advance(&[Step::Verify, Step::Cleanup]);
    let fake = Fake::new();
    fake.advance(&PRIOR);
    let stopping = Record {
        markers: fake
            .record()
            .markers
            .into_iter()
            .chain([Marker::Stopping])
            .collect(),
        ..fake.record()
    };
    fake.set_record(&stopping);
    fake.new_boot();
    fake.advance(&[Step::Cleanup]);
    assert_eq!(fake.record().previous, None);
}

/// Asserts the cleanup of a restart: the old unit is already stopped and stopping it is no error.
/// It would catch a `cleanup` that insists the unit it stops is running.
#[test]
fn cleanup_after_a_restart_stops_a_stopped_unit() {
    let fake = Fake::legacy();
    fake.advance(&PRIOR);
    fake.advance(&[Step::Cleanup]);
    let snap = fake.snapshot();
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9302]);
    assert!(!snap.links.contains_key(&9301));
}

/// Asserts a cleanup of an update that goes the other way, retiring slot 9302 and keeping 9301.
/// It would catch a `cleanup` that assumes the first slot is the old one.
#[test]
fn cleanup_retires_the_right_slot_in_both_directions() {
    let fake = Fake::new();
    fake.advance(&PRIOR);
    fake.advance(&[Step::Cleanup]);
    let params = fake.params_for(NEWER, "0.5.0");
    let ports = fake.ports();
    for step in [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup] {
        let done = super::run_step(step, Some(&params), &ports);
        assert_eq!(done.outcome, Outcome::Success, "{step}: {done:?}");
    }
    let snap = fake.snapshot();
    assert_eq!(snap.running.keys().copied().collect::<Vec<_>>(), [9301]);
    assert!(!snap.links.contains_key(&9302));
    assert_eq!(snap.redirect, Some(9301));
}
