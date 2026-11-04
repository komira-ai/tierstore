//! Every step with each of its calls failing in turn, and then run again: the host ends in the
//! state of a run that was never interrupted. A failing call is a crash at that boundary seen
//! from the step; a halted world is a crash that no recovery code of the step gets through; a
//! call that fails after its effect is a rename made and its sync failed.

use super::fake::{Fake, assert_failed, assert_ok};
use crate::server::update_hook::{Done, Outcome, Reason, Step};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Uninstall, Verify};

struct Scenario {
    name: &'static str,
    first: bool,
    setup: &'static [Step],
    act: Step,
}

const fn scenario(name: &'static str, first: bool, setup: &'static [Step], act: Step) -> Scenario {
    Scenario {
        name,
        first,
        setup,
        act,
    }
}

const SCENARIOS: &[Scenario] = &[
    scenario("prepare", false, &[], Prepare),
    scenario("cutover", false, &[Prepare], Cutover),
    scenario("verify", false, &[Prepare, Cutover], Verify),
    scenario("cleanup", false, &[Prepare, Cutover, Verify], Cleanup),
    scenario("rollback after prepare", false, &[Prepare], Rollback),
    scenario(
        "rollback after cutover",
        false,
        &[Prepare, Cutover],
        Rollback,
    ),
    scenario(
        "rollback after verify",
        false,
        &[Prepare, Cutover, Verify],
        Rollback,
    ),
    scenario(
        "second prepare",
        false,
        &[Prepare, Cutover, Verify, Cleanup],
        Prepare,
    ),
    scenario("restore", false, &[Prepare, Cutover], Restore),
    scenario(
        "restore after the commit",
        false,
        &[Prepare, Cutover, Verify, Cleanup],
        Restore,
    ),
    scenario("uninstall", false, &[Prepare, Cutover], Uninstall),
    scenario("first prepare", true, &[], Prepare),
    scenario("first cutover", true, &[Prepare], Cutover),
    scenario("first verify", true, &[Prepare, Cutover], Verify),
    scenario("first cleanup", true, &[Prepare, Cutover, Verify], Cleanup),
    scenario("first uninstall", true, &[Prepare, Cutover], Uninstall),
];

fn start(s: &Scenario) -> Fake {
    let fake = if s.first {
        Fake::first_install()
    } else {
        Fake::new()
    };
    fake.advance(s.setup);
    if s.act == Restore {
        fake.reboot();
    }
    fake
}

/// A report says `"reversible":false` exactly when the record on disk says `stopping`, except
/// for a `cleanup` that began to write `stopping` and could not read back whether the write
/// reached the disk: an undecided commit counts as made.
fn check_reversible(s: &Scenario, fake: &Fake, first: &Done, n: usize, from_log: usize) {
    let committed = fake.snapshot().record.is_some_and(|r| r.committed());
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

/// How a call goes wrong in the sweep.
#[derive(Clone, Copy, Debug)]
enum Fault {
    Fails,
    Halts,
    FailsLate,
}

fn inject(fake: &Fake, fault: Fault, n: usize) {
    match fault {
        Fault::Fails => fake.fail_at(n),
        Fault::Halts => fake.halt_at(n),
        Fault::FailsLate => fake.fail_late_at(n),
    }
}

/// For each scenario and each way a call can go wrong (it fails without effect, the world stops
/// there, it fails after its effect), at every call of the step: the lock is released, the report
/// line is well formed, a report says `"reversible":false` exactly when the record says
/// `stopping`, and the step run again succeeds and leaves the host as an uninterrupted run
/// does. Catches a step that does not converge after a failure at some boundary (a `cutover`
/// that cannot finish after the job was booted out, say), one that leaves the lock held, and one
/// that reports a commit it did not write.
#[test]
fn each_step_converges_after_a_failure_at_every_call() {
    for s in SCENARIOS {
        let clean = start(s);
        let from = clean.calls();
        assert_ok(&clean.host_run(s.act));
        let end = clean.snapshot();
        let calls = clean.calls() - from;
        assert!(calls >= 2, "{}: {calls} calls", s.name);
        let was_committed = start(s).snapshot().record.is_some_and(|r| r.committed());
        for fault in [Fault::Fails, Fault::Halts, Fault::FailsLate] {
            for n in 1..=calls {
                let fake = start(s);
                inject(&fake, fault, n);
                let from_log = fake.log().len();
                let first = fake.host_run(s.act);
                let line: serde_json::Value = serde_json::from_str(&first.report.to_line())
                    .expect("the report is one JSON object");
                assert!(line["reversible"].is_boolean() && line["class"].is_string());
                assert_eq!(fake.live_locks(), 0, "{} at {n} {fault:?}", s.name);
                if matches!(fault, Fault::Halts) {
                    assert_ne!(first.outcome, Outcome::Success, "{} at {n}", s.name);
                }
                if first.outcome != Outcome::Success && !was_committed {
                    check_reversible(s, &fake, &first, n, from_log);
                }
                fake.heal();
                let second = fake.host_run(s.act);
                assert_eq!(
                    second.outcome,
                    Outcome::Success,
                    "{} at {n} {fault:?}: {second:?}",
                    s.name
                );
                assert_eq!(fake.snapshot(), end, "{} at {n} {fault:?}", s.name);
            }
        }
    }
}

/// Asserts that each step run a second time after it converged changes nothing: for each
/// scenario, a clean run, then another, and the end states are equal. Catches a step that is
/// only idempotent after a failure.
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

/// Asserts that a `cutover` whose probe fails, interrupted at any of its calls (a call failing,
/// or the world stopping there), is finished by `rollback` in the state an uninterrupted failed
/// `cutover` and `rollback` reach: the previous release serves from `current` and nothing is in
/// flight. Catches a crash inside the cutover that leaves the new release loaded for good.
#[test]
fn a_failed_cutover_interrupted_anywhere_is_finished_by_rollback() {
    let failing = || {
        let fake = Fake::new();
        fake.advance(&[Prepare]);
        fake.lie(super::fake::NEW, "9.9.9");
        fake
    };
    let clean = failing();
    let from = clean.calls();
    assert_failed(&clean.host_run(Cutover), Reason::ProbeFailed);
    let calls = clean.calls() - from;
    clean.advance(&[Rollback]);
    let end = clean.snapshot();
    for n in 1..=calls {
        for halt in [false, true] {
            let fake = failing();
            if halt {
                fake.halt_at(n);
            } else {
                fake.fail_at(n);
            }
            let _ = fake.host_run(Cutover);
            assert_eq!(fake.live_locks(), 0, "at {n}");
            fake.heal();
            let back = fake.host_run(Rollback);
            assert_eq!(
                back.outcome,
                Outcome::Success,
                "at {n}, halt {halt}: {back:?}"
            );
            assert_eq!(fake.snapshot(), end, "at {n}, halt {halt}");
        }
    }
}

/// Asserts that the rollback of a first install, with each of its calls failing in turn, says
/// `no_previous_release` when it is run again, and ends with no job loaded and the update on
/// record without markers. Catches a first-install rollback that a crash leaves with a job
/// loaded.
#[test]
fn the_rollback_of_a_first_install_converges_after_a_failure_at_every_call() {
    let setup = || {
        let fake = Fake::first_install();
        fake.advance(&[Prepare, Cutover, Verify]);
        fake
    };
    let clean = setup();
    let from = clean.calls();
    assert_failed(&clean.host_run(Rollback), Reason::NoPreviousRelease);
    let calls = clean.calls() - from;
    let end = clean.snapshot();
    for n in 1..=calls {
        let fake = setup();
        fake.halt_at(n);
        let first = fake.host_run(Rollback);
        assert_eq!(first.outcome, Outcome::Failure, "at {n}");
        fake.heal();
        assert_failed(&fake.host_run(Rollback), Reason::NoPreviousRelease);
        assert_eq!(fake.snapshot(), end, "at {n}");
        assert!(fake.strays().is_empty(), "at {n}");
    }
}
