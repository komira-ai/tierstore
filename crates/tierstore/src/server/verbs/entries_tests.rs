//! `tierstore entries`: what `ls` and `show` print of the entries in a memory tier whose clock the
//! test holds, how they treat an entry that is gone or cannot be read, and the verb from its
//! arguments to its exit status.

use std::sync::Arc;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;

use super::super::testing::{args, put_entry, status, tier};
use super::{list, op_name, run, show, state_name};
use crate::entry::{EntryOp, EntryState, entry_key};
use crate::faulty::{FaultyBackend, Op};
use crate::{Backend, DirBackend, Error, TestClock};

async fn listed(hot: &dyn Backend, now: u64) -> (Result<(), String>, String) {
    let mut out = Vec::new();
    let result = list(hot, now, &mut out)
        .await
        .map_err(|why| why.to_string());
    (result, String::from_utf8(out).unwrap())
}

async fn shown(hot: &dyn Backend, now: u64, key: &str) -> (Result<(), String>, String) {
    let mut out = Vec::new();
    let result = show(hot, now, key, &mut out)
        .await
        .map_err(|why| why.to_string());
    (result, String::from_utf8(out).unwrap())
}

fn name_of(key: &str) -> String {
    entry_key(key).strip_prefix("_entries/").unwrap().to_owned()
}

/// Asserts that every operation and every state has its name in the listing, the unknown ones of a
/// newer writer included.
#[test]
fn every_operation_and_state_has_a_name() {
    let ops = [
        (EntryOp::Put, "PUT"),
        (EntryOp::Del, "DEL"),
        (EntryOp::Cb, "CB"),
        (EntryOp::Evict, "EVICT"),
        (EntryOp::Unknown, "UNKNOWN"),
    ];
    for (op, name) in ops {
        assert_eq!(op_name(op), name);
    }
    let states = [
        (EntryState::Held, "HELD"),
        (EntryState::Pending, "PENDING"),
        (EntryState::Sealed, "SEALED"),
        (EntryState::Unknown, "UNKNOWN"),
    ];
    for (state, name) in states {
        assert_eq!(state_name(state), name);
    }
}

/// Asserts that a store with no entries lists none.
#[tokio::test]
async fn ls_of_no_entries_says_so() {
    let (hot, clock) = tier();
    let (result, out) = listed(&*hot, clock.now()).await;
    assert_eq!(result, Ok(()));
    assert_eq!(out, "0 open entries\n");
}

/// Asserts the line of each entry: the name, the operation, the state, the owner (`-` for none),
/// the whole seconds since the last write, and the key; and the count.
#[tokio::test]
async fn ls_prints_a_line_for_each_open_entry() {
    let (hot, clock) = tier();
    put_entry(&*hot, "a/one", EntryOp::Put, EntryState::Held, Some("p1")).await;
    put_entry(&*hot, "a/two", EntryOp::Del, EntryState::Pending, None).await;
    clock.advance(5_999);
    let (result, out) = listed(&*hot, clock.now()).await;
    assert_eq!(result, Ok(()));
    let mut expected = vec![
        format!(
            "{} PUT HELD owner=p1 age=5s key=\"a/one\"",
            name_of("a/one")
        ),
        format!(
            "{} DEL PENDING owner=- age=5s key=\"a/two\"",
            name_of("a/two")
        ),
    ];
    expected.sort();
    expected.push("2 open entries".to_owned());
    assert_eq!(out, format!("{}\n", expected.join("\n")));
}

/// Asserts that a key with a newline in it is printed escaped, so that it cannot make a line of
/// its own.
#[tokio::test]
async fn ls_escapes_a_key_that_has_a_newline() {
    let (hot, clock) = tier();
    put_entry(&*hot, "a\nb", EntryOp::Put, EntryState::Held, None).await;
    let (_, out) = listed(&*hot, clock.now()).await;
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(out.contains("key=\"a\\nb\"\n"), "{out}");
}

/// Asserts that an entry whose body cannot be read is listed with the reason and counted, and
/// that the others are still listed.
#[tokio::test]
async fn ls_lists_an_entry_it_cannot_read_and_goes_on() {
    let (hot, clock) = tier();
    hot.put("_entries/zz", Bytes::from_static(b"not json"))
        .await
        .unwrap();
    put_entry(&*hot, "k", EntryOp::Evict, EntryState::Sealed, Some("r")).await;
    let (result, out) = listed(&*hot, clock.now()).await;
    assert_eq!(result, Ok(()));
    assert!(
        out.contains("zz unreadable: backend error: corrupt entry: "),
        "{out}"
    );
    assert!(out.contains(" EVICT SEALED owner=r "), "{out}");
    assert!(out.ends_with("2 open entries\n"), "{out}");
}

/// Asserts that an entry deleted between the listing and its read is left out, and that any other
/// failure of the store is the store's, named as the hot tier.
#[tokio::test]
async fn ls_leaves_out_an_entry_that_vanished_and_fails_on_a_store_error() {
    let (hot, clock) = tier();
    put_entry(&*hot, "k", EntryOp::Put, EntryState::Held, None).await;
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    faulty.fail_next_on(Op::Get, "_entries/", Error::NotFound("gone".to_owned()));
    let (result, out) = listed(&*faulty, clock.now()).await;
    assert_eq!((result, out.as_str()), (Ok(()), "0 open entries\n"));
    faulty.fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    let (result, _) = listed(&*faulty, clock.now()).await;
    assert_eq!(
        result,
        Err("the hot tier: backend unavailable: down".to_owned())
    );
}

/// Asserts that a key with no entry is said to have none, with the name of the object.
#[tokio::test]
async fn show_of_a_key_with_no_entry_says_so() {
    let (hot, clock) = tier();
    let (result, out) = shown(&*hot, clock.now(), "nokey").await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        format!(
            "no open entry for key \"nokey\" (object _entries/{})\n",
            name_of("nokey")
        )
    );
}

/// Asserts every line of the entry of a key: the object, the key, the operation, the state, the
/// owner, the nonce, the creation time, the age since the last write, and the content tag.
#[tokio::test]
async fn show_prints_the_entry_of_a_key() {
    let (hot, clock) = tier();
    put_entry(&*hot, "a/one", EntryOp::Put, EntryState::Held, Some("p1")).await;
    let tag = hot.head(&entry_key("a/one")).await.unwrap().tag;
    clock.advance(12_000);
    let (result, out) = shown(&*hot, clock.now(), "a/one").await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        out,
        format!(
            "object: _entries/{}\nkey: \"a/one\"\nop: PUT\nstate: HELD\nowner: p1\nnonce: n1\n\
             created_ms: 7\nage: 12s since the last write\ntag: {tag}\n",
            name_of("a/one")
        )
    );
}

/// Asserts that an entry with no owner says `none`.
#[tokio::test]
async fn show_says_none_for_an_entry_with_no_owner() {
    let (hot, clock) = tier();
    put_entry(&*hot, "k", EntryOp::Del, EntryState::Pending, None).await;
    let (_, out) = shown(&*hot, clock.now(), "k").await;
    assert!(out.contains("\nowner: none\n"), "{out}");
}

/// Asserts that an entry that cannot be read is an error naming the object, that one deleted
/// between the head and the get is said to be gone, and that another failure is the store's.
#[tokio::test]
async fn show_reports_a_body_it_cannot_read_a_vanished_entry_and_a_store_error() {
    let (hot, clock) = tier();
    hot.put(&entry_key("bad"), Bytes::from_static(b"not json"))
        .await
        .unwrap();
    let (result, _) = shown(&*hot, clock.now(), "bad").await;
    let why = result.unwrap_err();
    assert!(
        why.starts_with(&format!(
            "{}: backend error: corrupt entry: ",
            entry_key("bad")
        )),
        "{why}"
    );
    put_entry(&*hot, "k", EntryOp::Put, EntryState::Held, None).await;
    let faulty = Arc::new(FaultyBackend::new(hot.clone()));
    faulty.fail_next_with(Op::Get, Error::NotFound("gone".to_owned()));
    let (result, out) = shown(&*faulty, clock.now(), "k").await;
    assert_eq!(result, Ok(()));
    assert!(out.starts_with("no open entry for key \"k\""), "{out}");
    faulty.fail_next_with(Op::Head, Error::Unavailable("down".to_owned()));
    let (result, _) = shown(&*faulty, clock.now(), "k").await;
    assert_eq!(
        result,
        Err("the hot tier: backend unavailable: down".to_owned())
    );
}

fn ran(words: &[&str], clock: &TestClock) -> (String, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(words), &mut out, &mut err, clock);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Asserts the verb end to end over a `dir:` tier: `ls` and `show` read the entry the directory
/// holds and exit 0, a corrupt entry's `show` exits 1 with the reason on stderr.
#[test]
fn the_verb_reads_the_entries_of_a_directory_tier() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-entries");
    let hot = format!("dir:{}", dir.path().display());
    let clock = TestClock::new();
    let (code, out, err) = ran(&["entries", "ls", "--hot", &hot], &clock);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (status(0), "0 open entries\n", "")
    );
    let backend = DirBackend::open(dir.path()).unwrap();
    let put = backend.put(&entry_key("k"), Bytes::from_static(b"not json"));
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(put)
        .unwrap();
    let (code, out, _) = ran(&["entries", "ls", "--hot", &hot], &clock);
    assert_eq!(code, status(0));
    assert!(out.contains(" unreadable: "), "{out}");
    let (code, out, err) = ran(&["entries", "show", "--hot", &hot, "--key", "k"], &clock);
    assert_eq!((code, out.as_str()), (status(1), ""));
    assert!(err.starts_with("tierstore entries: "), "{err}");
    let (code, out, _) = ran(
        &["entries", "show", "--hot", &hot, "--key", "other"],
        &clock,
    );
    assert_eq!(code, status(0));
    assert!(out.starts_with("no open entry for key \"other\""), "{out}");
}

/// Asserts the statuses of a request that is not valid: no subcommand, `show` with no key, are 2;
/// help is 0 on stdout; a tier that cannot be opened is 1 with its message.
#[test]
fn flags_that_are_not_valid_are_status_2_and_help_is_status_0() {
    let clock = TestClock::new();
    assert_eq!(ran(&["entries"], &clock).0, status(2));
    assert_eq!(
        ran(&["entries", "show", "--hot", "memory"], &clock).0,
        status(2)
    );
    let (code, out, err) = ran(&["entries", "--help"], &clock);
    assert_eq!((code, err.as_str()), (status(0), ""));
    assert!(out.contains("Usage: tierstore entries"), "{out}");
    let (code, _, err) = ran(&["entries", "ls", "--hot", "s3:bucket"], &clock);
    assert_eq!(code, status(1));
    assert_eq!(
        err,
        "tierstore entries: --hot \"s3:bucket\": it needs --hot-endpoint\n"
    );
}
