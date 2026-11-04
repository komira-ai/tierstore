//! `tierstore format-epoch`: what each action reads, refuses and writes, over a memory tier whose
//! clock and sleeper the test holds, and the verb from its arguments to its exit status.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use tierstore_testkit::dir::TestDir;

use super::super::testing::{
    args, put_entry, register, register_old, set_format, status, stored_format, tier,
};
use super::{Cli, Work, act, run};
use crate::entry::{EntryOp, EntryState};
use crate::faulty::{FaultyBackend, Op};
use crate::format::{Format, FormatPhase};
use crate::journal::FORMAT_KEY;
use crate::{Backend, DirBackend, Error, RecordingSleeper, Sleeper, TestClock};
use clap::Parser;

fn cli(flags: &[&str]) -> Cli {
    let base = ["tierstore format-epoch", "--hot", "memory"];
    Cli::try_parse_from(base.into_iter().chain(flags.iter().copied())).unwrap()
}

/// What the action printed, and what it returned (the message of an error).
async fn acted(
    hot: &dyn Backend,
    clock: &TestClock,
    sleeper: &dyn Sleeper,
    flags: &[&str],
) -> (Result<(), String>, String) {
    let cli = cli(flags);
    let mut out = Vec::new();
    let work = Work {
        cli: &cli,
        hot,
        clock,
        sleeper,
    };
    let result = act(work, &mut out).await.map_err(|why| why.to_string());
    (result, String::from_utf8(out).unwrap())
}

fn drain_format() -> Format {
    Format {
        phase: FormatPhase::Drain,
        epoch: None,
    }
}

fn drained_format() -> Format {
    Format {
        phase: FormatPhase::Drained,
        epoch: None,
    }
}

fn active_format(epoch: u64) -> Format {
    Format {
        phase: FormatPhase::Active,
        epoch: Some(epoch),
    }
}

/// A sleeper that does what the reconciler or another operator would do while the verb waits: on
/// its `on_call`th call it replaces the format object (or deletes it), and it writes down the
/// format it found at each call and the wait it was asked for.
struct Settles {
    hot: Arc<dyn Backend>,
    on_call: usize,
    then: Option<Format>,
    seen: Mutex<Vec<Option<Format>>>,
    asked: Mutex<Vec<Duration>>,
}

impl Settles {
    fn new(hot: &Arc<dyn Backend>, on_call: usize, then: Option<Format>) -> Self {
        Self {
            hot: Arc::clone(hot),
            on_call,
            then,
            seen: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Sleeper for Settles {
    async fn sleep(&self, duration: Duration) {
        self.asked.lock().unwrap().push(duration);
        let now = stored_format(self.hot.as_ref()).await;
        let calls = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(now);
            seen.len()
        };
        if calls != self.on_call {
            return;
        }
        match &self.then {
            Some(format) => {
                let tag = self.hot.head(FORMAT_KEY).await.unwrap().tag;
                let body = crate::format::format_body(format);
                self.hot.put_if_match(FORMAT_KEY, body, &tag).await.unwrap();
            }
            None => self.hot.delete(FORMAT_KEY).await.unwrap(),
        }
    }
}

/// Asserts the report of `--check`: the phase, each registration with how it is classified, the
/// number of open entries, and that `--drain` would be accepted when every registration is live.
#[tokio::test]
async fn check_reports_the_state_and_accepts_when_every_registration_is_live() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    put_entry(&*hot, "k", EntryOp::Put, EntryState::Held, Some("a")).await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--check"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "format: absent\n\
         registrations: 1\n  a: live, protocol 2, written 0s ago\n\
         open entries: 1\n\
         --drain would be accepted\n"
    );
}

/// Asserts that the epoch of the format is printed with its phase, and that a check of a store
/// that is already active writes nothing and is accepted.
#[tokio::test]
async fn check_prints_the_epoch_of_an_active_format() {
    let (hot, clock) = tier();
    set_format(&*hot, &active_format(2)).await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--check"]).await;
    assert_eq!(result, Ok(()));
    assert!(
        out.starts_with("format: active (epoch 2)\nregistrations: 0\n"),
        "{out}"
    );
}

/// Asserts the refusal of `--check` (the message exactly) when a registration of a release before
/// the heartbeat is not named, with the report printed before it; and that naming it with
/// `--departed` makes the check pass.
#[tokio::test]
async fn check_refuses_an_unlabelled_registration_until_it_is_named_departed() {
    let (hot, clock) = tier();
    register_old(&*hot, "old").await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--check"]).await;
    assert_eq!(
        result,
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: old"
                .to_owned()
        )
    );
    assert!(
        out.contains("  old: unlabelled, no heartbeat, written 0s ago\n"),
        "{out}"
    );
    let (named, _) = acted(&*hot, &clock, &sleeper, &["--check", "--departed", "old"]).await;
    assert_eq!(named, Ok(()));
}

/// Asserts the edge of freshness: a heartbeat is live up to one millisecond before three beats
/// (of 5 s) and silent from three beats; and that `--beat` and `--beats` move the edge.
#[tokio::test]
async fn a_heartbeat_is_live_until_three_beats_and_silent_after() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    let sleeper = RecordingSleeper::new();
    clock.advance(14_999);
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--check"]).await;
    assert_eq!(result, Ok(()));
    assert!(
        out.contains("  a: live, protocol 2, written 14s ago\n"),
        "{out}"
    );
    clock.advance(1);
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--check"]).await;
    assert_eq!(
        result,
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: a"
                .to_owned()
        )
    );
    assert!(
        out.contains("  a: silent, protocol 2, written 15s ago\n"),
        "{out}"
    );
    let wider = ["--check", "--beat", "10s", "--beats", "2"];
    let (result, _) = acted(&*hot, &clock, &sleeper, &wider).await;
    assert_eq!(result, Ok(()));
}

/// Asserts the refusals about `--departed` (the messages exactly): an id with no registration, and
/// an id whose registration has a fresh heartbeat, which is running whatever the operator thinks.
#[tokio::test]
async fn departed_names_only_registrations_that_are_not_running() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    let sleeper = RecordingSleeper::new();
    let (ghost, _) = acted(&*hot, &clock, &sleeper, &["--check", "--departed", "ghost"]).await;
    assert_eq!(
        ghost,
        Err("--departed ghost: there is no registration of that id".to_owned())
    );
    let (live, _) = acted(&*hot, &clock, &sleeper, &["--check", "--departed", "a"]).await;
    assert_eq!(
        live,
        Err("--departed a: it has a fresh heartbeat, so it is running".to_owned())
    );
}

/// Asserts that a phase this release does not know is refused by `--check` and `--drain` with the
/// message exactly, and that nothing is written.
#[tokio::test]
async fn a_phase_this_release_does_not_know_is_refused() {
    let (hot, clock) = tier();
    hot.put(FORMAT_KEY, Bytes::from_static(br#"{"phase":"someday"}"#))
        .await
        .unwrap();
    let sleeper = RecordingSleeper::new();
    let message = "the format object has a phase this release does not know; use the tool of the \
                   release that wrote it";
    for flags in [&["--check"][..], &["--drain", "--apply"][..]] {
        let (result, _) = acted(&*hot, &clock, &sleeper, flags).await;
        assert_eq!(result, Err(message.to_owned()), "{flags:?}");
    }
    assert!(sleeper.asked().is_empty());
}

/// Asserts that `--drain` without `--apply` writes nothing and says so.
#[tokio::test]
async fn drain_without_apply_writes_nothing() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--drain"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "dry run: would write the format {\"phase\":\"drain\"}; nothing was written; add --apply \
         to write it\n"
    );
    assert_eq!(stored_format(&*hot).await, None);
    assert!(sleeper.asked().is_empty());
}

/// Asserts that `--drain --apply` writes `drain`, then reads the format every `--poll` and prints
/// what it waits for until the reconciler writes `drained`, and returns.
#[tokio::test]
async fn drain_writes_the_phase_and_waits_until_it_says_drained() {
    let (hot, clock) = tier();
    register(&*hot, "a", 2).await;
    let tier: Arc<dyn Backend> = hot.clone();
    let sleeper = Settles::new(&tier, 2, Some(drained_format()));
    let (result, out) = acted(
        &*hot,
        &clock,
        &sleeper,
        &["--drain", "--apply", "--poll", "7s"],
    )
    .await;
    assert_eq!(result, Ok(()));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(lines[0], "wrote the format: drain");
    assert!(
        lines[1].starts_with("waiting for the format to say drained: "),
        "{out}"
    );
    assert!(lines[1].ends_with("--cancel to resume writes"), "{out}");
    let waiting = "still waiting: the format says drain; 1 registrations";
    assert_eq!((lines[2], lines[3]), (waiting, waiting));
    assert_eq!(
        lines[4],
        "drained: writes are paused and every journal is applied"
    );
    assert_eq!(
        *sleeper.asked.lock().unwrap(),
        vec![Duration::from_secs(7); 2]
    );
    assert_eq!(
        *sleeper.seen.lock().unwrap(),
        vec![Some(drain_format()), Some(drain_format())]
    );
    assert_eq!(stored_format(&*hot).await, Some(drained_format()));
}

/// Asserts that a drain over an active format keeps its epoch, so that `--cancel` of a rollback
/// drain can put the active format back.
#[tokio::test]
async fn drain_over_an_active_format_keeps_the_epoch() {
    let (hot, clock) = tier();
    set_format(&*hot, &active_format(2)).await;
    let tier: Arc<dyn Backend> = hot.clone();
    let sleeper = Settles::new(&tier, 1, Some(drained_format()));
    let (result, _) = acted(&*hot, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(result, Ok(()));
    let drain_over_epoch = Format {
        phase: FormatPhase::Drain,
        epoch: Some(2),
    };
    assert_eq!(*sleeper.seen.lock().unwrap(), vec![Some(drain_over_epoch)]);
}

/// Asserts that running `--drain` again while the format says `drain` writes nothing and goes on
/// waiting, which is how an interrupted drain is resumed, even without `--apply`.
#[tokio::test]
async fn drain_resumes_waiting_when_the_format_already_says_drain() {
    let (hot, clock) = tier();
    set_format(&*hot, &drain_format()).await;
    let tier: Arc<dyn Backend> = hot.clone();
    let sleeper = Settles::new(&tier, 1, Some(drained_format()));
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--drain"]).await;
    assert_eq!(result, Ok(()));
    assert!(
        out.starts_with("the format already says drain\nwaiting for the format"),
        "{out}"
    );
    assert_eq!(*sleeper.seen.lock().unwrap(), vec![Some(drain_format())]);
}

/// Asserts that `--drain` of a drained format says so, waits for nothing and writes nothing.
#[tokio::test]
async fn drain_of_a_drained_format_is_done() {
    let (hot, clock) = tier();
    set_format(&*hot, &drained_format()).await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "the format is already drained: writes are paused and every journal is applied\n"
    );
    assert!(sleeper.asked().is_empty());
}

/// Asserts that `--drain --apply` refuses, before it writes anything, while a registration that
/// is not running is not named (the message exactly).
#[tokio::test]
async fn drain_refuses_an_unnamed_registration_and_writes_nothing() {
    let (hot, clock) = tier();
    register_old(&*hot, "old").await;
    register_old(&*hot, "older").await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(
        result,
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: old, older"
                .to_owned()
        )
    );
    assert_eq!(out, "");
    assert_eq!(stored_format(&*hot).await, None);
}

/// Asserts that a drain that finds the format object created, or replaced, between its read and
/// its write is refused with the message exactly, and that another failure of the store is the
/// store's, named as the hot tier.
#[tokio::test]
async fn drain_refuses_when_the_format_changed_under_it() {
    let (hot, clock) = tier();
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    let sleeper = RecordingSleeper::new();
    let changed = "the format object changed while it was being written; run --check and try again";
    faulty.fail_next_with(Op::Put, Error::AlreadyExists(FORMAT_KEY.to_owned()));
    let (created, _) = acted(&*faulty, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(created, Err(changed.to_owned()));
    set_format(&*hot, &active_format(2)).await;
    let precondition = Error::PreconditionFailed(FORMAT_KEY.to_owned());
    faulty.fail_next_with(Op::PutIfMatch, precondition);
    let (replaced, _) = acted(&*faulty, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(replaced, Err(changed.to_owned()));
    faulty.fail_next_with(Op::PutIfMatch, Error::Unavailable("down".to_owned()));
    let (down, _) = acted(&*faulty, &clock, &sleeper, &["--drain", "--apply"]).await;
    assert_eq!(
        down,
        Err("the hot tier: backend unavailable: down".to_owned())
    );
}

/// Asserts that a wait does not go on when the format was lifted, here deleted, by someone else
/// (the message exactly).
#[tokio::test]
async fn drain_stops_waiting_when_someone_lifts_the_pause() {
    let (hot, clock) = tier();
    set_format(&*hot, &drain_format()).await;
    let tier: Arc<dyn Backend> = hot.clone();
    let sleeper = Settles::new(&tier, 1, None);
    let (result, _) = acted(&*hot, &clock, &sleeper, &["--drain"]).await;
    assert_eq!(
        result,
        Err(
            "not waiting: the format is absent, so the pause was lifted by someone else".to_owned()
        )
    );
}

/// Asserts that `--cancel` refuses when nothing is paused (the messages exactly), for each of an
/// absent object, an active format and a phase this release does not know.
#[tokio::test]
async fn cancel_refuses_when_writes_are_not_paused() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    let (absent, _) = acted(&*hot, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(
        absent,
        Err("nothing to cancel: the format is absent, not drain or drained".to_owned())
    );
    set_format(&*hot, &active_format(2)).await;
    let (active, _) = acted(&*hot, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(
        active,
        Err("nothing to cancel: the format is active, not drain or drained".to_owned())
    );
    hot.delete(FORMAT_KEY).await.unwrap();
    hot.put(FORMAT_KEY, Bytes::from_static(br#"{"phase":"someday"}"#))
        .await
        .unwrap();
    let (unknown, _) = acted(&*hot, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(
        unknown,
        Err("nothing to cancel: the format is unknown, not drain or drained".to_owned())
    );
}

/// Asserts that `--cancel` without `--apply` says what it would do for each kind of drain and
/// writes nothing.
#[tokio::test]
async fn cancel_without_apply_says_what_it_would_do() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    set_format(&*hot, &drain_format()).await;
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--cancel"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "dry run: would delete the format object; nothing was written; add --apply to do it\n"
    );
    let over_epoch = Format {
        phase: FormatPhase::Drained,
        epoch: Some(2),
    };
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &over_epoch).await;
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--cancel"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "dry run: would write the active format back; nothing was written; add --apply to do it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(over_epoch));
}

/// Asserts that `--cancel --apply` of a drain that began from no format deletes the object, and of
/// one that began from an active format writes that format back with its epoch.
#[tokio::test]
async fn cancel_puts_back_the_format_that_was_before_the_drain() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    set_format(&*hot, &drained_format()).await;
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "cancelled: writes resume as every proxy reads the format again\n"
    );
    assert_eq!(stored_format(&*hot).await, None);
    let over_epoch = Format {
        phase: FormatPhase::Drain,
        epoch: Some(2),
    };
    set_format(&*hot, &over_epoch).await;
    let (result, _) = acted(&*hot, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(stored_format(&*hot).await, Some(active_format(2)));
}

/// Asserts that a cancel that finds the format object gone or replaced between its read and its
/// write is refused with the message exactly, for the delete and for the write back.
#[tokio::test]
async fn cancel_refuses_when_the_format_changed_under_it() {
    let (hot, clock) = tier();
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    let sleeper = RecordingSleeper::new();
    let changed = "the format object changed while it was being written; run --check and try again";
    set_format(&*hot, &drain_format()).await;
    faulty.fail_next_with(
        Op::DeleteIfMatch,
        Error::PreconditionFailed(FORMAT_KEY.to_owned()),
    );
    let (deleted, _) = acted(&*faulty, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(deleted, Err(changed.to_owned()));
    hot.delete(FORMAT_KEY).await.unwrap();
    let over_epoch = Format {
        phase: FormatPhase::Drain,
        epoch: Some(2),
    };
    set_format(&*hot, &over_epoch).await;
    faulty.fail_next_with(Op::PutIfMatch, Error::NotFound(FORMAT_KEY.to_owned()));
    let (written, _) = acted(&*faulty, &clock, &sleeper, &["--cancel", "--apply"]).await;
    assert_eq!(written, Err(changed.to_owned()));
}

/// Asserts that `--activate` refuses unless the format says `drained` (the messages exactly), for
/// an absent object of a store that has a journal object, a drain, and an active format of another
/// epoch.
#[tokio::test]
async fn activate_refuses_unless_the_format_is_drained() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    hot.put(
        "_journal/p1/00000000000000000001",
        Bytes::from_static(b"[]"),
    )
    .await
    .unwrap();
    let (absent, _) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        absent,
        Err(
            "refusing to activate: there is no format object and the store has 1 journal \
             objects of a release before the entries; drain it with the bridge release first"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, None);
    set_format(&*hot, &drain_format()).await;
    let (drain, _) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        drain,
        Err("refusing to activate: the format is drain, not drained".to_owned())
    );
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &active_format(3)).await;
    let (other, _) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        other,
        Err("refusing to activate: the format is active, not drained".to_owned())
    );
}

/// Asserts that `--activate` of a store with no format object and no journal object (a new store)
/// writes nothing without `--apply`, creates the active format at epoch 2 with it, and refuses
/// as changed when a format object appeared between the look and the create.
#[tokio::test]
async fn activate_creates_the_format_of_a_store_with_no_journal() {
    let (hot, clock) = tier();
    let sleeper = RecordingSleeper::new();
    let (dry, out) = acted(&*hot, &clock, &sleeper, &["--activate"]).await;
    assert_eq!(dry, Ok(()));
    assert!(out.starts_with("dry run: would write the format"), "{out}");
    assert_eq!(stored_format(&*hot).await, None);

    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    faulty.fail_next_with(Op::Put, Error::AlreadyExists(FORMAT_KEY.to_owned()));
    let (lost, _) = acted(&*faulty, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        lost,
        Err(
            "the format object changed while it was being written; run --check and try again"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, None);

    let (done, out) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(done, Ok(()));
    assert_eq!(
        out.lines().next(),
        Some("wrote the format: active, epoch 2")
    );
    assert_eq!(stored_format(&*hot).await, Some(active_format(2)));
}

/// Asserts that `--activate` of a store that is active at epoch 2 says so and writes nothing.
#[tokio::test]
async fn activate_of_an_active_format_is_done() {
    let (hot, clock) = tier();
    set_format(&*hot, &active_format(2)).await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(out, "the format is already active at epoch 2\n");
}

/// Asserts that `--activate` refuses an unnamed registration that is not running, and a live one
/// below protocol 2 (the messages exactly), and writes nothing.
#[tokio::test]
async fn activate_refuses_registrations_that_are_not_ready() {
    let (hot, clock) = tier();
    set_format(&*hot, &drained_format()).await;
    register_old(&*hot, "old").await;
    let sleeper = RecordingSleeper::new();
    let (unnamed, _) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        unnamed,
        Err(
            "refusing to activate: registrations with no fresh heartbeat that --departed does \
             not name: old"
                .to_owned()
        )
    );
    register(&*hot, "behind", 1).await;
    register(&*hot, "ready", 2).await;
    let flags = ["--activate", "--apply", "--departed", "old"];
    let (behind, _) = acted(&*hot, &clock, &sleeper, &flags).await;
    assert_eq!(
        behind,
        Err("refusing to activate: registrations below protocol 2: behind".to_owned())
    );
    assert_eq!(stored_format(&*hot).await, Some(drained_format()));
}

/// Asserts that `--activate` without `--apply` writes nothing and says what it would write.
#[tokio::test]
async fn activate_without_apply_writes_nothing() {
    let (hot, clock) = tier();
    set_format(&*hot, &drained_format()).await;
    register(&*hot, "a", 2).await;
    let sleeper = RecordingSleeper::new();
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--activate"]).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        "dry run: would write the format {\"epoch\":2,\"phase\":\"active\"}; nothing was written; \
         add --apply to write it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(drained_format()));
}

/// Asserts that `--activate --apply` writes the active format at epoch 2 over `drained`, names the
/// credential fence it does not do, and that a replaced object is refused.
#[tokio::test]
async fn activate_writes_the_active_epoch_and_names_the_fence() {
    let (hot, clock) = tier();
    set_format(&*hot, &drained_format()).await;
    register(&*hot, "a", 2).await;
    let sleeper = RecordingSleeper::new();
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    faulty.fail_next_with(
        Op::PutIfMatch,
        Error::PreconditionFailed(FORMAT_KEY.to_owned()),
    );
    let (lost, _) = acted(&*faulty, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(
        lost,
        Err(
            "the format object changed while it was being written; run --check and try again"
                .to_owned()
        )
    );
    let (result, out) = acted(&*hot, &clock, &sleeper, &["--activate", "--apply"]).await;
    assert_eq!(result, Ok(()));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert_eq!(lines[0], "wrote the format: active, epoch 2");
    assert!(
        lines[1].starts_with("not done by this verb: rotate the old write credential"),
        "{out}"
    );
    assert_eq!(stored_format(&*hot).await, Some(active_format(2)));
}

/// A writer that fails every write.
struct Broken;

impl std::io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that an output that cannot be written is an error of its own, not a success.
#[tokio::test]
async fn an_output_that_cannot_be_written_is_an_error() {
    let (hot, clock) = tier();
    let cli = cli(&["--check"]);
    let sleeper = RecordingSleeper::new();
    let work = Work {
        cli: &cli,
        hot: &*hot,
        clock: &*clock,
        sleeper: &sleeper,
    };
    let result = act(work, &mut Broken).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "writing the output: closed"
    );
}

fn ran(words: &[&str], clock: &TestClock) -> (String, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let sleeper = RecordingSleeper::new();
    let code = run(&args(words), &mut out, &mut err, clock, &sleeper);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts the verb end to end over a `dir:` tier: the flags open the tier, `--check` prints the
/// report on stdout and exits 0, and a refusal prints its message with the verb's name on stderr
/// and exits 1.
#[test]
fn the_verb_opens_the_tier_prints_the_report_and_exits_with_the_result() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-format-epoch");
    let hot = format!("dir:{}", dir.path().display());
    let clock = TestClock::new();
    let (code, out, err) = ran(&["format-epoch", "--hot", &hot, "--check"], &clock);
    assert_eq!(code, status(0));
    assert_eq!(
        out,
        "format: absent\nregistrations: 0\nopen entries: 0\n--drain would be accepted\n"
    );
    assert_eq!(err, "");
    let backend = DirBackend::open(dir.path()).unwrap();
    let put = backend.put("_tiering/proxies/old", Bytes::from_static(b""));
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(put)
        .unwrap();
    let (code, _, err) = ran(
        &["format-epoch", "--hot", &hot, "--drain", "--apply"],
        &clock,
    );
    assert_eq!(code, status(1));
    assert_eq!(
        err,
        "tierstore format-epoch: refusing to drain: registrations with no fresh heartbeat that \
         --departed does not name: old\n"
    );
}

/// Asserts the exit statuses of a request that is not valid: no action, two actions, and a beat
/// count of zero are 2 with the usage error on stderr; help is 0 on stdout; and a tier that cannot
/// be opened is 1 with its message.
#[test]
fn flags_that_are_not_valid_are_status_2_and_help_is_status_0() {
    let clock = TestClock::new();
    let (code, out, err) = ran(&["format-epoch", "--hot", "memory"], &clock);
    assert_eq!((code, out.as_str()), (status(2), ""));
    assert!(err.starts_with("error: "), "{err}");
    let (code, _, _) = ran(
        &["format-epoch", "--hot", "memory", "--check", "--drain"],
        &clock,
    );
    assert_eq!(code, status(2));
    let zero = ["format-epoch", "--hot", "memory", "--check", "--beats", "0"];
    assert_eq!(ran(&zero, &clock).0, status(2));
    let (code, out, err) = ran(&["format-epoch", "--help"], &clock);
    assert_eq!((code, err.as_str()), (status(0), ""));
    assert!(out.contains("Usage: tierstore format-epoch"), "{out}");
    let (code, _, err) = ran(&["format-epoch", "--hot", "s3:bucket", "--check"], &clock);
    assert_eq!(code, status(1));
    assert_eq!(
        err,
        "tierstore format-epoch: --hot \"s3:bucket\": it needs --hot-endpoint\n"
    );
}
