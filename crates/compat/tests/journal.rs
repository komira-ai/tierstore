//! Journal objects shared by this branch and the release before it, in one directory: what one
//! wrote, the other reads. The release before is the real `tierstore` library of its commit
//! (`tierstore_prev`), and its own decoder and encoder read and write the bytes, not a copy of
//! its journal format.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use tierstore::{Backend, DirBackend, JournalEntry, JournalOp, decode_journal, encode_journal};
use tierstore_prev::{
    Backend as PrevBackend, DirBackend as PrevDir, JournalEntry as PrevEntry, JournalOp as PrevOp,
    decode_journal as prev_decode, encode_journal as prev_encode,
};
use tierstore_testkit::dir::TestDir;

/// The key of a journal object of a proxy.
const OBJECT: &str = "journal-object";

/// An entry of this branch, which may name a cold tag (and a hot tag, which a test sets).
fn now_entry(op: JournalOp, key: &str, size: u64, tag: Option<&str>) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size,
        time: 1_790_000_000_000 + size,
        cold_tag: tag.map(str::to_owned),
        hot_tag: None,
    }
}

/// An entry of the previous release.
fn before_entry(op: PrevOp, key: &str, size: u64) -> PrevEntry {
    PrevEntry {
        op,
        key: key.to_owned(),
        size,
        time: 1_790_000_000_000 + size,
    }
}

/// Asserts that the previous release reads a journal object that this branch wrote with every
/// kind of entry: the plain puts and deletes are read as before, with their keys, sizes and times,
/// and the reserved put and the mirror (ops it does not know, with the fields `cold_tag` and
/// `hot_tag`, which it does not know) are read as entries it skips, with their keys.
#[tokio::test]
async fn the_previous_release_reads_a_journal_object_this_branch_wrote() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-journal-new");
    let now = DirBackend::open(dir.path()).unwrap();
    let before = PrevDir::open(dir.path()).unwrap();

    let mut mirror = now_entry(JournalOp::PutMirror, "c", 3, Some("object-tag"));
    mirror.hot_tag = Some("hot-object-tag".to_owned());
    let entries = [
        now_entry(JournalOp::Put, "a", 1, None),
        now_entry(JournalOp::PutReserved, "b", 2, Some("reservation-tag")),
        mirror,
        now_entry(JournalOp::Delete, "d", 0, None),
    ];
    now.put(OBJECT, encode_journal(&entries)).await.unwrap();

    let bytes = before.get(OBJECT, None).await.unwrap();
    let read = prev_decode(&bytes).unwrap();
    let ops: Vec<PrevOp> = read.iter().map(|entry| entry.op).collect();
    assert_eq!(
        ops,
        [
            PrevOp::Put,
            PrevOp::Unknown,
            PrevOp::Unknown,
            PrevOp::Delete
        ]
    );
    let seen: Vec<(&str, u64, u64)> = read
        .iter()
        .map(|entry| (entry.key.as_str(), entry.size, entry.time))
        .collect();
    let want: Vec<(&str, u64, u64)> = entries
        .iter()
        .map(|entry| (entry.key.as_str(), entry.size, entry.time))
        .collect();
    assert_eq!(seen, want);
}

/// Asserts that this branch reads a journal object that the previous release wrote: the entries
/// are the ones it wrote, none names a cold tag or a hot tag, and what this branch writes for the
/// same entries is the same bytes.
#[tokio::test]
async fn this_branch_reads_a_journal_object_the_previous_release_wrote() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-journal-old");
    let now = DirBackend::open(dir.path()).unwrap();
    let before = PrevDir::open(dir.path()).unwrap();

    let written = [
        before_entry(PrevOp::Put, "a", 1),
        before_entry(PrevOp::Delete, "d", 0),
    ];
    let encoded: Bytes = prev_encode(&written);
    before.put(OBJECT, encoded.clone()).await.unwrap();

    let bytes = now.get(OBJECT, None).await.unwrap();
    let read = decode_journal(&bytes).unwrap();
    let want = [
        now_entry(JournalOp::Put, "a", 1, None),
        now_entry(JournalOp::Delete, "d", 0, None),
    ];
    assert_eq!(read, want);
    assert_eq!(encode_journal(&want), encoded);
}
