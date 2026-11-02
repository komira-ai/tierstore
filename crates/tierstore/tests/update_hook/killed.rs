//! The hook killed at each boundary: the fake that receives the n-th call does it, logs it, and
//! kills the hook (SIGKILL) before the hook sees the answer. The interrupted step is then run
//! again, or the update rolled back, by the real binary. Forward handover and restart, rollback
//! in both modes, and the boot entry are each cut after every call they make.

use std::collections::BTreeSet;

use tierstore::server::update_hook::{GUARD_TABLE, Marker};

use crate::flows::{kinds, public_answers};
use crate::rig::{Machine, NEW_VERSION, Rig, Setup};

/// The steps of an update, in order.
const FORWARD: [&str; 4] = ["prepare", "cutover", "verify", "cleanup"];

/// One matrix: a host, the steps run before the cut (never cut), whether the host reboots
/// then, and the steps that are cut after each of their `calls` calls.
struct Matrix {
    name: &'static str,
    setup: Setup,
    before: &'static [&'static str],
    reboot: bool,
    steps: &'static [&'static str],
    calls: usize,
}

/// A host cut after its `n`-th call: the host, what the fakes held before the cut steps, and
/// the index of the step that was cut.
struct Cut {
    rig: Rig,
    prior: Machine,
    killed: usize,
}

/// Asserts that the two units never run at once in a restart, where the release that runs
/// has no lock.
fn one_at_most(rig: &Rig, setup: Setup, n: usize) {
    if !setup.old_handover {
        assert!(rig.running().len() <= 1, "both run, cut after call {n}");
    }
}

impl Matrix {
    /// Runs the steps before the cut, then the cut steps until the `n`-th call kills one.
    fn cut(&self, n: usize) -> Cut {
        let rig = Rig::new(&format!("{}-{n}", self.name), self.setup);
        for step in self.before {
            rig.step(step).ok();
        }
        if self.reboot {
            rig.reboot("boot-two");
        }
        rig.world.clear_logs();
        let prior = rig.machine();
        rig.world.kill_after(Some(n));
        let killed = self
            .steps
            .iter()
            .position(|step| {
                let ran = rig.step(step);
                one_at_most(&rig, self.setup, n);
                if !ran.killed() {
                    assert_eq!(ran.code, Some(0), "cut after call {n}, {step}: {ran:?}");
                }
                ran.killed()
            })
            .unwrap_or_else(|| panic!("no step was cut after call {n}"));
        assert_eq!(rig.calls().len(), n, "cut after call {n}");
        rig.world.kill_after(None);
        Cut { rig, prior, killed }
    }

    /// Cuts after each call in turn, runs the cut step and the rest again, and checks the end.
    fn run_on(&self, check: impl Fn(&Cut, usize)) {
        for n in 1..=self.calls {
            let cut = self.cut(n);
            for step in &self.steps[cut.killed..] {
                let ran = cut.rig.step(step);
                assert_eq!(ran.code, Some(0), "cut after call {n}, {step}: {ran:?}");
                one_at_most(&cut.rig, self.setup, n);
            }
            none_repeated(&cut.rig, n);
            check(&cut, n);
        }
    }
}

/// Asserts that no change of the fakes was made twice.
fn none_repeated(rig: &Rig, n: usize) {
    let effects = rig.effects();
    let distinct: BTreeSet<&String> = effects.iter().collect();
    assert_eq!(
        distinct.len(),
        effects.len(),
        "cut after call {n}: {effects:#?}"
    );
}

/// Asserts the end of an update: the new slot alone, behind the redirect and the guard, the
/// record committed with no previous slot, the old link gone, the public port answered by it.
fn finished(rig: &Rig, n: usize) {
    let (a, b) = (rig.a, rig.b);
    assert_eq!(rig.running(), [b], "cut after call {n}");
    assert_eq!(rig.redirect(), Some(b), "cut after call {n}");
    assert!(rig.has_table(GUARD_TABLE), "cut after call {n}");
    let record = rig.record();
    assert_eq!((record.serving, record.previous), (b, None), "{n}");
    assert_eq!(
        record.markers,
        BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
        "cut after call {n}"
    );
    assert_eq!(rig.link(a), None, "cut after call {n}");
    public_answers(rig, b, NEW_VERSION);
}

/// Asserts that a handover cut after each of its eight calls (three in `prepare`, one in
/// `cutover`, one in `verify`, three in `cleanup`), after the fake has done what the call asks,
/// and then run again from the step that was cut, finishes without making any change twice: the
/// fakes change exactly as in a run nobody cut (the guard loaded, the new unit started, the
/// redirect moved, the old unit stopped, once each). It would catch a re-run that starts a
/// second instance, swaps the redirect back and forth, or stops the old unit before the commit
/// is on record.
#[test]
fn a_handover_cut_after_any_call_finishes_when_run_again_and_repeats_nothing() {
    let matrix = Matrix {
        name: "hook-conformance-killed",
        setup: Setup::handover(),
        before: &[],
        reboot: false,
        steps: &FORWARD,
        calls: 8,
    };
    matrix.run_on(|cut, n| {
        let (a, b) = (cut.rig.a, cut.rig.b);
        assert_eq!(
            kinds(&cut.rig.effects()),
            [
                "set kbf_guard_tierstore",
                format!("start tierstore@{b}.service").as_str(),
                "set kbf_forward_tierstore",
                format!("stop tierstore@{a}.service").as_str(),
            ],
            "cut after call {n}"
        );
        finished(&cut.rig, n);
    });
}

/// Asserts that a restart cut after each of its nine calls (two in `prepare`, the stop, the start
/// and the redirect of `cutover`, one in `verify`, three in `cleanup`) and run again finishes,
/// makes no change twice, and never has both units running after any run. It would catch a
/// re-run of a restart that starts the new release beside the old one.
#[test]
fn a_restart_cut_after_any_call_finishes_and_never_runs_both() {
    let matrix = Matrix {
        name: "hook-conformance-killed-restart",
        setup: Setup::restart(),
        before: &[],
        reboot: false,
        steps: &FORWARD,
        calls: 9,
    };
    matrix.run_on(|cut, n| finished(&cut.rig, n));
}

/// Asserts that a hook cut after each call of a handover and then rolled back ends with the host
/// it found, making no change twice: before the commit `rollback` restores the old slot behind
/// the redirect, stops the new one and removes what the update placed; the one thing it leaves
/// is a guard loaded by a `prepare` cut before it could record that (the design states that case,
/// and the guard only keeps other hosts off the private ports). After the commit (cut after the
/// old unit's stop) `rollback` refuses as irreversible and a `cleanup` run again finishes. It
/// would catch a rollback that cannot undo a step cut short, or one that undoes a commit.
#[test]
fn a_handover_cut_after_any_call_is_rolled_back_to_the_host_it_found() {
    let matrix = Matrix {
        name: "hook-conformance-killed-then-rollback",
        setup: Setup::handover(),
        before: &[],
        reboot: false,
        steps: &FORWARD,
        calls: 8,
    };
    for n in 1..=matrix.calls {
        let Cut { rig, prior, .. } = matrix.cut(n);
        let ran = rig.step("rollback");
        if rig.record().committed() {
            assert_eq!(
                ran.line["reason"], "committed",
                "cut after call {n}: {ran:?}"
            );
            assert_eq!(ran.line["reversible"], false, "{n}");
            rig.step("cleanup").ok();
            finished(&rig, n);
        } else {
            assert_eq!(ran.code, Some(0), "cut after call {n}: {ran:?}");
            let mut after = rig.machine();
            if n == 2 {
                assert!(
                    after.tables.remove(GUARD_TABLE).is_some(),
                    "a guard loaded and not recorded stays"
                );
            }
            assert_eq!(after, prior, "cut after call {n}");
            assert_eq!(rig.link(rig.b), None, "cut after call {n}");
            let record = rig.record();
            assert_eq!((record.serving, record.previous), (rig.a, None), "{n}");
            assert!(record.markers.is_empty(), "cut after call {n}");
        }
        none_repeated(&rig, n);
    }
}

/// Asserts that `rollback` itself, cut after each of its calls (a handover's four: the old
/// unit's start, the redirect, the new unit's stop, the guard's removal; a restart's five, the
/// new unit stopped before the old one starts) and run again, leaves the host as it was before
/// the update and never runs both units of a restart. It would catch a rollback whose own crash
/// leaves the redirect on a stopped slot, or the guard behind.
#[test]
fn a_rollback_cut_after_any_call_finishes_when_run_again() {
    for (setup, calls, name) in [
        (Setup::handover(), 4, "hook-conformance-killed-rollback"),
        (
            Setup::restart(),
            5,
            "hook-conformance-killed-rollback-restart",
        ),
    ] {
        let matrix = Matrix {
            name,
            setup,
            before: &["prepare", "cutover"],
            reboot: false,
            steps: &["rollback"],
            calls,
        };
        matrix.run_on(|cut, n| {
            let rig = &cut.rig;
            assert_eq!(rig.running(), [rig.a], "cut after call {n}");
            assert_eq!(rig.redirect(), Some(rig.a), "cut after call {n}");
            assert!(!rig.has_table(GUARD_TABLE), "cut after call {n}");
            assert_eq!(rig.link(rig.b), None, "cut after call {n}");
            let record = rig.record();
            assert_eq!((record.serving, record.previous), (rig.a, None), "{n}");
        });
    }
}

/// Asserts that the boot entry, cut after each of its three calls (the guard, the redirect, the
/// start of the slot) after a reboot in the middle of an update, and run again, ends with the old
/// slot alone behind the redirect and the guard, the record as it was. It would catch a boot
/// whose own crash leaves the public port on no slot.
#[test]
fn restore_cut_after_any_call_finishes_when_run_again() {
    let matrix = Matrix {
        name: "hook-conformance-killed-restore",
        setup: Setup::handover(),
        before: &["prepare", "cutover"],
        reboot: true,
        steps: &["restore"],
        calls: 3,
    };
    matrix.run_on(|cut, n| {
        let rig = &cut.rig;
        assert_eq!(rig.running(), [rig.a], "cut after call {n}");
        assert_eq!(rig.redirect(), Some(rig.a), "cut after call {n}");
        assert!(rig.has_table(GUARD_TABLE), "cut after call {n}");
        let record = rig.record();
        assert_eq!(
            (record.serving, record.previous),
            (rig.b, Some(rig.a)),
            "{n}"
        );
        assert!(cut.prior.units.is_empty(), "the reboot stopped every unit");
    });
}
