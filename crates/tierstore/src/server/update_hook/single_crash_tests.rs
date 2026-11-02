//! Every single-instance step, with each of its calls failing in turn, and then run again: the
//! host ends in the state of a run that was never interrupted. The model is that of the two-slot
//! steps (`steps_crash_tests.rs`): a failing call is a crash at that boundary seen from the step;
//! a halted world is a crash that no recovery code of the step gets through.

use super::single_testing::{SingleFake, Snapshot};
use super::{Done, Outcome, Step};

struct Scenario {
    name: &'static str,
    setup: &'static [Step],
    act: Step,
}

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Verify};

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "prepare",
        setup: &[],
        act: Prepare,
    },
    Scenario {
        name: "cutover",
        setup: &[Prepare],
        act: Cutover,
    },
    Scenario {
        name: "verify",
        setup: &[Prepare, Cutover],
        act: Verify,
    },
    Scenario {
        name: "cleanup",
        setup: &[Prepare, Cutover, Verify],
        act: Cleanup,
    },
    Scenario {
        name: "rollback after prepare",
        setup: &[Prepare],
        act: Rollback,
    },
    Scenario {
        name: "rollback after cutover",
        setup: &[Prepare, Cutover],
        act: Rollback,
    },
    Scenario {
        name: "rollback after verify",
        setup: &[Prepare, Cutover, Verify],
        act: Rollback,
    },
    Scenario {
        name: "second prepare",
        setup: &[Prepare, Cutover, Verify, Cleanup],
        act: Prepare,
    },
    Scenario {
        name: "restore",
        setup: &[Prepare, Cutover],
        act: Restore,
    },
    Scenario {
        name: "restore after the commit",
        setup: &[Prepare, Cutover, Verify, Cleanup],
        act: Restore,
    },
];

fn start(s: &Scenario) -> SingleFake {
    let fake = SingleFake::new();
    fake.advance(s.setup);
    if s.act == Restore {
        fake.reboot();
    }
    fake
}

/// What a failed first run may say of the commit: never `"reversible":true` once the record on
/// disk says `stopping`, and `"reversible":false` without it only for a `cleanup` that began to
/// write `stopping` and could not read back whether the write reached the disk (an undecided
/// commit counts as made). Such a host is finished by the next `cleanup`, forward, which each
/// sweep runs.
fn check_reversible(s: &Scenario, fake: &SingleFake, first: &Done, n: usize, from_log: usize) {
    let committed = fake.snapshot().record.is_some_and(|r| r.committed());
    // The write of `stopping` is this run's only save before its failure: a cleanup writes
    // nothing else until the commit is on disk.
    let undecided = s.act == Cleanup
        && !committed
        && !first.report.reversible
        && fake.log()[from_log..]
            .iter()
            .any(|call| call == "state.save");
    assert!(
        first.report.reversible != committed || undecided,
        "{} at {n}: {first:?}",
        s.name
    );
}

fn check_line(done: &Done) {
    let line: serde_json::Value = serde_json::from_str(&done.report.to_line()).unwrap();
    assert!(line["reversible"].is_boolean() && line["class"].is_string());
}

/// For each scenario: a run with every call failing in turn, then a clean run; the end state is
/// that of a clean run, the lock is released, the report line is well formed, and a report says
/// `"reversible":false` exactly when the record on disk says `stopping`. It would catch a step
/// that does not converge after a failure at some boundary (a `cutover` whose own rollback left
/// the update unfinishable, say), one that leaves the lock held, and one that reports a commit
/// it did not write.
#[test]
fn each_single_step_converges_after_a_failure_at_every_call() {
    for s in SCENARIOS {
        let clean = start(s);
        let from = clean.calls();
        let done = clean.run(s.act);
        assert_eq!(done.outcome, Outcome::Success, "{}: {done:?}", s.name);
        let end: Snapshot = clean.snapshot();
        let was_committed = start(s).snapshot().record.is_some_and(|r| r.committed());
        let calls = clean.calls() - from;
        assert!(calls > 3, "{}: {calls} calls", s.name);
        for n in 1..=calls {
            let fake = start(s);
            fake.fail_at(Some(n));
            let from_log = fake.log().len();
            let first = fake.run(s.act);
            check_line(&first);
            assert_eq!(fake.live_locks(), 0, "{} at {n}", s.name);
            if first.outcome != Outcome::Success && !was_committed {
                check_reversible(s, &fake, &first, n, from_log);
            }
            fake.fail_at(None);
            let second = fake.run(s.act);
            assert_eq!(
                second.outcome,
                Outcome::Success,
                "{} at {n}: {second:?}",
                s.name
            );
            assert_eq!(fake.snapshot(), end, "{} at {n}", s.name);
            assert_eq!(fake.live_locks(), 0);
        }
    }
}

/// Asserts that a single-instance step run a second time after it has converged changes nothing:
/// for each scenario, a clean run, then another, and the end states are equal. It would catch a
/// step that is only idempotent after a failure.
#[test]
fn each_single_step_run_twice_ends_where_it_ended_once() {
    for s in SCENARIOS {
        let fake = start(s);
        fake.advance(&[s.act]);
        let once = fake.snapshot();
        fake.advance(&[s.act]);
        assert_eq!(fake.snapshot(), once, "{}", s.name);
    }
}

/// For each scenario: the world stops from the n-th call on, so that no recovery code of the
/// step runs (the process is gone, and what it had done stays done: the service stopped, the
/// link swapped), and then a clean run finishes the step. The end state is that of a run that
/// was never interrupted, and a report says `"reversible":false` exactly when the record on disk
/// says `stopping`. It would catch a step that only converges because its own recovery ran, such
/// as a `cutover` that cannot restart a service a crash left stopped.
#[test]
fn each_single_step_converges_after_the_world_stops_at_any_call() {
    for s in SCENARIOS {
        let clean = start(s);
        let from = clean.calls();
        clean.advance(&[s.act]);
        let end: Snapshot = clean.snapshot();
        let calls = clean.calls() - from;
        let was_committed = start(s).snapshot().record.is_some_and(|r| r.committed());
        for n in 1..=calls {
            let fake = start(s);
            fake.halt_at(Some(n));
            let from_log = fake.log().len();
            let first = fake.run(s.act);
            check_line(&first);
            assert_ne!(
                first.outcome,
                Outcome::Success,
                "{} at {n}: {first:?}",
                s.name
            );
            assert_eq!(fake.live_locks(), 0, "{} at {n}", s.name);
            if !was_committed {
                check_reversible(s, &fake, &first, n, from_log);
            }
            fake.halt_at(None);
            let second = fake.run(s.act);
            assert_eq!(
                second.outcome,
                Outcome::Success,
                "{} at {n}: {second:?}",
                s.name
            );
            assert_eq!(fake.snapshot(), end, "{} at {n}", s.name);
        }
    }
}

/// The fake of a host whose staged release `NEW` answers with another version, after `prepare`:
/// its `cutover` fails the probe and rolls back.
fn failing() -> SingleFake {
    let fake = SingleFake::new();
    fake.lie(super::single_testing::NEW, "9.9.9");
    fake.advance(&[Prepare]);
    fake
}

/// Asserts that a `cutover` whose probe fails, interrupted at any of its calls (one call failing,
/// or the world stopping there, inside the swap or inside its own rollback), is finished by the
/// updater's `rollback` in the state an uninterrupted failed `cutover` and `rollback` reach: the
/// old release running from `current`, no `previous`, nothing in flight. It would catch a crash
/// inside the in-cutover rollback that leaves the new release linked or the service stopped.
#[test]
fn a_failed_cutover_interrupted_anywhere_is_finished_by_rollback() {
    let clean = failing();
    let from = clean.calls();
    let failed = clean.run(Cutover);
    assert_eq!(failed.outcome, Outcome::Failure, "{failed:?}");
    let calls = clean.calls() - from;
    clean.advance(&[Rollback]);
    let end = clean.snapshot();
    assert_eq!(end.running, end.current);
    for n in 1..=calls {
        for halt in [false, true] {
            let fake = failing();
            if halt {
                fake.halt_at(Some(n));
            } else {
                fake.fail_at(Some(n));
            }
            let first = fake.run(Cutover);
            check_line(&first);
            assert_eq!(fake.live_locks(), 0, "at {n}");
            fake.halt_at(None);
            fake.fail_at(None);
            let back = fake.run(Rollback);
            assert_eq!(
                back.outcome,
                Outcome::Success,
                "at {n}, halt {halt}: {back:?}"
            );
            assert_eq!(fake.snapshot(), end, "at {n}, halt {halt}");
        }
    }
}

/// For each scenario: every call in turn reports a failure after its effect has happened (a
/// service started or stopped, a link made or removed, a record written, and then an error, as a
/// rename followed by a failed sync gives), then a clean run. The end state is that of a run
/// that was never interrupted, and the first report says `"reversible":false` exactly when the
/// record on disk says `stopping`. It would catch a step that a reported failure leaves in a
/// state it cannot finish, or that hides a commit it made.
#[test]
fn each_single_step_converges_after_a_failure_reported_after_its_effect() {
    for s in SCENARIOS {
        let clean = start(s);
        let from = clean.calls();
        clean.advance(&[s.act]);
        let end: Snapshot = clean.snapshot();
        let calls = clean.calls() - from;
        let was_committed = start(s).snapshot().record.is_some_and(|r| r.committed());
        for n in 1..=calls {
            let fake = start(s);
            fake.fail_late_at(Some(n));
            let from_log = fake.log().len();
            let first = fake.run(s.act);
            check_line(&first);
            assert_eq!(fake.live_locks(), 0, "{} at {n}", s.name);
            // A step that fails before it can read the record says nothing of a commit that an
            // earlier run made.
            if first.outcome != Outcome::Success && !was_committed {
                check_reversible(s, &fake, &first, n, from_log);
            }
            fake.fail_late_at(None);
            let second = fake.run(s.act);
            assert_eq!(
                second.outcome,
                Outcome::Success,
                "{} at {n}: {second:?}",
                s.name
            );
            assert_eq!(fake.snapshot(), end, "{} at {n}", s.name);
        }
    }
}
