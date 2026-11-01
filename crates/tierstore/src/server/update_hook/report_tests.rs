//! The exit status and the last line.

use std::collections::BTreeSet;
use std::process::ExitCode;

use serde_json::Value;

use super::{
    Class, Done, MAX_REASON_LEN, Outcome, PortError, PortKind, Reason, Report, is_reason_token,
};

fn line(done: &Done) -> Value {
    serde_json::from_str(&done.report.to_line()).unwrap()
}

/// Asserts that every reason is a token the updater accepts and no two are the same. It would
/// catch a misspelt or too long reason, which the updater replaces with an empty one.
#[test]
fn every_reason_is_a_token_and_they_differ() {
    let mut seen = BTreeSet::new();
    for reason in Reason::ALL {
        assert!(is_reason_token(reason.as_str()), "{reason:?}");
        assert!(seen.insert(reason.as_str()), "{reason:?} twice");
    }
    assert!(!is_reason_token(""));
    assert!(!is_reason_token("Upper"));
    assert!(!is_reason_token("with space"));
    assert!(!is_reason_token(&"a".repeat(MAX_REASON_LEN + 1)));
    assert!(is_reason_token(&"a".repeat(MAX_REASON_LEN)));
}

/// Asserts whom each reason blames: the host and the hook's own inputs are the environment, a
/// new release that does not become ready or does not answer is the release. A deferral for the
/// host is `environment`, and a v-next that never becomes ready is `release`.
#[test]
fn the_class_follows_the_reason() {
    use Reason::*;
    for reason in [
        BadArgs,
        BadParams,
        Locked,
        StateInvalid,
        HostNotReady,
        NoSlot,
        NftFailed,
        UnitFailed,
        LinkFailed,
        StateIo,
        HostFailed,
    ] {
        assert_eq!(reason.class(), Class::Environment, "{reason:?}");
    }
    for reason in [
        NotImplemented,
        OutOfOrder,
        NotVerified,
        Committed,
        NotReady,
        UnexpectedMode,
        ProbeFailed,
        WrongBuild,
        RedirectWrong,
        PreviousDown,
    ] {
        assert_eq!(reason.class(), Class::Release, "{reason:?}");
    }
    assert_eq!(Done::fail(NotReady, "x").report.class, Class::Release);
    assert_eq!(
        Done::deferred(HostNotReady, "x", 5).report.class,
        Class::Environment
    );
    assert_eq!(Class::default(), Class::Release);
}

/// Asserts the fields of the last line of a success, a failure and a deferral, and that the
/// three that are always there are. It would catch a line the updater reads as `{}`.
#[test]
fn the_last_line_has_the_contract_fields() {
    let ok = line(&Done::ok("fine"));
    assert_eq!(ok["class"], "release");
    assert_eq!(ok["reversible"], true);
    assert_eq!(ok["promotion_pending"], false);
    assert_eq!(ok["detail"], "fine");
    assert!(ok.get("reason").is_none() && ok.get("retry_after_secs").is_none());
    let failed = line(&Done::fail(Reason::NftFailed, "nft said no"));
    assert_eq!(failed["class"], "environment");
    assert_eq!(failed["reason"], "nft_failed");
    let deferred = line(&Done::deferred(Reason::HostNotReady, "later", 300));
    assert_eq!(deferred["retry_after_secs"], 300);
    assert_eq!(deferred["class"], "environment");
    let mut report = Report {
        reversible: false,
        promotion_pending: true,
        ..Done::ok("x").report
    };
    report.detail = None;
    let value: Value = serde_json::from_str(&report.to_line()).unwrap();
    assert_eq!(value["reversible"], false);
    assert_eq!(value["promotion_pending"], true);
    assert!(value.get("detail").is_none());
}

/// Asserts the status of each outcome and that the line is written with a newline.
#[test]
fn the_outcomes_have_the_contract_statuses() {
    assert_eq!(Outcome::Success.code(), 0);
    assert_eq!(Outcome::Failure.code(), 1);
    assert_eq!(Outcome::Deferred.code(), 75);
    assert_eq!(
        format!("{:?}", ExitCode::from(Outcome::Deferred)),
        format!("{:?}", ExitCode::from(75))
    );
    let mut out = Vec::new();
    Done::ok("x").report.write_to(&mut out).unwrap();
    assert!(out.ends_with(b"}\n"));
}

/// Asserts that a failing port becomes the reason of its kind, and a busy lock `locked`. It
/// would catch a unit failure reported as an nft failure, which sends an operator to the wrong
/// tool.
#[test]
fn a_port_error_becomes_the_reason_of_its_port() {
    let want = [
        (PortKind::Nft, Reason::NftFailed),
        (PortKind::Units, Reason::UnitFailed),
        (PortKind::Slots, Reason::LinkFailed),
        (PortKind::State, Reason::StateIo),
        (PortKind::Host, Reason::HostFailed),
        (PortKind::Instances, Reason::HostFailed),
    ];
    for (kind, reason) in want {
        let done = Done::from(PortError::failed(kind, "boom"));
        assert_eq!(done.outcome, Outcome::Failure);
        assert_eq!(done.report.reason, Some(reason));
        assert!(done.report.detail.unwrap().ends_with(": boom"));
    }
    let busy = Done::from(PortError::Busy);
    assert_eq!(busy.report.reason, Some(Reason::Locked));
}
