//! `tierstore format-epoch --activate-marks`, `--restore-entries` and the rollback `--drain` over
//! `active-marks`: what each checks, refuses and writes, over memory tiers whose registrations,
//! markers and format a sleeper changes as the leader and the proxies would while the verb waits.

use std::sync::Arc;

use bytes::Bytes;
use clap::Parser;

use super::super::super::testing::{
    register, register_old, register_reporting, set_format, status, stored_format, tier,
};
use super::super::{Cli, Work, act, run};
use super::Marks;
use crate::faulty::{FaultyBackend, Op};
use crate::format::{Format, FormatPhase};
use crate::journal::FORMAT_KEY;
use crate::{
    Backend, Error, MARKER_VERSION, Marker, MarkerState, MemoryBackend, RecordingSleeper, Sleeper,
    TestClock, encode_marker,
};

pub(super) const NAME: &str = "tier-marks";

fn cli(flags: &[&str]) -> Cli {
    let base = ["tierstore format-epoch", "--hot", "memory"];
    Cli::try_parse_from(base.into_iter().chain(flags.iter().copied())).unwrap()
}

/// What the action printed, and what it returned (the message of an error), over the marks bucket
/// `marks` named `NAME`.
pub(super) async fn acted(
    hot: &dyn Backend,
    marks: Option<&dyn Backend>,
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
        marks: marks.map(|store| Marks { store, name: NAME }),
    };
    let result = act(work, &mut out).await.map_err(|why| why.to_string());
    (result, String::from_utf8(out).unwrap())
}

/// Replaces the format object, which `set_format` only creates.
pub(super) async fn replace_format(hot: &dyn Backend, record: &Format) {
    hot.delete(FORMAT_KEY).await.unwrap();
    set_format(hot, record).await;
}

pub(super) fn format(phase: FormatPhase, epoch: Option<u64>, bucket: Option<&str>) -> Format {
    Format {
        phase,
        epoch,
        marks_bucket: bucket.map(str::to_owned),
    }
}

pub(super) fn drained() -> Format {
    format(FormatPhase::Drained, None, None)
}

pub(super) fn active_marks() -> Format {
    format(FormatPhase::ActiveMarks, Some(3), Some(NAME))
}

/// The record of the pause of a rollback.
pub(super) fn rolling_back() -> Format {
    format(FormatPhase::Drain, Some(3), Some(NAME))
}

fn marker(key: &str, st: MarkerState) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone: false,
        dirty: true,
        why: None,
        owner: None,
        nonce: "n".to_owned(),
        extra: serde_json::Map::new(),
    }
}

pub(super) async fn plant(marks: &dyn Backend, key: &str, st: MarkerState) {
    marks
        .put(key, encode_marker(&marker(key, st)))
        .await
        .unwrap();
}

/// A store at `drained` with one live proxy at protocol 3 that acts on `drained`, ready for the
/// activation.
async fn ready() -> (Arc<MemoryBackend>, Arc<MemoryBackend>, Arc<TestClock>) {
    let (hot, clock) = tier();
    set_format(&*hot, &drained()).await;
    register_reporting(&*hot, "a", 3, "drained", 0).await;
    (hot, Arc::new(MemoryBackend::new()), clock)
}

const FENCED: [&str; 3] = ["--activate-marks", "--old-release-fenced", "--apply"];

/// Asserts the activation when every check holds: without `--apply` it prints each check and the
/// record it would write and writes nothing; with it, the stored record is `active-marks`, epoch
/// 3, naming the marks bucket, in the bytes of the fixture of the format.
#[tokio::test]
async fn activate_marks_reports_each_check_and_writes_the_record_only_with_apply() {
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    let flags = ["--activate-marks", "--old-release-fenced"];
    let (dry, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(dry, Ok(()));
    assert_eq!(
        out,
        "format: drained\n\
         registrations: 1 live, all protocol 3 or later, acting on drained and with no write in \
         flight\n\
         no entry and no journal object is held\n\
         marks bucket: \"tier-marks\" exists and is empty (it is not the hot bucket)\n\
         the old release is fenced (asserted by --old-release-fenced)\n\
         dry run: would write the format \
         {\"phase\":\"active-marks\",\"epoch\":3,\"marks_bucket\":\"tier-marks\"}; nothing was \
         written; add --apply to write it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));

    let (done, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(done, Ok(()));
    assert!(
        out.contains("wrote the format: active-marks, epoch 3, marks bucket \"tier-marks\"\n"),
        "{out}"
    );
    assert_eq!(stored_format(&*hot).await, Some(active_marks()));
    let raw = hot.get(FORMAT_KEY, None).await.unwrap();
    assert_eq!(
        &raw[..],
        br#"{"phase":"active-marks","epoch":3,"marks_bucket":"tier-marks"}"#
    );
}

/// Asserts that the activation of a store that is already `active-marks` in this bucket says so and
/// writes nothing, and that one in another bucket, or in any phase but `drained`, is refused with
/// the phase and the way to `drained`.
#[tokio::test]
async fn activate_marks_needs_drained_and_is_done_when_the_record_is_already_this_one() {
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    replace_format(&*hot, &active_marks()).await;
    let (done, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(done, Ok(()));
    assert_eq!(
        out,
        "the format is already active-marks at epoch 3, in the marks bucket \"tier-marks\"\n"
    );
    let tail = "not drained; `--drain --apply` first (the one planned pause of writes), roll the \
                release that knows markers, and run this again";
    let cases = [
        (None, "absent".to_owned()),
        (
            Some(format(FormatPhase::Active, Some(2), None)),
            "active".to_owned(),
        ),
        (
            Some(format(FormatPhase::Drain, None, None)),
            "drain".to_owned(),
        ),
        (
            Some(format(FormatPhase::ActiveMarks, Some(3), Some("other"))),
            "active-marks (it names the marks bucket \"other\")".to_owned(),
        ),
    ];
    for (record, named) in cases {
        hot.delete(FORMAT_KEY).await.unwrap();
        if let Some(record) = &record {
            set_format(&*hot, record).await;
        }
        let (refused, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
        let want = format!("refusing to activate marks: the format is {named}, {tail}");
        assert_eq!(refused, Err(want), "{record:?}");
        assert_eq!(stored_format(&*hot).await, record);
    }
}

/// Asserts the refusals about the proxies: an unnamed registration with no fresh heartbeat, a live
/// one below protocol 3, a live one that has not reported `drained` (no phase, or another), and
/// one that reports `drained` with a write in flight or no count of them, each with its message
/// exactly; and that naming the departed one lets the activation go on.
#[tokio::test]
async fn activate_marks_refuses_proxies_that_are_not_ready_and_accepts_a_departed_one() {
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    register_old(&*hot, "old").await;
    let (unnamed, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(
        unnamed,
        Err(
            "refusing to activate marks: registrations with no fresh heartbeat that --departed \
             does not name: old"
                .to_owned()
        )
    );
    let named = ["--departed", "old"];
    let with = |flags: &[&'static str]| -> Vec<&'static str> {
        flags.iter().chain(named.iter()).copied().collect()
    };
    register(&*hot, "behind", 2).await;
    let (behind, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &with(&FENCED)).await;
    assert_eq!(
        behind,
        Err("refusing to activate: registrations below protocol 3: behind".to_owned())
    );
    register_reporting(&*hot, "behind", 3, "drain", 0).await;
    register(&*hot, "silent-phase", 3).await;
    let (unseen, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &with(&FENCED)).await;
    assert_eq!(
        unseen,
        Err(
            "refusing to activate marks: proxies that have not reported the phase drained, so \
             that may still skip the marker on a hot miss: behind acts on drain, silent-phase \
             reports no phase; run it again after their next beat"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
    register_reporting(&*hot, "behind", 3, "drained", 0).await;
    register_reporting(&*hot, "silent-phase", 4, "drained", 7).await;
    let (writing, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &with(&FENCED)).await;
    assert_eq!(
        writing,
        Err(
            "refusing to activate marks: proxies that may still be finishing a write begun \
             before the pause: silent-phase has 7 writes in flight; run it again after their \
             next beat"
                .to_owned()
        )
    );
    let uncounted = Bytes::from_static(br#"{"proto":3,"boot":"b","hb":1,"phase":"drained"}"#);
    hot.delete("_tiering/proxies/behind").await.unwrap();
    hot.put("_tiering/proxies/behind", uncounted).await.unwrap();
    let (silent, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &with(&FENCED)).await;
    assert_eq!(
        silent,
        Err(
            "refusing to activate marks: proxies that may still be finishing a write begun \
             before the pause: behind reports no count of writes in flight; silent-phase has 7 \
             writes in flight; run it again after their next beat"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
    register_reporting(&*hot, "behind", 3, "drained", 0).await;
    register_reporting(&*hot, "silent-phase", 4, "drained", 0).await;
    let (done, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &with(&FENCED)).await;
    assert_eq!(done, Ok(()));
    assert!(out.contains("registrations: 3 live,"), "{out}");
    assert_eq!(stored_format(&*hot).await, Some(active_marks()));
}

/// Asserts that the activation refuses while an entry is held, or a journal object of a release
/// before the entries exists, each with its count in the message.
#[tokio::test]
async fn activate_marks_refuses_while_an_entry_is_held_or_a_journal_object_exists() {
    use crate::entry::{EntryOp, EntryState};
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    super::super::super::testing::put_entry(&*hot, "k", EntryOp::Put, EntryState::Held, Some("a"))
        .await;
    let (entry, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(
        entry,
        Err(
            "refusing to activate marks: 1 entries under _entries/ are held; the entry protocol \
             finishes every one at the drain, so look at them with `tierstore entries ls`"
                .to_owned()
        )
    );
    let entries = hot.list("_entries/").await.unwrap();
    hot.delete(&entries[0].key).await.unwrap();
    hot.put("_journal/a/1", Bytes::from_static(b"{}"))
        .await
        .unwrap();
    let (journal, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(
        journal,
        Err(
            "refusing to activate marks: the store has 1 journal objects of a release before the \
             entries; drain it with the bridge release first"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

/// Asserts the checks of the marks bucket: one that cannot be listed (it does not exist, or the
/// credentials do not reach it) is a refusal that says how to set it up, and one that holds an
/// object is refused with the key; and that a hot tier with no marks bucket refuses.
#[tokio::test]
async fn activate_marks_refuses_a_marks_bucket_that_is_missing_or_not_empty() {
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    let faulty = FaultyBackend::new(marks.clone());
    faulty.fail_next_with(Op::List, Error::Unavailable("NoSuchBucket".to_owned()));
    let (missing, _) = acted(&*hot, Some(&faulty), &clock, &sleeper, &FENCED).await;
    assert_eq!(
        missing,
        Err(
            "the marks bucket \"tier-marks\" cannot be listed (backend unavailable: \
             NoSuchBucket): create it on the hot store and grant the credentials of the proxies \
             list, read, write and delete on it"
                .to_owned()
        )
    );
    plant(&*marks, "left", MarkerState::Want).await;
    let (full, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &FENCED).await;
    let why = full.unwrap_err();
    assert!(
        why.starts_with(
            "refusing to activate marks: the marks bucket \"tier-marks\" holds an object, \"left\";"
        ),
        "{why}"
    );
    let (none, _) = acted(&*hot, None, &clock, &sleeper, &FENCED).await;
    assert_eq!(
        none,
        Err(
            "this hot tier has no marks bucket in this release (a `dir:<path>` hot tier cannot \
             run the markers); the markers need an `s3:<bucket>` hot tier"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

/// Asserts that the activation does not go on without `--old-release-fenced`, in the dry run too,
/// and says what the rotation is for; and that a format object replaced between the look and the
/// write is refused as changed.
#[tokio::test]
async fn activate_marks_needs_the_old_release_fenced_and_refuses_a_changed_record() {
    let (hot, marks, clock) = ready().await;
    let sleeper = RecordingSleeper::new();
    for flags in [
        &["--activate-marks"][..],
        &["--activate-marks", "--apply"][..],
    ] {
        let (unfenced, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, flags).await;
        let why = unfenced.unwrap_err();
        assert!(
            why.starts_with("refusing to activate marks: the old release is not fenced: an old release that starts after the activation"),
            "{why}"
        );
        assert!(why.ends_with("and then pass --old-release-fenced"), "{why}");
    }
    let faulty = FaultyBackend::new(hot.clone());
    let lost = Error::PreconditionFailed(FORMAT_KEY.to_owned());
    faulty.fail_next_with(Op::PutIfMatch, lost);
    let (lost, _) = acted(&faulty, Some(&*marks), &clock, &sleeper, &FENCED).await;
    assert_eq!(
        lost,
        Err(
            "the format object changed while it was being written; run --check and try again"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

/// A store at `drained` after a rollback, ready for `--restore-entries`: one live proxy that acts
/// on `drained` with no write in flight.
async fn rolled_back() -> (Arc<MemoryBackend>, Arc<MemoryBackend>, Arc<TestClock>) {
    ready().await
}

/// Asserts the way back's last step: without `--apply` it checks and prints the record it would
/// write; with it the stored record is `{epoch 2, active}` and the output says what it does not
/// do; and an `active` format at epoch 2 is done already.
#[tokio::test]
async fn restore_entries_writes_active_epoch_2_over_drained_when_the_proxies_are_at_rest() {
    let (hot, marks, clock) = rolled_back().await;
    let sleeper = RecordingSleeper::new();
    let (dry, out) = acted(
        &*hot,
        Some(&*marks),
        &clock,
        &sleeper,
        &["--restore-entries"],
    )
    .await;
    assert_eq!(dry, Ok(()));
    assert_eq!(
        out,
        "format: drained\n\
         registrations: 1 live, none acting on active-marks and none with a write in flight\n\
         marks bucket: \"tier-marks\" is empty\n\
         dry run: would write the format {\"phase\":\"active\",\"epoch\":2}; nothing was written; \
         add --apply to write it\n"
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
    let flags = ["--restore-entries", "--apply"];
    let (done, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(done, Ok(()));
    assert!(out.contains("wrote the format: active, epoch 2\n"), "{out}");
    assert_eq!(
        stored_format(&*hot).await,
        Some(format(FormatPhase::Active, Some(2), None))
    );
    let (again, out) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(again, Ok(()));
    assert_eq!(out, "the format is already active at epoch 2\n");
}

/// Asserts that the restore refuses in every phase but `drained`, naming the phase and the way to
/// `drained`, and writes nothing.
#[tokio::test]
async fn restore_entries_refuses_every_phase_but_drained() {
    let (hot, marks, clock) = rolled_back().await;
    let sleeper = RecordingSleeper::new();
    for (record, named) in [
        (Some(active_marks()), "active-marks"),
        (Some(rolling_back()), "drain"),
        (None, "absent"),
    ] {
        hot.delete(FORMAT_KEY).await.unwrap();
        if let Some(record) = &record {
            set_format(&*hot, record).await;
        }
        let flags = ["--restore-entries", "--apply"];
        let (refused, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
        assert_eq!(
            refused,
            Err(format!(
                "refusing to restore entries: the format is {named}, not drained; to leave \
                 active-marks run `--drain --apply` and let it write drained"
            ))
        );
        assert_eq!(stored_format(&*hot).await, record);
    }
}

/// Asserts the refusals of the restore about the proxies: one that still acts on `active-marks`
/// (the window after the phase changed), one with a write in flight, one that reports neither,
/// each in a message that names it; a proxy of protocol 2, which writes no marker, is not in the
/// way; and an unnamed registration with no heartbeat is refused as in the other verbs.
#[tokio::test]
async fn restore_entries_refuses_a_proxy_that_may_still_write_a_marker() {
    let (hot, marks, clock) = rolled_back().await;
    let sleeper = RecordingSleeper::new();
    let flags = ["--restore-entries", "--apply"];
    register_reporting(&*hot, "marking", 3, "active-marks", 0).await;
    register_reporting(&*hot, "writing", 3, "drain", 2).await;
    register(&*hot, "silent", 3).await;
    register(&*hot, "old-release", 2).await;
    let (busy, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(
        busy,
        Err(
            "refusing to restore entries: proxies that may still write a marker: marking acts on \
             active-marks; silent reports no phase; silent reports no count of writes in flight; \
             writing has 2 writes in flight"
                .to_owned()
        )
    );
    register_old(&*hot, "unlabelled").await;
    let (unnamed, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(
        unnamed,
        Err(
            "refusing to restore entries: registrations with no fresh heartbeat that --departed \
             does not name: unlabelled"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

/// Asserts that the restore refuses while a marker exists in the marks bucket (the window after
/// the phase change), while an entry is held, and with no marks bucket; and that a record replaced
/// meanwhile is refused as changed.
#[tokio::test]
async fn restore_entries_refuses_while_a_marker_or_an_entry_exists_or_the_record_changed() {
    use crate::entry::{EntryOp, EntryState};
    let (hot, marks, clock) = rolled_back().await;
    let sleeper = RecordingSleeper::new();
    let flags = ["--restore-entries", "--apply"];
    plant(&*marks, "k", MarkerState::Dirty).await;
    let (marked, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    let why = marked.unwrap_err();
    assert!(
        why.starts_with(
            "refusing to restore entries: the marks bucket \"tier-marks\" holds an object, \"k\";"
        ),
        "{why}"
    );
    marks.delete("k").await.unwrap();
    super::super::super::testing::put_entry(&*hot, "k", EntryOp::Put, EntryState::Held, Some("a"))
        .await;
    let (entry, _) = acted(&*hot, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(
        entry,
        Err(
            "refusing to restore entries: 1 entries under _entries/ are held; the entry protocol \
             finishes every one at the drain, so look at them with `tierstore entries ls`"
                .to_owned()
        )
    );
    let (none, _) = acted(&*hot, None, &clock, &sleeper, &flags).await;
    assert!(
        none.unwrap_err()
            .starts_with("this hot tier has no marks bucket")
    );
    let entries = hot.list("_entries/").await.unwrap();
    hot.delete(&entries[0].key).await.unwrap();
    let faulty = FaultyBackend::new(hot.clone());
    let lost = Error::PreconditionFailed(FORMAT_KEY.to_owned());
    faulty.fail_next_with(Op::PutIfMatch, lost);
    let (lost, _) = acted(&faulty, Some(&*marks), &clock, &sleeper, &flags).await;
    assert_eq!(
        lost,
        Err(
            "the format object changed while it was being written; run --check and try again"
                .to_owned()
        )
    );
    assert_eq!(stored_format(&*hot).await, Some(drained()));
}

fn ran(words: &[&str]) -> (String, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let clock = TestClock::new();
    let sleeper = RecordingSleeper::new();
    let code = run(
        &super::super::super::testing::args(words),
        &mut out,
        &mut err,
        &clock,
        &sleeper,
    );
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts the verbs end to end from their flags: over a `dir:` hot tier, which has no marks
/// bucket, `--activate-marks` and `--restore-entries` are refused with status 1 and the reason;
/// over `memory` (a store of its own for each run) they are refused for the format that is absent;
/// and `--marks-bucket` is refused for a hot tier that is not an `s3` one.
#[test]
fn the_verbs_from_their_flags_refuse_a_hot_tier_with_no_marks_bucket_and_a_misplaced_flag() {
    let dir =
        tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tierstore-format-marks");
    let hot = format!("dir:{}", dir.path().display());
    for verb in ["--activate-marks", "--restore-entries"] {
        let (code, out, err) = ran(&["format-epoch", verb, "--hot", &hot]);
        assert_eq!((code, out.as_str()), (status(1), ""));
        assert!(
            err.starts_with("tierstore format-epoch: this hot tier has no marks bucket"),
            "{err}"
        );
        let (code, _, err) = ran(&["format-epoch", verb, "--hot", "memory"]);
        assert_eq!(code, status(1));
        assert!(
            err.contains("the format is absent, not drained") || err.contains("drained"),
            "{err}"
        );
    }
    let (code, _, err) = ran(&[
        "format-epoch",
        "--check",
        "--hot",
        "memory",
        "--marks-bucket",
        "m",
    ]);
    assert_eq!(code, status(1));
    assert_eq!(
        err,
        "tierstore format-epoch: --marks-bucket \"m\" is only for an `s3:<bucket>` hot tier\n"
    );
}

/// Asserts the name of the marks bucket of an `s3` hot tier: `<bucket>-marks` unless named, and a
/// name that is empty or is the hot bucket is refused, before any request is made.
#[test]
fn the_marks_bucket_of_an_s3_hot_tier_is_the_named_one_or_the_default_and_never_the_hot_bucket() {
    let cli = |flags: &[&str]| {
        let base = ["tierstore format-epoch", "--check", "--hot", "s3:data"];
        let extra = ["--hot-endpoint", "http://hot.invalid"];
        let all = base.into_iter().chain(extra).chain(flags.iter().copied());
        Cli::try_parse_from(all).unwrap()
    };
    let dir =
        tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tierstore-format-marks-cred");
    let file = dir.path().join("credentials");
    std::fs::write(&file, "id\nsecret\n").unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let credentials = file.display().to_string();
    let with = |flags: &[&str]| {
        let mut all = vec!["--hot-credentials-file", credentials.as_str()];
        all.extend_from_slice(flags);
        let cli = cli(&all);
        cli.hot.open_marks(cli.marks_bucket.as_deref())
    };
    assert_eq!(
        with(&[]).unwrap().map(|(name, _)| name),
        Some("data-marks".to_owned())
    );
    let named = with(&["--marks-bucket", "elsewhere"]).unwrap();
    assert_eq!(named.map(|(name, _)| name), Some("elsewhere".to_owned()));
    for bad in ["data", ""] {
        let refused = with(&["--marks-bucket", bad]).err().unwrap().to_string();
        assert_eq!(
            refused,
            format!(
                "--marks-bucket {bad:?}: the markers need a bucket of their own, not empty and not the hot bucket"
            )
        );
    }
}
