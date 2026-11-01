//! `systemctl` through a recording runner.

use std::time::Duration;

use super::super::ports::{PortError, PortKind, Units};
use super::testing::{FakeRunner, exit, signalled};
use super::{Call, RunError, SystemdUnits};

const START: Duration = Duration::from_secs(11);
const STOP: Duration = Duration::from_secs(22);

fn units(runner: &FakeRunner) -> SystemdUnits<'_> {
    SystemdUnits::new(runner, START, STOP)
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

/// Asserts the argument vectors, and that a start is given the start time limit and a stop the
/// stop's (the drain makes it long). It would catch a stop killed at the start's limit.
#[test]
fn start_and_stop_run_systemctl_with_their_own_time_limits() {
    let runner = FakeRunner::replying(vec![exit(0, "", ""), exit(0, "", "")]);
    units(&runner).start("tierstore@9302.service").unwrap();
    units(&runner).stop("tierstore@9301.service").unwrap();
    assert_eq!(
        runner.calls(),
        vec![
            Call::new("systemctl", &["start", "tierstore@9302.service"], START),
            Call::new("systemctl", &["stop", "tierstore@9301.service"], STOP),
        ]
    );
}

/// Asserts that a start that exits 0 is Ok whatever the unit did with its condition: a unit
/// whose `ConditionPathExists=` fails is skipped by systemd and `systemctl start` exits 0, so
/// the port reports no error and the step finds the unit inactive by looking.
#[test]
fn a_start_that_exits_zero_is_ok_even_if_the_condition_skipped_the_unit() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    assert!(units(&runner).start("tierstore@9301.service").is_ok());
}

/// Asserts that a non-zero exit of a start and of a stop are failures of the units port that
/// carry the command and the program's words, and so is a signal.
#[test]
fn a_non_zero_exit_is_a_failure() {
    let runner = FakeRunner::replying(vec![
        exit(1, "", "Job for tierstore@9302.service failed.\n"),
        exit(5, "", "Unit x not loaded.\n"),
        signalled(),
    ]);
    let text = message(units(&runner).start("tierstore@9302.service").unwrap_err());
    assert_eq!(
        text,
        "systemctl start tierstore@9302.service: exited 1: Job for tierstore@9302.service failed."
    );
    let text = message(units(&runner).stop("x").unwrap_err());
    assert_eq!(text, "systemctl stop x: exited 5: Unit x not loaded.");
    let text = message(units(&runner).stop("x").unwrap_err());
    assert_eq!(text, "systemctl stop x: ended by a signal: ");
}

/// Asserts that a timeout is a failure of the start and of the stop.
#[test]
fn a_timeout_is_a_failure() {
    let runner = FakeRunner::replying(vec![
        Err(RunError::Timeout(START)),
        Err(RunError::Timeout(STOP)),
    ]);
    assert!(units(&runner).start("u").is_err());
    let text = message(units(&runner).stop("u").unwrap_err());
    assert_eq!(text, "systemctl stop u: no end after 22s, killed");
}
