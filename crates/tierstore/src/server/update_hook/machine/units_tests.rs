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
            Call::new(
                "/usr/bin/systemctl",
                &["start", "--", "tierstore@9302.service"],
                START,
            ),
            Call::new(
                "/usr/bin/systemctl",
                &["stop", "--", "tierstore@9301.service"],
                STOP,
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
        "/usr/bin/systemctl start -- tierstore@9302.service: exited 1: Job for tierstore@9302.service failed."
    );
    let text = message(units(&runner).stop("x").unwrap_err());
    assert_eq!(
        text,
        "/usr/bin/systemctl stop -- x: exited 5: Unit x not loaded."
    );
    let text = message(units(&runner).stop("x").unwrap_err());
    assert_eq!(text, "/usr/bin/systemctl stop -- x: ended by a signal: ");
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
    assert_eq!(
        text,
        "/usr/bin/systemctl stop -- u: no end after 22s, killed"
    );
}

/// Asserts that a unit name outside the alphabet (empty, a leading dash, a space, a semicolon, a
/// slash, a line break, longer than 255 characters) is refused by start and by stop before any
/// program runs.
#[test]
fn a_unit_name_outside_the_alphabet_is_refused_before_any_call() {
    let runner = FakeRunner::replying(vec![]);
    let long = "u".repeat(256);
    for name in [
        "",
        "-x",
        "--now",
        "a b",
        "a;b",
        "../x",
        "x\n",
        long.as_str(),
    ] {
        let text = message(units(&runner).start(name).unwrap_err());
        assert!(text.ends_with("is not a unit name"), "{text}");
        assert!(units(&runner).stop(name).is_err());
    }
    assert!(runner.calls().is_empty());
}

/// Asserts that the names of the slot units, a template instance and a name of the longest
/// length are passed on.
#[test]
fn a_unit_name_inside_the_alphabet_is_passed_on() {
    let runner = FakeRunner::replying(vec![exit(0, "", ""), exit(0, "", "")]);
    assert!(units(&runner).start("tierstore@9301.service").is_ok());
    assert!(units(&runner).stop(&"u".repeat(255)).is_ok());
}

/// Asserts that the program can be named by the caller.
#[test]
fn the_program_can_be_named() {
    let runner = FakeRunner::replying(vec![exit(0, "", "")]);
    units(&runner)
        .with_program("/opt/systemctl")
        .start("u")
        .unwrap();
    assert_eq!(runner.calls()[0].program, "/opt/systemctl");
}
