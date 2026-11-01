//! Every step, with each of its calls failing in turn, and then run again: the host ends in the
//! state of a run that was never interrupted. A failing call is a crash at that boundary seen
//! from the step; the call is not made, so the next run finds the world as the crash left it.

use super::testing::{Fake, Snapshot};
use super::{Done, Outcome, Step};

struct Scenario {
    name: &'static str,
    make: fn() -> Fake,
    setup: &'static [Step],
    act: Step,
}

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Verify};

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "prepare",
        make: Fake::new,
        setup: &[],
        act: Prepare,
    },
    Scenario {
        name: "cutover",
        make: Fake::new,
        setup: &[Prepare],
        act: Cutover,
    },
    Scenario {
        name: "verify",
        make: Fake::new,
        setup: &[Prepare, Cutover],
        act: Verify,
    },
    Scenario {
        name: "cleanup",
        make: Fake::new,
        setup: &[Prepare, Cutover, Verify],
        act: Cleanup,
    },
    Scenario {
        name: "rollback after prepare",
        make: Fake::new,
        setup: &[Prepare],
        act: Rollback,
    },
    Scenario {
        name: "rollback after cutover",
        make: Fake::new,
        setup: &[Prepare, Cutover],
        act: Rollback,
    },
    Scenario {
        name: "rollback after verify",
        make: Fake::new,
        setup: &[Prepare, Cutover, Verify],
        act: Rollback,
    },
    Scenario {
        name: "restart prepare",
        make: Fake::legacy,
        setup: &[],
        act: Prepare,
    },
    Scenario {
        name: "restart cutover",
        make: Fake::legacy,
        setup: &[Prepare],
        act: Cutover,
    },
    Scenario {
        name: "restart verify",
        make: Fake::legacy,
        setup: &[Prepare, Cutover],
        act: Verify,
    },
    Scenario {
        name: "restart cleanup",
        make: Fake::legacy,
        setup: &[Prepare, Cutover, Verify],
        act: Cleanup,
    },
    Scenario {
        name: "restart rollback",
        make: Fake::legacy,
        setup: &[Prepare, Cutover],
        act: Rollback,
    },
    Scenario {
        name: "second prepare",
        make: Fake::new,
        setup: &[Prepare, Cutover, Verify, Cleanup],
        act: Prepare,
    },
    Scenario {
        name: "restore",
        make: Fake::new,
        setup: &[Prepare, Cutover],
        act: Restore,
    },
    Scenario {
        name: "restore at rest",
        make: Fake::new,
        setup: &[],
        act: Restore,
    },
    Scenario {
        name: "restore after the commit",
        make: Fake::new,
        setup: &[Prepare, Cutover, Verify, Cleanup],
        act: Restore,
    },
];

fn start(s: &Scenario) -> Fake {
    let fake = (s.make)();
    fake.advance(s.setup);
    if s.act == Restore {
        fake.reboot();
    }
    fake
}

fn check_line(done: &Done) {
    let line: serde_json::Value = serde_json::from_str(&done.report.to_line()).unwrap();
    assert!(line["reversible"].is_boolean() && line["class"].is_string());
}

/// For each scenario: a run with every call failing in turn, then a clean run; the end state is
/// that of a clean run, the lock is released, the report line is well formed, and a report says
/// `"reversible":false` exactly when the record on disk says `stopping`. It would catch a step
/// that does not converge after a crash at some boundary, one that leaves the lock held, and
/// one that reports a commit it did not write (or hides one it did).
#[test]
fn each_step_converges_after_a_failure_at_every_call() {
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
            let first = fake.run(s.act);
            check_line(&first);
            assert_eq!(fake.live_locks(), 0, "{} at {n}", s.name);
            assert!(
                fake.errors().is_empty(),
                "{} at {n}: {:?}",
                s.name,
                fake.errors()
            );
            // A step that fails before it can read the record says nothing of a commit that
            // an earlier run made.
            if first.outcome != Outcome::Success && !was_committed {
                let committed = fake.snapshot().record.is_some_and(|r| r.committed());
                assert_eq!(
                    first.report.reversible, !committed,
                    "{} at {n}: {first:?}",
                    s.name
                );
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

/// Asserts that a step run a third time after it has converged still changes nothing: for each
/// scenario, a clean run, then another, and the end states are equal. It would catch a step that
/// is only idempotent after a failure.
#[test]
fn each_step_run_twice_ends_where_it_ended_once() {
    for s in SCENARIOS {
        let fake = start(s);
        fake.advance(&[s.act]);
        let once = fake.snapshot();
        fake.advance(&[s.act]);
        assert_eq!(fake.snapshot(), once, "{}", s.name);
    }
}

/// For each scenario: the world stops from the n-th call on, so that no recovery code of the step
/// runs (a real crash: the process is gone, and what it had done stays done), and then a clean run
/// finishes the step. The end state is that of a run that was never interrupted, the host never
/// held two instances of a release that has no lock, and a report says `"reversible":false`
/// exactly when the record on disk says `stopping`. It would catch a step that only converges
/// because its own recovery code ran.
#[test]
fn each_step_converges_after_the_world_stops_at_any_call() {
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
                let committed = fake.snapshot().record.is_some_and(|r| r.committed());
                assert_eq!(
                    first.report.reversible, !committed,
                    "{} at {n}: {first:?}",
                    s.name
                );
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
            assert!(
                fake.errors().is_empty(),
                "{} at {n}: {:?}",
                s.name,
                fake.errors()
            );
        }
    }
}
