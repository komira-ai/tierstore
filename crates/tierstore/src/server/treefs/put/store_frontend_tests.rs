//! The calls of the staged-file store that a frontend makes beside those of a job: a name by its
//! number, the open handles and a handle taken over, which directories hold names, and the journal
//! written afresh.

use super::error::PutError;
use super::store::{Attr, Kind};
use super::testing::{Rig, START_MS, dir, staged, written};

/// Asserts that a name is found by its directory and name with its number, and by its number with
/// its directory and name; a listing gives each name with its number, in order; a name that is
/// not there, and a number that is no name's, are none.
#[test]
fn a_name_is_found_with_its_number_and_by_it() {
    let rig = Rig::new("put-store-find");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "b.part", b"bb");
    staged(&store, &out, "a.part", b"a");
    let (a, attr) = store.find(&out, "a.part").unwrap();
    let want = Attr {
        size: 1,
        ms: START_MS,
        kind: Kind::Staged,
    };
    assert_eq!(attr, want);
    assert_eq!(
        store.by_id(a),
        Some(("cas/b/1/n".to_owned(), "a.part".to_owned(), want))
    );
    let (b, _) = store.find(&out, "b.part").unwrap();
    let listed: Vec<(String, u64)> = store
        .list_numbered(&out)
        .into_iter()
        .map(|(name, id, _)| (name, id))
        .collect();
    assert_eq!(listed, [("a.part".to_owned(), a), ("b.part".to_owned(), b)]);
    assert_eq!(store.find(&out, "c.part"), None);
    assert_eq!(store.by_id(999), None);
    assert!(store.list_numbered(&dir("cas/b/2")).is_empty());
}

/// Asserts that a handle is taken over for a staged file: it writes, it counts as open (a commit
/// waits for it, here `EBUSY`), its release lets the commit go; and that a file not staged (a
/// number of no file) has no handle to take over. The open handles are listed with their files.
#[test]
fn a_handle_is_taken_over_for_a_staged_file_alone() {
    let rig = Rig::new("put-store-adopt");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    let first = written(&store, &out, "a.part", b"one");
    let (id, _) = store.find(&out, "a.part").unwrap();
    store.release(first);
    assert!(store.open_handles().is_empty());
    let adopted = store.adopt(id).unwrap();
    assert_eq!(store.open_handles(), vec![(adopted, id)]);
    assert_eq!(store.write(adopted, 3, b"two").unwrap(), 3);
    assert!(matches!(
        store.rename(&out, "a.part", "a"),
        Err(PutError::Busy(_))
    ));
    store.release(adopted);
    assert!(store.rename(&out, "a.part", "a").is_ok());
    assert!(matches!(store.adopt(999), Err(PutError::BadHandle)));
}

/// Asserts which directories hold names: the directory of a name holds it, a directory above it
/// holds it below, a directory that only shares the text of its path does not, and the list of
/// directories with names is in order.
#[test]
fn the_directories_that_hold_names_are_told() {
    let rig = Rig::new("put-store-holds");
    let store = rig.open();
    staged(&store, &dir("cas/b/x/y"), "a.part", b"a");
    staged(&store, &dir("cas/b/x-z"), "b.part", b"b");
    assert!(store.holds("cas/b/x/y"));
    assert!(!store.holds("cas/b/x"));
    assert!(store.holds_under("cas/b/x"));
    assert!(store.holds_under("cas/b/x/y"));
    assert!(store.holds_under("cas"));
    assert!(!store.holds_under("cas/b/x/y/z"));
    assert!(!store.holds_under("cas/b/w"));
    assert!(!store.holds_under("cas/b/x-"));
    assert_eq!(store.dirs(), ["cas/b/x-z", "cas/b/x/y"]);
}

/// Asserts that the journal written afresh holds the live names alone and is appended to from then
/// on, so a restart replays to the same names; and that a stage whose journal cannot be written
/// says so and keeps the journal it had.
#[test]
fn the_journal_is_written_afresh_with_the_live_names() {
    let rig = Rig::new("put-store-reopen");
    let store = rig.open();
    let out = dir("cas/b/1/n");
    staged(&store, &out, "gone.part", b"x");
    store.unlink(&out, "gone.part").unwrap();
    staged(&store, &out, "kept.part", b"kept");
    std::fs::remove_file(rig.stage().join("journal")).unwrap();
    store.reopen_journal().unwrap();
    let journal = rig.journal();
    assert_eq!(journal.lines().count(), 1, "{journal}");
    staged(&store, &out, "later.part", b"later");
    assert_eq!(rig.journal().lines().count(), 2);
    drop(store);
    let again = rig.open();
    assert!(again.find(&out, "kept.part").is_some());
    assert!(again.find(&out, "later.part").is_some());
    assert!(again.find(&out, "gone.part").is_none());
    std::fs::create_dir(rig.stage().join("journal.new")).unwrap();
    std::fs::write(rig.stage().join("journal.new").join("x"), "").unwrap();
    assert!(again.reopen_journal().is_err());
}
