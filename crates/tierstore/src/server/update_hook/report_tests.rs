//! The exit status and the last line.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{
    Class, Done, Invalid, MAX_REASON_LEN, Outcome, PortError, PortKind, Reason, RecordError,
    Report, is_reason_token,
};

fn line(done: &Done) -> Value {
    serde_json::from_str(&done.report.to_line()).unwrap()
}

/// Asserts that every reason is a token the hostkeeper accepts, that no two are the same, and
/// where the edges of a token are. It would catch a misspelt or too long reason, which hostkeeper
/// replaces with an empty one.
#[test]
fn every_reason_is_a_token_and_they_differ() {
    let mut seen = BTreeSet::new();
    for reason in Reason::ALL {
        assert!(is_reason_token(reason.as_str()), "{reason:?}");
        assert!(seen.insert(reason.as_str()), "{reason:?} twice");
    }
    assert_eq!(seen.len(), Reason::ALL.len());
    assert!(!is_reason_token(""));
    assert!(is_reason_token("a1_9"));
    assert!(is_reason_token("9"));
    assert!(is_reason_token("_"));
    assert!(!is_reason_token("a-b"));
    assert!(!is_reason_token("Upper"));
    assert!(!is_reason_token("with space"));
    assert!(!is_reason_token(&"a".repeat(MAX_REASON_LEN + 1)));
    assert!(is_reason_token(&"a".repeat(MAX_REASON_LEN)));
}

/// Asserts the token of each reason as the operator's page spells it.
#[test]
fn the_tokens_are_the_ones_the_contract_documents() {
    let tokens: Vec<&str> = Reason::ALL.iter().map(|reason| reason.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "bad_args",
            "bad_params",
            "locked",
            "state_invalid",
            "out_of_order",
            "not_verified",
            "committed",
            "not_confirmed",
            "host_not_ready",
            "draining",
            "not_ready",
            "probe_failed",
            "wrong_build",
            "handoff_not_configured",
            "config_missing",
            "service_manager_unknown",
            "no_previous_release",
            "unit_failed",
            "state_io",
            "host_failed",
        ]
    );
}

/// Asserts whom each reason blames: the host, the node's configuration and the hook's own inputs
/// are the environment; what the new release does or does not do is the release. The two lists
/// together are every reason, so a reason added without a class here fails the test.
#[test]
fn the_class_follows_the_reason() {
    use Reason::*;
    let environment = [
        BadArgs,
        BadParams,
        Locked,
        StateInvalid,
        HostNotReady,
        Draining,
        HandoffNotConfigured,
        ConfigMissing,
        UnitFailed,
        StateIo,
        HostFailed,
    ];
    let release = [
        OutOfOrder,
        NotVerified,
        Committed,
        NotConfirmed,
        NotReady,
        ProbeFailed,
        WrongBuild,
        ServiceManagerUnknown,
        NoPreviousRelease,
    ];
    for reason in environment {
        assert_eq!(reason.class(), Class::Environment, "{reason:?}");
    }
    for reason in release {
        assert_eq!(reason.class(), Class::Release, "{reason:?}");
    }
    assert_eq!(environment.len() + release.len(), Reason::ALL.len());
    assert_eq!(Done::fail(NotReady, "x").report.class, Class::Release);
    assert_eq!(
        Done::deferred(HostNotReady, "x", 5).report.class,
        Class::Environment
    );
    assert_eq!(Class::default(), Class::Release);
    assert_eq!(Class::Release.as_str(), "release");
    assert_eq!(Class::Environment.as_str(), "environment");
}

/// Asserts the fields of the last line of a success, a failure and a deferral: `class`,
/// `reversible` and `promotion_pending` always, `reason`, `retry_after_secs` and `detail` when
/// set. The line is one object, no newline. It would catch a line hostkeeper reads as `{}`.
#[test]
fn the_last_line_has_the_contract_fields() {
    let ok = Done::ok("fine");
    assert_eq!(
        line(&ok),
        json!({
            "class": "release",
            "reversible": true,
            "promotion_pending": false,
            "detail": "fine",
        })
    );
    assert!(!ok.report.to_line().contains('\n'));
    let failed = Done::fail(Reason::HandoffNotConfigured, "no socket");
    assert_eq!(
        line(&failed),
        json!({
            "class": "environment",
            "reason": "handoff_not_configured",
            "reversible": true,
            "promotion_pending": false,
            "detail": "no socket",
        })
    );
    let deferred = Done::deferred(Reason::HostNotReady, "later", 30);
    assert_eq!(deferred.outcome, Outcome::Deferred);
    assert_eq!(
        line(&deferred),
        json!({
            "class": "environment",
            "reason": "host_not_ready",
            "retry_after_secs": 30,
            "reversible": true,
            "promotion_pending": false,
            "detail": "later",
        })
    );
    let mut report = Report {
        reversible: false,
        promotion_pending: true,
        ..Done::ok("x").report
    };
    report.detail = None;
    assert_eq!(
        serde_json::from_str::<Value>(&report.to_line()).unwrap(),
        json!({"class": "release", "reversible": false, "promotion_pending": true})
    );
}

/// Asserts the status of each outcome, that `ok`, `fail` and `deferred` give the outcome they are
/// named for, and that the line is written with a newline.
#[test]
fn the_outcomes_have_the_contract_statuses() {
    assert_eq!(Outcome::Success.code(), 0);
    assert_eq!(Outcome::Failure.code(), 1);
    assert_eq!(Outcome::Deferred.code(), 75);
    assert_eq!(Done::ok("x").outcome, Outcome::Success);
    assert_eq!(Done::fail(Reason::Locked, "x").outcome, Outcome::Failure);
    assert_eq!(
        Done::deferred(Reason::Draining, "x", 10).outcome,
        Outcome::Deferred
    );
    assert_eq!(
        format!("{:?}", ExitCode::from(Outcome::Deferred)),
        format!("{:?}", ExitCode::from(75))
    );
    let mut out = Vec::new();
    Done::ok("x").report.write_to(&mut out).unwrap();
    assert!(out.starts_with(b"{") && out.ends_with(b"}\n"));
}

/// Asserts that a failed write of the line is returned to the caller.
#[test]
fn a_write_that_fails_is_an_error() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let err = Done::ok("x").report.write_to(&mut Closed).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
}

/// Asserts that a failing port becomes the reason of its kind and carries the port's words, and a
/// busy lock `locked`. It would catch a unit failure reported as a state failure, which sends an
/// operator to the wrong tool.
#[test]
fn a_port_error_becomes_the_reason_of_its_port() {
    let want = [
        (PortKind::Units, Reason::UnitFailed),
        (PortKind::Launchd, Reason::UnitFailed),
        (PortKind::Markers, Reason::StateIo),
        (PortKind::State, Reason::StateIo),
        (PortKind::Host, Reason::HostFailed),
        (PortKind::Instances, Reason::HostFailed),
    ];
    for (kind, reason) in want {
        let done = Done::from(PortError::failed(kind, "boom"));
        assert_eq!(done.outcome, Outcome::Failure);
        assert_eq!(done.report.reason, Some(reason), "{kind}");
        assert_eq!(done.report.detail, Some(format!("{kind}: boom")));
    }
    let busy = Done::from(PortError::Busy);
    assert_eq!(busy.outcome, Outcome::Failure);
    assert_eq!(busy.report.reason, Some(Reason::Locked));
    assert_eq!(busy.report.class, Class::Environment);
}

/// Asserts that a record that cannot be read or breaks a rule is `state_invalid` with the words of
/// the error, and that every kind of port names itself.
#[test]
fn a_record_that_is_not_trusted_is_state_invalid() {
    let unreadable = Done::from(RecordError::Newer { found: 2 });
    assert_eq!(unreadable.outcome, Outcome::Failure);
    assert_eq!(unreadable.report.reason, Some(Reason::StateInvalid));
    assert_eq!(
        unreadable.report.detail.as_deref(),
        Some("the record has schema 2, newer than 1")
    );
    let broken = Done::from(Invalid::Same);
    assert_eq!(broken.report.reason, Some(Reason::StateInvalid));
    assert_eq!(
        broken.report.detail.as_deref(),
        Some("live and next are the same instance")
    );
    let names = [
        (PortKind::Units, "units"),
        (PortKind::Markers, "markers"),
        (PortKind::Launchd, "launchd"),
        (PortKind::Instances, "instances"),
        (PortKind::Host, "host"),
        (PortKind::State, "state"),
    ];
    for (kind, name) in names {
        assert_eq!(kind.to_string(), name);
    }
}
