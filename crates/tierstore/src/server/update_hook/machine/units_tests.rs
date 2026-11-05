//! `systemctl` through a recording runner.

use std::time::Duration;

use super::super::ports::{Instance, PortError, PortKind, UnitState, Units};
use super::testing::{FakeRunner, exit, signalled};
use super::{Call, RunError, SystemdUnits};

const START: Duration = Duration::from_secs(11);
const STOP: Duration = Duration::from_secs(22);
const SHOW: Duration = Duration::from_secs(33);

fn units(runner: &FakeRunner) -> SystemdUnits<'_> {
    SystemdUnits::new(runner, START, STOP, SHOW)
}

fn message(error: PortError) -> String {
    match error {
        PortError::Failed {
            kind: PortKind::Units,
            message,
        } => message,
        other => panic!("not a units failure: {other:?}"),
    }
}

/// Asserts the argument vectors, and that a start is given the start time limit, a stop the
/// stop's (the drain makes it long) and a look at the state the short one of `show`.
#[test]
fn start_stop_and_state_run_systemctl_with_their_own_time_limits() {
    let runner = FakeRunner::replying(vec![
        exit(0, "", ""),
        exit(0, "", ""),
        exit(0, "active\n", ""),
    ]);
    units(&runner).start(Instance::B).unwrap();
    units(&runner).stop(Instance::A).unwrap();
    units(&runner).state(Instance::A).unwrap();
    assert_eq!(
        runner.calls(),
        vec![
            Call::new(
                "/usr/bin/systemctl",
                &["start", "--", "tierstore@b.service"],
                START
            ),
            Call::new(
                "/usr/bin/systemctl",
                &["stop", "--", "tierstore@a.service"],
                STOP
            ),
            Call::new(
                "/usr/bin/systemctl",
                &[
                    "show",
                    "--property=ActiveState",
                    "--value",
                    "--",
                    "tierstore@a.service"
                ],
                SHOW
            ),
        ]
    );
}

/// Asserts that a start that exits 0 is Ok whatever the unit did with its condition: a unit
/// whose `ConditionPathExists=` fails is skipped by systemd and `systemctl start` exits 0, so
/// the port reports no error and the step finds the unit inactive by looking.
#[test]
fn a_start_that_exits_zero_is_ok_even_if_the_condition_skipped_the_unit() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    assert!(units(&runner).start(Instance::A).is_ok());
}

/// Asserts that a non-zero exit of a start and of a stop are failures of the units port that
/// carry the command and the program's words, and so is a signal.
#[test]
fn a_non_zero_exit_is_a_failure() {
    let runner = FakeRunner::replying(vec![
        exit(1, "", "Job for tierstore@b.service failed.\n"),
        exit(5, "", "Unit x not loaded.\n"),
        signalled(),
    ]);
    let text = message(units(&runner).start(Instance::B).unwrap_err());
    assert_eq!(
        text,
        "/usr/bin/systemctl start -- tierstore@b.service: exited 1: Job for tierstore@b.service failed."
    );
    let text = message(units(&runner).stop(Instance::A).unwrap_err());
    assert_eq!(
        text,
        "/usr/bin/systemctl stop -- tierstore@a.service: exited 5: Unit x not loaded."
    );
    let text = message(units(&runner).state(Instance::A).unwrap_err());
    assert_eq!(
        text,
        "/usr/bin/systemctl show --property=ActiveState --value -- tierstore@a.service: ended by a signal: "
    );
}

/// Asserts that a timeout is a failure of the start and of the stop.
#[test]
fn a_timeout_is_a_failure() {
    let runner = FakeRunner::replying(vec![
        Err(RunError::Timeout(START)),
        Err(RunError::Timeout(STOP)),
    ]);
    assert!(units(&runner).start(Instance::A).is_err());
    let text = message(units(&runner).stop(Instance::A).unwrap_err());
    assert_eq!(
        text,
        "/usr/bin/systemctl stop -- tierstore@a.service: no end after 22s, killed"
    );
}

/// Asserts that every `ActiveState` word systemd has is read as its state: `active` and
/// `reloading` as active, and the rest as themselves, with the line break trimmed.
#[test]
fn every_active_state_is_read() {
    let words = [
        ("active\n", UnitState::Active),
        ("reloading\n", UnitState::Active),
        ("activating\n", UnitState::Activating),
        ("deactivating\n", UnitState::Deactivating),
        ("inactive\n", UnitState::Inactive),
        ("failed\n", UnitState::Failed),
    ];
    let runner = FakeRunner::replying(words.iter().map(|(text, _)| exit(0, text, "")).collect());
    for (_, want) in words {
        assert_eq!(units(&runner).state(Instance::B).unwrap(), want);
    }
}

/// Asserts that a word that is no state, and an empty answer, are failures that name the word
/// and not a guess.
#[test]
fn an_unknown_active_state_is_a_failure() {
    let runner = FakeRunner::replying(vec![exit(0, "maintenance\n", ""), exit(0, "", "")]);
    let text = message(units(&runner).state(Instance::A).unwrap_err());
    assert!(
        text.ends_with("unknown ActiveState \"maintenance\""),
        "{text}"
    );
    let text = message(units(&runner).state(Instance::A).unwrap_err());
    assert!(text.ends_with("unknown ActiveState \"\""), "{text}");
}

/// Asserts that the program can be named by the caller.
#[test]
fn the_program_can_be_named() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    units(&runner)
        .with_program("/opt/systemctl")
        .start(Instance::A)
        .unwrap();
    assert_eq!(runner.calls()[0].program, "/opt/systemctl");
}
