//! The rollback of `tierstore format-epoch --drain` over `active-marks`: what it waits for, in what
//! order, and what ends or interrupts the wait, over memory tiers that a sleeper changes as the
//! proxies and the leader would while the verb waits.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;

use super::super::super::testing::{
    register_old, register_reporting, set_format, stored_format, tier,
};
use super::tests::{acted, active_marks, drained, format, plant, rolling_back};
use crate::faulty::{FaultyBackend, Op};
use crate::format::{Format, FormatPhase};
use crate::journal::FORMAT_KEY;
use crate::{Backend, Error, MarkerState, MemoryBackend, RecordingSleeper, Sleeper, TestClock};

/// One thing the sleeper does while the verb waits.
enum Do {
    /// The proxy writes its registration again.
    Beat(&'static str, &'static str, u64),
    /// The leader finishes the marker of the key.
    Finish(&'static str),
    /// Another operator replaces the format object (none: deletes it).
    Format(Option<Format>),
}

/// A sleeper that does, at its n-th call, the n-th list of things, and writes down how often it
/// was asked.
struct Plays {
    hot: Arc<MemoryBackend>,
    marks: Arc<MemoryBackend>,
    steps: Mutex<VecDeque<Vec<Do>>>,
    asked: Mutex<usize>,
}

impl Plays {
    fn new(hot: &Arc<MemoryBackend>, marks: &Arc<MemoryBackend>, steps: Vec<Vec<Do>>) -> Arc<Self> {
        Arc::new(Self {
            hot: hot.clone(),
            marks: marks.clone(),
            steps: Mutex::new(steps.into()),
            asked: Mutex::new(0),
        })
    }

    fn asked(&self) -> usize {
        *self.asked.lock().unwrap()
    }
}

#[async_trait]
impl Sleeper for Plays {
    async fn sleep(&self, _duration: std::time::Duration) {
        let asked = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked
        };
        assert!(asked < 50, "the verb waits for something no step provides");
        let step = self.steps.lock().unwrap().pop_front().unwrap_or_default();
        for item in step {
            match item {
                Do::Beat(id, phase, writes) => {
                    register_reporting(&*self.hot, id, 3, phase, writes).await;
                }
                Do::Finish(key) => self.marks.delete(key).await.unwrap(),
                Do::Format(record) => {
                    self.hot.delete(FORMAT_KEY).await.unwrap();
                    if let Some(record) = &record {
                        set_format(&*self.hot, record).await;
                    }
                }
            }
        }
    }
}

/// A store at `active-marks` with one live proxy `a` that acts on it and has a write in flight.
async fn marking() -> (Arc<MemoryBackend>, Arc<MemoryBackend>, Arc<TestClock>) {
    let (hot, clock) = tier();
    set_format(&*hot, &active_marks()).await;
    register_reporting(&*hot, "a", 3, "active-marks", 1).await;
    (hot, Arc::new(MemoryBackend::new()), clock)
}

/// Asserts the rollback from the first look to `drained`: `--drain` over `active-marks` writes
/// `drain` with the epoch and the bucket kept; the proxy that still acts on `active-marks` with a
/// write in flight is waited for (one poll each), then the markers, where a `WANT` marker is
/// deleted at once and a `DIRTY` and a `LOCK` one are waited for, counted by state; `drained` is
/// written only after the marks bucket is seen empty, and is the bytes of a plain `drained`.
#[tokio::test]
async fn the_rollback_drain_waits_for_the_proxies_then_the_markers_and_writes_drained() {
    let (hot, marks, clock) = marking().await;
    plant(&*marks, "d", MarkerState::Dirty).await;
    plant(&*marks, "l", MarkerState::Lock).await;
    plant(&*marks, "w", MarkerState::Want).await;
    let sleeper = Plays::new(
        &hot,
        &marks,
        vec![
            vec![Do::Beat("a", "drain", 1)],
            vec![Do::Beat("a", "drain", 0)],
            vec![Do::Finish("d")],
            vec![Do::Finish("l")],
        ],
    );
    let flags = ["--drain", "--apply"];
    let (done, out) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &flags).await;
    assert_eq!(done, Ok(()));
    assert_eq!(
        out,
        "wrote the format: drain, over active-marks\n\
         waiting until every proxy reports a phase other than active-marks with no write in \
         flight, and the leader has finished the markers of the marks bucket \"tier-marks\"; \
         there is no time limit, interrupt to stop and --cancel to resume writes\n\
         still waiting: a acts on active-marks; a has 1 writes in flight\n\
         still waiting: a has 1 writes in flight\n\
         still waiting: 2 markers are not finished (DIRTY 1, LOCK 1, CLEARING 0, other 0)\n\
         still waiting: 1 markers are not finished (DIRTY 0, LOCK 1, CLEARING 0, other 0)\n\
         drained: no proxy writes a marker, the marks bucket is empty (1 WANT markers were deleted). Roll the release before \
         the markers back if it is to run, and then run `--restore-entries --apply`\n"
    );
    assert_eq!(sleeper.asked(), 4);
    assert!(marks.list("").await.unwrap().is_empty());
    let raw = hot.get(FORMAT_KEY, None).await.unwrap();
    assert_eq!(&raw[..], br#"{"phase":"drained"}"#);
}

/// Asserts that the first look of the rollback writes the pause with the bucket kept and the epoch,
/// and that without `--apply` nothing is written and nothing waited for; and that `--check`
/// reports the registration's phase and writes in flight.
#[tokio::test]
async fn the_rollback_drain_without_apply_writes_nothing_and_a_check_shows_what_it_waits_for() {
    let (hot, marks, clock) = marking().await;
    let sleeper = Plays::new(&hot, &marks, vec![]);
    let (dry, out) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &["--drain"]).await;
    assert_eq!(dry, Ok(()));
    assert_eq!(
        out,
        "dry run: would write the format drain over active-marks, keeping the marks bucket \
         \"tier-marks\" in it, and wait until the proxies are at rest and the markers are done; \
         nothing was written; add --apply to do it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(active_marks()));
    assert_eq!(sleeper.asked(), 0);
    let (checked, out) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &["--check"]).await;
    assert_eq!(checked, Ok(()));
    assert!(
        out.contains(
            "  a: live, protocol 3, phase active-marks, 1 writes in flight, written 0s ago\n"
        ),
        "{out}"
    );
}

/// Asserts that a rollback is refused when the flags name another marks bucket than the record
/// (the verb would watch the wrong bucket), when a registration is silent and unnamed, and that
/// naming the bucket right and the silent one as departed lets it begin; a store error is passed
/// on.
#[tokio::test]
async fn the_rollback_drain_refuses_another_bucket_and_an_unnamed_silent_proxy() {
    let (hot, marks, clock) = marking().await;
    let sleeper = RecordingSleeper::new();
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(
        &*hot,
        &format(FormatPhase::ActiveMarks, Some(3), Some("other")),
    )
    .await;
    let (wrong, _) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &sleeper,
        &["--drain", "--apply"],
    )
    .await;
    assert_eq!(
        wrong,
        Err(
            "the format record names the marks bucket Some(\"other\"), and the flags name \
             \"tier-marks\"; pass the bucket of hot_store.marks_bucket (--marks-bucket)"
                .to_owned()
        )
    );
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &active_marks()).await;
    register_old(&*hot, "old").await;
    let (silent, _) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &sleeper,
        &["--drain", "--apply"],
    )
    .await;
    assert_eq!(
        silent,
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: old"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(active_marks()));
    let faulty = FaultyBackend::new(hot.clone());
    faulty.fail_next_with(Op::PutIfMatch, Error::PreconditionFailed(FORMAT_KEY.into()));
    let flags = ["--drain", "--apply", "--departed", "old"];
    let (lost, _) = acted(&faulty, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(
        lost,
        Err(
            "the format object changed while it was being written; run --check and try again"
                .to_owned()
        )
    );
}

/// Asserts that a rollback that was begun is resumed by running `--drain` again: it does not write
/// `drain` again, waits as before and writes `drained`; the dry run says it would wait; and
/// another bucket is refused.
#[tokio::test]
async fn a_rollback_that_was_begun_is_resumed_by_running_drain_again() {
    let (hot, marks, clock) = marking().await;
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &rolling_back()).await;
    register_reporting(&*hot, "a", 3, "drain", 0).await;
    let sleeper = Plays::new(&hot, &marks, vec![]);
    let (dry, out) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &["--drain"]).await;
    assert_eq!(dry, Ok(()));
    assert_eq!(
        out,
        "the format already says drain, over active-marks\n\
         dry run: would wait until the proxies are at rest and the markers are done, and write \
         drained; nothing was written; add --apply to do it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(rolling_back()));
    let (done, out) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &*sleeper,
        &["--drain", "--apply"],
    )
    .await;
    assert_eq!(done, Ok(()));
    assert!(out.starts_with("the format already says drain, over active-marks\nwaiting until"));
    assert!(!out.contains("wrote the format"), "{out}");
    assert_eq!(stored_format(&*hot).await, Some(drained()));

    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &format(FormatPhase::Drain, Some(3), Some("other"))).await;
    let (wrong, _) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &*sleeper,
        &["--drain", "--apply"],
    )
    .await;
    assert!(
        wrong
            .unwrap_err()
            .starts_with("the format record names the marks bucket")
    );
}

/// Asserts what the waiting rollback does when the format object changes under it: it stops with
/// the phase if someone cancelled it or lifted the pause, and is done if someone else wrote
/// `drained`.
#[tokio::test]
async fn the_rollback_wait_stops_when_the_format_is_changed_by_someone_else() {
    for (replaced, outcome) in [
        (
            Some(active_marks()),
            Err(
                "not waiting: the format is active-marks, so the rollback was changed by someone \
                 else"
                    .to_owned(),
            ),
        ),
        (
            None,
            Err(
                "not waiting: the format is absent, so the rollback was changed by someone else"
                    .to_owned(),
            ),
        ),
        (Some(drained()), Ok(())),
    ] {
        let (hot, marks, clock) = marking().await;
        let sleeper = Plays::new(&hot, &marks, vec![vec![Do::Format(replaced)]]);
        let flags = ["--drain", "--apply"];
        let (result, out) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &flags).await;
        assert_eq!(result, outcome);
        assert_eq!(
            out.ends_with("drained: the format was written by another run\n"),
            outcome.is_ok(),
            "{out}"
        );
    }
}

/// Asserts that a proxy that goes silent during the wait aborts it with the registrations to name
/// as departed, and that naming it lets the wait finish.
#[tokio::test]
async fn the_rollback_wait_stops_for_a_proxy_that_went_silent_and_goes_on_when_it_is_named() {
    let (hot, marks, clock) = marking().await;
    register_reporting(&*hot, "a", 3, "drain", 0).await;
    let sleeper = Plays::new(&hot, &marks, vec![]);
    plant(&*marks, "d", MarkerState::Dirty).await;
    let first = RecordingSleeper::new();
    // The clock moves past the window of fresh beats while the verb waits for the marker.
    struct Ages(Arc<TestClock>, Arc<MemoryBackend>);
    #[async_trait]
    impl Sleeper for Ages {
        async fn sleep(&self, _duration: std::time::Duration) {
            self.0.advance(60_000);
            self.1.delete("d").await.unwrap();
        }
    }
    let ages = Ages(clock.clone(), marks.clone());
    let flags = ["--drain", "--apply"];
    let (stopped, _) = acted(&*hot, Some(&*marks), &clock, &ages, &flags).await;
    assert_eq!(
        stopped,
        Err(
            "refusing to drain: registrations with no fresh heartbeat that --departed does not \
             name: a"
                .to_owned()
        )
    );
    assert_eq!(first.asked().len() + sleeper.asked(), 0);
    let named = ["--drain", "--apply", "--departed", "a"];
    let (done, _) = acted(&*hot, Some(&*marks), &clock, &*sleeper, &named).await;
    assert_eq!(done, Ok(()));
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

/// Asserts how the marks bucket is read while waiting: markers of a state this release does not
/// know and bodies that are not markers are waited for and counted as other, a marker that vanishes
/// between the listing and its read is nothing, a `WANT` that changed under its delete is waited
/// for, a failed read is the store's error, and more markers than a page are all counted.
#[tokio::test]
async fn the_rollback_counts_every_kind_of_marker_and_every_page() {
    let (hot, marks, clock) = marking().await;
    register_reporting(&*hot, "a", 3, "drain", 0).await;
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &rolling_back()).await;
    plant(&*marks, "c", MarkerState::Clearing).await;
    plant(&*marks, "o", MarkerState::Other("LATER".to_owned())).await;
    marks.put("junk", Bytes::from_static(b"{")).await.unwrap();
    plant(&*marks, "w1", MarkerState::Want).await;
    plant(&*marks, "w2", MarkerState::Want).await;
    let faulty = Arc::new(FaultyBackend::new(marks.clone()));
    faulty.fail_next_on(Op::Get, "c", Error::NotFound("c".to_owned()));
    faulty.fail_next_on(
        Op::DeleteIfMatch,
        "w1",
        Error::PreconditionFailed("w1".to_owned()),
    );
    faulty.fail_next_on(Op::DeleteIfMatch, "w2", Error::NotFound("w2".to_owned()));
    let sleeper = Plays::new(
        &hot,
        &marks,
        vec![
            vec![Do::Finish("o"), Do::Finish("junk")],
            vec![Do::Finish("c")],
        ],
    );
    let flags = ["--drain", "--apply"];
    let (done, out) = acted(&*hot, Some(&*faulty), &clock, &*sleeper, &flags).await;
    assert_eq!(done, Ok(()));
    assert!(
        out.contains(
            "still waiting: 3 markers are not finished (DIRTY 0, LOCK 0, CLEARING 0, other 3)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("(1 WANT markers were deleted)") || out.contains("WANT markers were deleted"),
        "{out}"
    );

    let (hot, marks, clock) = marking().await;
    register_reporting(&*hot, "a", 3, "drain", 0).await;
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &rolling_back()).await;
    for n in 0..1001 {
        plant(&*marks, &format!("k{n:04}"), MarkerState::Dirty).await;
    }
    let sleeper = Plays::new(&hot, &marks, vec![vec![]]);
    let faulty = FaultyBackend::new(marks.clone());
    faulty.fail_next_on(Op::Get, "k1000", Error::Unavailable("down".to_owned()));
    let (failed, _) = acted(
        &*hot,
        Some(&faulty),
        &clock,
        &*sleeper,
        &["--drain", "--apply"],
    )
    .await;
    assert_eq!(
        failed,
        Err("the hot tier: backend unavailable: down".to_owned())
    );
}

/// Asserts that cancelling the pause of a rollback puts `active-marks` back with its epoch and
/// bucket (not `active` at epoch 3, which would make the proxies write on entries beside the
/// markers), and that the dry run says so.
#[tokio::test]
async fn cancelling_the_pause_of_a_rollback_puts_active_marks_back() {
    let (hot, marks, clock) = marking().await;
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(&*hot, &rolling_back()).await;
    let sleeper = RecordingSleeper::new();
    let (dry, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &["--cancel"]).await;
    assert_eq!(dry, Ok(()));
    assert_eq!(stored_format(&*hot).await, Some(rolling_back()));
    let (done, _) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &sleeper,
        &["--cancel", "--apply"],
    )
    .await;
    assert_eq!(done, Ok(()));
    assert_eq!(stored_format(&*hot).await, Some(active_marks()));
}
