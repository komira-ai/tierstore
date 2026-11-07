//! The classification of registrations, and the checks of `--departed` and of the protocol.

use std::time::Duration;

use bytes::Bytes;

use super::super::testing::{register, register_old, register_reporting, tier};
use super::{
    Class, Registration, check_departed, check_protocol, holding_entry_drain, not_at_rest,
    not_drained, phase_word, read,
};
use crate::faulty::{FaultyBackend, Op};
use crate::format::FormatPhase;
use crate::{Backend, Error};
use std::sync::Arc;

const FRESH: Duration = Duration::from_secs(15);

fn entry(id: &str, class: Class) -> Registration {
    Registration {
        id: id.to_owned(),
        class,
        age_ms: 0,
        phase: None,
        writes_in_flight: None,
    }
}

/// Asserts how a body and an age classify: a protocol number and an age below the window is live,
/// at the window is silent, no protocol number (an empty object, text that is not JSON, JSON of
/// another shape) is unlabelled however fresh; and that a field the verbs do not read is ignored.
#[tokio::test]
async fn a_registration_is_live_silent_or_unlabelled() {
    let (hot, clock) = tier();
    register(&*hot, "live", 3).await;
    register_old(&*hot, "old").await;
    hot.put("_tiering/proxies/text", Bytes::from_static(b"hello"))
        .await
        .unwrap();
    hot.put("_tiering/proxies/other", Bytes::from_static(b"42"))
        .await
        .unwrap();
    clock.advance(14_000);
    register(&*hot, "recent", 2).await;
    clock.advance(1_000);
    let found = read(&*hot, clock.now(), FRESH).await.unwrap();
    let got: Vec<(&str, Class, u64)> = found
        .iter()
        .map(|found| (found.id.as_str(), found.class, found.age_ms))
        .collect();
    assert_eq!(
        got,
        vec![
            ("live", Class::Silent { proto: 3 }, 15_000),
            ("old", Class::Unlabelled, 15_000),
            ("other", Class::Unlabelled, 15_000),
            ("recent", Class::Live { proto: 2 }, 1_000),
            ("text", Class::Unlabelled, 15_000),
        ]
    );
}

/// Asserts that a time before the write (the clocks of the verb and the store disagree) is age 0,
/// not a wrap-around, and that a window too large for a count of milliseconds is a window with no
/// end.
#[tokio::test]
async fn an_age_before_the_write_is_zero_and_a_huge_window_does_not_wrap() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    let found = read(&*hot, clock.now() - 5_000, Duration::MAX)
        .await
        .unwrap();
    assert_eq!(
        (found[0].class, found[0].age_ms),
        (Class::Live { proto: 2 }, 0)
    );
}

/// Asserts that a registration deleted between the listing and its read is left out, and that a
/// failure of the store is returned.
#[tokio::test]
async fn a_registration_that_vanished_is_left_out_and_a_store_error_is_returned() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    faulty.fail_next_with(Op::Get, Error::NotFound("gone".to_owned()));
    assert_eq!(read(&*faulty, clock.now(), FRESH).await.unwrap(), vec![]);
    faulty.fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    let failed = read(&*faulty, clock.now(), FRESH).await;
    assert_eq!(failed, Err(Error::Unavailable("down".to_owned())));
    faulty.fail_next_with(Op::List, Error::Unavailable("listing".to_owned()));
    let failed = read(&*faulty, clock.now(), FRESH).await;
    assert_eq!(failed, Err(Error::Unavailable("listing".to_owned())));
}

/// Asserts the line of each class, with whole seconds.
#[test]
fn each_class_has_its_line() {
    let at = |class| Registration {
        id: "a".to_owned(),
        class,
        age_ms: 2_999,
        phase: None,
        writes_in_flight: None,
    };
    assert_eq!(
        at(Class::Live { proto: 2 }).describe(),
        "live, protocol 2, written 2s ago"
    );
    assert_eq!(
        at(Class::Silent { proto: 1 }).describe(),
        "silent, protocol 1, written 2s ago"
    );
    assert_eq!(
        at(Class::Unlabelled).describe(),
        "unlabelled, no heartbeat, written 2s ago"
    );
}

/// Asserts the refusals of the departed check (the messages exactly): an id with no registration
/// and one that is live come first; then the registrations that are not live and not named, listed
/// in order; and that naming every one passes.
#[test]
fn departed_must_name_every_registration_that_is_not_live_and_only_those() {
    let all = [
        entry("live", Class::Live { proto: 2 }),
        entry("old", Class::Unlabelled),
        entry("silent", Class::Silent { proto: 2 }),
    ];
    let said = |departed: &[&str]| {
        let departed: Vec<String> = departed.iter().map(|id| (*id).to_owned()).collect();
        check_departed(&all, &departed, "drain").map_err(|why| why.to_string())
    };
    assert_eq!(
        said(&["ghost"]),
        Err("--departed ghost: there is no registration of that id".to_owned())
    );
    assert_eq!(
        said(&["live"]),
        Err("--departed live: it has a fresh heartbeat, so it is running".to_owned())
    );
    assert_eq!(
        said(&["old"]),
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: silent"
                .to_owned()
        )
    );
    assert_eq!(
        said(&[]),
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: old, silent"
                .to_owned()
        )
    );
    assert_eq!(said(&["old", "silent"]), Ok(()));
}

/// Asserts the refusal of the protocol check (the message exactly), that it names only the live
/// registrations below the minimum, in order, and that none below passes.
#[test]
fn every_live_registration_must_be_at_the_protocol() {
    let all = [
        entry("a", Class::Live { proto: 1 }),
        entry("b", Class::Live { proto: 2 }),
        entry("c", Class::Silent { proto: 1 }),
        entry("d", Class::Live { proto: 0 }),
    ];
    assert_eq!(
        check_protocol(&all, 2).map_err(|why| why.to_string()),
        Err("refusing to activate: registrations below protocol 2: a, d".to_owned())
    );
    assert!(check_protocol(&all[1..3], 2).is_ok());
}

/// Asserts that the phase and the writes in flight a body reports are read, in the line for the
/// operator of a live and of a silent registration, and that a body that reports neither, and a
/// phase word this release does not know, read as they are: the phase `unknown`, no count.
#[tokio::test]
async fn the_phase_and_the_writes_in_flight_are_read_and_described() {
    let (hot, clock) = tier();
    register_reporting(&*hot, "a", 3, "active-marks", 4).await;
    register_reporting(&*hot, "b", 3, "someday", 0).await;
    register(&*hot, "c", 3).await;
    clock.advance(20_000);
    let found = read(&*hot, clock.now(), FRESH).await.unwrap();
    let got: Vec<(Option<FormatPhase>, Option<u64>)> = found
        .iter()
        .map(|found| (found.phase, found.writes_in_flight))
        .collect();
    assert_eq!(
        got,
        vec![
            (Some(FormatPhase::ActiveMarks), Some(4)),
            (Some(FormatPhase::Unknown), Some(0)),
            (None, None),
        ]
    );
    let lines: Vec<String> = found.iter().map(Registration::describe).collect();
    assert_eq!(
        lines,
        vec![
            "silent, protocol 3, phase active-marks, 4 writes in flight, written 20s ago",
            "silent, protocol 3, phase unknown, 0 writes in flight, written 20s ago",
            "silent, protocol 3, written 20s ago",
        ]
    );
    let live = Registration {
        class: Class::Live { proto: 3 },
        phase: Some(FormatPhase::Drained),
        writes_in_flight: Some(0),
        ..entry("d", Class::Unlabelled)
    };
    assert_eq!(
        live.describe(),
        "live, protocol 3, phase drained, 0 writes in flight, written 0s ago"
    );
    for (phase, word) in [
        (FormatPhase::Drain, "drain"),
        (FormatPhase::Drained, "drained"),
        (FormatPhase::Active, "active"),
        (FormatPhase::ActiveMarks, "active-marks"),
        (FormatPhase::Unknown, "unknown"),
    ] {
        assert_eq!(phase_word(phase), word);
    }
}

/// Asserts which registrations are not at rest under a pause: only a live one of protocol 3 or
/// later that does not report `drain` or `drained`, or reports no count or a count above zero;
/// one of protocol 2, a silent one and an unlabelled one are not judged.
#[test]
fn only_a_live_proxy_that_reads_markers_can_be_not_at_rest() {
    let reporting = |id: &str, class, phase, writes| Registration {
        class,
        phase,
        writes_in_flight: writes,
        ..entry(id, Class::Unlabelled)
    };
    let live3 = Class::Live { proto: 3 };
    let rows = vec![
        reporting("ok-drain", live3, Some(FormatPhase::Drain), Some(0)),
        reporting("ok-drained", live3, Some(FormatPhase::Drained), Some(0)),
        reporting("marking", live3, Some(FormatPhase::ActiveMarks), Some(0)),
        reporting("unsaid", live3, None, None),
        reporting("busy", live3, Some(FormatPhase::Drain), Some(3)),
        reporting("proto2", Class::Live { proto: 2 }, None, None),
        reporting("silent", Class::Silent { proto: 3 }, None, None),
        reporting("unlabelled", Class::Unlabelled, None, None),
    ];
    assert_eq!(
        not_at_rest(&rows),
        vec![
            "marking acts on active-marks",
            "unsaid reports no phase",
            "unsaid reports no count of writes in flight",
            "busy has 3 writes in flight",
        ]
    );
    assert_eq!(
        not_drained(&rows),
        vec![
            "ok-drain acts on drain",
            "marking acts on active-marks",
            "unsaid reports no phase",
            "busy acts on drain",
            "proto2 reports no phase",
        ]
    );
}

/// Asserts which registrations hold the leader's `drained` under the pause over the entry
/// protocol: every one that does not report `drain` or `drained` and no write in flight,
/// whatever its protocol (a live one of protocol 2 included), and one that is not live says
/// also that it has no fresh heartbeat; a silent one that reports a pause and no write holds
/// nothing.
#[test]
fn under_the_entry_pause_every_registration_that_may_write_holds_the_drain() {
    let reporting = |id: &str, class, phase, writes| Registration {
        class,
        phase,
        writes_in_flight: writes,
        ..entry(id, Class::Unlabelled)
    };
    let live2 = Class::Live { proto: 2 };
    let rows = vec![
        reporting("ok", live2, Some(FormatPhase::Drained), Some(0)),
        reporting(
            "silent-ok",
            Class::Silent { proto: 2 },
            Some(FormatPhase::Drain),
            Some(0),
        ),
        reporting("proto2", live2, Some(FormatPhase::Active), Some(1)),
        reporting("silent", Class::Silent { proto: 2 }, None, None),
        reporting("unlabelled", Class::Unlabelled, None, None),
    ];
    assert_eq!(
        holding_entry_drain(&rows),
        vec![
            "proto2 acts on active",
            "proto2 has 1 writes in flight",
            "silent reports no phase",
            "silent reports no count of writes in flight",
            "silent has no fresh heartbeat",
            "unlabelled reports no phase",
            "unlabelled reports no count of writes in flight",
            "unlabelled has no fresh heartbeat",
        ]
    );
}
