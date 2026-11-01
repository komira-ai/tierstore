//! The hook killed at each boundary: the fake that receives the n-th call does it, logs it, and
//! kills the hook (SIGKILL) before the hook sees the answer. The update is then run on, or
//! rolled back, by the real binary.

use std::collections::BTreeSet;

use tierstore::server::update_hook::{GUARD_TABLE, Marker};

use crate::flows::{kinds, public_answers};
use crate::rig::{NEW_VERSION, Rig, Setup};

/// The steps of a handover, in order.
const STEPS: [&str; 4] = ["prepare", "cutover", "verify", "cleanup"];
/// The calls a handover makes to the fakes: three in `prepare`, one in `cutover`, one in
/// `verify`, three in `cleanup`.
const CALLS: usize = 8;

/// Runs the steps from the first until one is killed, and returns its index.
fn run_until_killed(rig: &Rig, n: usize) -> usize {
    for (index, step) in STEPS.iter().enumerate() {
        let ran = rig.step(step);
        if ran.killed() {
            return index;
        }
        assert_eq!(ran.code, Some(0), "killed after call {n}, {step}: {ran:?}");
    }
    panic!("no step was killed with the kill after call {n}");
}

/// Asserts that no change of the fakes was made twice.
fn none_repeated(rig: &Rig, n: usize) {
    let effects = rig.effects();
    let distinct: BTreeSet<&String> = effects.iter().collect();
    assert_eq!(
        distinct.len(),
        effects.len(),
        "killed after call {n}: {effects:#?}"
    );
}

/// Asserts that a hook killed after each of the eight calls of a handover (after the fake has
/// done what the call asks), and then run again from the step it was killed in, finishes the
/// update without making any change twice: the fakes change exactly as in a run nobody killed
/// (the guard loaded, the new unit started, the redirect moved, the old unit stopped, once each),
/// the record ends committed with no previous slot, and the public port is answered by the new
/// instance. It would catch a re-run that starts a second instance, swaps the redirect back and
/// forth, or stops the old unit before the commit is on record.
#[test]
fn a_hook_killed_after_any_call_finishes_when_run_again_and_repeats_nothing() {
    for n in 1..=CALLS {
        let rig = Rig::new(&format!("hook-conformance-killed-{n}"), Setup::handover());
        let (a, b) = (rig.a, rig.b);
        rig.kill_after(Some(n));
        let killed = run_until_killed(&rig, n);
        assert_eq!(rig.calls().len(), n, "killed after call {n}");
        rig.kill_after(None);
        for step in &STEPS[killed..] {
            let ran = rig.step(step);
            assert_eq!(ran.code, Some(0), "killed after call {n}, {step}: {ran:?}");
        }
        none_repeated(&rig, n);
        assert_eq!(
            kinds(&rig.effects()),
            [
                "set kbf_guard_tierstore",
                format!("start tierstore@{b}.service").as_str(),
                "set kbf_forward_tierstore",
                format!("stop tierstore@{a}.service").as_str(),
            ],
            "killed after call {n}"
        );
        assert_eq!(rig.running(), [b], "killed after call {n}");
        assert_eq!(rig.redirect(), Some(b), "killed after call {n}");
        assert!(rig.has_table(GUARD_TABLE), "killed after call {n}");
        let record = rig.record();
        assert_eq!((record.serving, record.previous), (b, None), "{n}");
        assert_eq!(
            record.markers,
            BTreeSet::from([Marker::Swapped, Marker::Verified, Marker::Stopping]),
            "killed after call {n}"
        );
        assert_eq!(rig.link(a), None, "killed after call {n}");
        public_answers(&rig, b, NEW_VERSION);
    }
}

/// Asserts that a hook killed after each call and then rolled back by the real binary ends with
/// the host it found, and makes no change twice: before the commit `rollback` restores the old
/// slot behind the redirect, stops the new one and removes what the update placed; the one thing
/// it leaves is a guard loaded by a `prepare` killed before it could record that (the design
/// states that case, and the guard only keeps other hosts off the private ports). After the commit
/// (killed after the old unit's stop) `rollback` refuses as irreversible and a `cleanup` run again
/// finishes. It would catch a rollback that cannot undo a step cut short, or one that undoes a
/// commit.
#[test]
fn a_hook_killed_after_any_call_is_rolled_back_to_the_host_it_found() {
    for n in 1..=CALLS {
        let rig = Rig::new(
            &format!("hook-conformance-killed-rollback-{n}"),
            Setup::handover(),
        );
        let (a, b) = (rig.a, rig.b);
        let before = rig.machine();
        rig.kill_after(Some(n));
        run_until_killed(&rig, n);
        rig.kill_after(None);
        let ran = rig.step("rollback");
        if rig.record().committed() {
            assert_eq!(ran.code, Some(1), "killed after call {n}: {ran:?}");
            assert_eq!(ran.line["reason"], "committed", "{n}");
            assert_eq!(ran.line["reversible"], false, "{n}");
            let ran = rig.step("cleanup");
            assert_eq!(ran.code, Some(0), "killed after call {n}: {ran:?}");
            assert_eq!(rig.running(), [b], "killed after call {n}");
            public_answers(&rig, b, NEW_VERSION);
        } else {
            assert_eq!(ran.code, Some(0), "killed after call {n}: {ran:?}");
            let mut after = rig.machine();
            if n == 2 {
                assert!(
                    after.tables.remove(GUARD_TABLE).is_some(),
                    "a guard loaded and not recorded stays"
                );
            }
            assert_eq!(after, before, "killed after call {n}");
            assert_eq!(rig.link(b), None, "killed after call {n}");
            let record = rig.record();
            assert_eq!((record.serving, record.previous), (a, None), "{n}");
            assert!(record.markers.is_empty(), "killed after call {n}");
        }
        none_repeated(&rig, n);
    }
}
