//! Every step, halted at each of its calls in turn (the host stops there: that call and every
//! later one fail without effect), and then run again with the steps that follow it: the host
//! ends in the state of a run that was never interrupted, and the steps end as they ended then.

use crate::server::update_hook::{Outcome, Params, Step};

use super::fake::{CONFIG, NEW, OLD, Snapshot, SystemdFake, assert_ok, params, params_with};

use Step::{Cleanup, Cutover, Prepare, Restore, Rollback, Verify};

/// Which update the world is set up for.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// An update from a to b with a hand-over.
    Handover,
    /// An update from a to b with a stop of a first.
    Restart,
    /// A first install on a host with nothing.
    FirstInstall,
}

struct Scenario {
    name: &'static str,
    /// Steps that ran before.
    setup: &'static [Step],
    /// Whether the host rebooted after them.
    reboot: bool,
    /// The step that is halted.
    act: Step,
    /// What runs after it.
    rest: &'static [Step],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "prepare",
        setup: &[],
        reboot: false,
        act: Prepare,
        rest: &[Cutover, Verify, Cleanup],
    },
    Scenario {
        name: "cutover",
        setup: &[Prepare],
        reboot: false,
        act: Cutover,
        rest: &[Verify, Cleanup],
    },
    Scenario {
        name: "verify",
        setup: &[Prepare, Cutover],
        reboot: false,
        act: Verify,
        rest: &[Cleanup],
    },
    Scenario {
        name: "cleanup",
        setup: &[Prepare, Cutover, Verify],
        reboot: false,
        act: Cleanup,
        rest: &[],
    },
    Scenario {
        name: "rollback after prepare",
        setup: &[Prepare],
        reboot: false,
        act: Rollback,
        rest: &[],
    },
    Scenario {
        name: "rollback after cutover",
        setup: &[Prepare, Cutover],
        reboot: false,
        act: Rollback,
        rest: &[],
    },
    Scenario {
        name: "rollback after verify",
        setup: &[Prepare, Cutover, Verify],
        reboot: false,
        act: Rollback,
        rest: &[],
    },
    Scenario {
        name: "prepare after the commit",
        setup: &[Prepare, Cutover, Verify, Cleanup],
        reboot: false,
        act: Prepare,
        rest: &[],
    },
    Scenario {
        name: "restore after cutover",
        setup: &[Prepare, Cutover],
        reboot: true,
        act: Restore,
        rest: &[],
    },
    Scenario {
        name: "restore after the commit",
        setup: &[Prepare, Cutover, Verify, Cleanup],
        reboot: true,
        act: Restore,
        rest: &[],
    },
];

/// The most calls any step makes here; a bound for the loop.
const MOST_CALLS: usize = 200;

fn world(kind: Kind) -> (SystemdFake, Params) {
    match kind {
        Kind::Handover => (SystemdFake::installed(), params(NEW, Some(OLD))),
        Kind::Restart => (
            SystemdFake::installed(),
            params_with(
                NEW,
                Some(OLD),
                &format!("config_file = \"{CONFIG}\"\nmode = \"restart\"\n"),
            ),
        ),
        Kind::FirstInstall => (SystemdFake::empty(), params(NEW, None)),
    }
}

/// The world after the setup of `scenario`.
fn set_up(kind: Kind, scenario: &Scenario) -> (SystemdFake, Params) {
    let (fake, p) = world(kind);
    for step in scenario.setup {
        assert_ok(&fake.run_as_hostkeeper(*step, &p));
    }
    if scenario.reboot {
        fake.reboot();
    }
    (fake, p)
}

/// Runs the halted step again and what follows it: how each ended.
fn finish(fake: &SystemdFake, p: &Params, scenario: &Scenario) -> Vec<Outcome> {
    [scenario.act]
        .iter()
        .chain(scenario.rest)
        .map(|step| fake.run_as_hostkeeper(*step, p).outcome)
        .collect()
}

/// Asserts that, for an update with a hand-over, one with a restart and a first install, a step
/// halted at any of its calls and run again with the steps that follow ends where an
/// uninterrupted run ends: the same record, units and markers, and the same outcomes. Catches a
/// step whose second run depends on what its first left half done.
#[test]
fn every_step_run_again_after_a_halt_at_any_call_converges() {
    for kind in [Kind::Handover, Kind::Restart, Kind::FirstInstall] {
        for scenario in SCENARIOS {
            let (fake, p) = set_up(kind, scenario);
            let expected_outcomes = finish(&fake, &p, scenario);
            let expected: Snapshot = fake.snapshot();

            let mut halted = 0;
            for n in 1..=MOST_CALLS {
                let (fake, p) = set_up(kind, scenario);
                let before = fake.calls();
                fake.halt_at(Some(n));
                let _ = fake.run_as_hostkeeper(scenario.act, &p);
                fake.halt_at(None);
                if fake.calls() - before < n {
                    break;
                }
                halted += 1;
                let outcomes = finish(&fake, &p, scenario);
                let name = scenario.name;
                assert_eq!(
                    fake.snapshot(),
                    expected,
                    "{kind:?} {name} halted at call {n}"
                );
                assert_eq!(
                    outcomes, expected_outcomes,
                    "{kind:?} {name} halted at call {n}"
                );
            }
            let name = scenario.name;
            assert!(
                halted >= 3,
                "{kind:?} {name}: only {halted} calls were halted"
            );
            assert!(halted < MOST_CALLS, "{kind:?} {name}: the loop did not end");
        }
    }
}
