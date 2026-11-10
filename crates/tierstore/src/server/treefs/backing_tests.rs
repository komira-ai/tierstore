//! The backing table: one id per blob while any open of it lives, the waits, and the file.

use std::path::Path;
use std::sync::Arc;

use tierstore_testkit::dir::TestDir;

use super::{BackingTable, FILE, Saved, Slot, saved};
use crate::server::treefs::locked;
use crate::server::treefs::testing::{FakeKernel, sha, until};
use crate::tree_manifest::TreeDigest;

fn blob(name: &str) -> TreeDigest {
    TreeDigest::from_hex(&sha(name.as_bytes())).unwrap()
}

fn table(dir: &TestDir, most: usize) -> Arc<BackingTable> {
    BackingTable::new(dir.path().join(FILE), most)
}

fn text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(FILE)).unwrap()
}

/// The ticket of a slot that must be one.
fn ticket(slot: Slot) -> super::Ticket {
    match slot {
        Slot::Register(ticket) => ticket,
        Slot::Live(backing) => panic!("a live id {} where a registration was due", backing.id),
        Slot::Spill => panic!("a cached open where a registration was due"),
    }
}

/// The id of a slot that must be live.
fn live(slot: Slot) -> u32 {
    match slot {
        Slot::Live(backing) => backing.id,
        Slot::Register(_) => panic!("a registration where a live id was due"),
        Slot::Spill => panic!("a cached open where a live id was due"),
    }
}

/// Asserts that the first open of a blob is told to register, that its id is the blob's for every
/// later open, in whatever order they come, and that the file names the blob, its id and its opens
/// at the first open.
#[test]
fn the_first_open_registers_and_every_later_open_shares_the_id() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-share");
    let table = table(&dir, 10);
    let kernel = FakeKernel::new();
    let a = blob("a");
    let first = ticket(table.open(&a)).registered(kernel.registration());
    assert_eq!(first.id, 100);
    assert_eq!(text(dir.path()), format!("{a} 100 1\n"));
    assert_eq!(live(table.open(&a)), 100);
    assert_eq!(live(table.open(&a)), 100);
    assert_eq!(table.opens(&a), 3);
    assert_eq!(table.live(), 1);
    // The file is written at a first open and a last release only.
    assert_eq!(text(dir.path()), format!("{a} 100 1\n"));
}

/// Asserts that the id is closed at the last release and not before, that the file loses the
/// blob then, and that an open after that registers a new id.
#[test]
fn the_last_release_closes_the_id_and_the_next_open_registers_again() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-release");
    let table = table(&dir, 10);
    let kernel = FakeKernel::new();
    let (a, b) = (blob("a"), blob("b"));
    let _ = ticket(table.open(&a)).registered(kernel.registration());
    let _ = ticket(table.open(&b)).registered(kernel.registration());
    assert_eq!(live(table.open(&a)), 100);
    table.release(&a);
    assert!(locked(&kernel.closed).is_empty());
    table.release(&a);
    assert_eq!(*locked(&kernel.closed), [100]);
    assert_eq!(text(dir.path()), format!("{b} 101 1\n"));
    assert_eq!(table.opens(&a), 0);
    assert_eq!(
        ticket(table.open(&a)).registered(kernel.registration()).id,
        102
    );
    // Two blobs are two ids, in the file by blob.
    let mut lines = [format!("{a} 102 1"), format!("{b} 101 1")];
    lines.sort();
    assert_eq!(text(dir.path()), format!("{}\n{}\n", lines[0], lines[1]));
}

/// Asserts that the release of a blob that has no id changes nothing.
#[test]
fn a_release_of_a_blob_with_no_id_changes_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-stray");
    let table = table(&dir, 10);
    table.release(&blob("a"));
    assert_eq!(table.live(), 0);
    assert_eq!(text(dir.path()), "");
}

/// Asserts that an open of a blob whose id is being registered waits, and then shares the id that
/// the registration made.
#[test]
fn an_open_waits_for_the_registration_of_the_same_blob_and_shares_its_id() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-wait-register");
    let table = table(&dir, 10);
    let kernel = FakeKernel::new();
    let a = blob("a");
    let held = ticket(table.open(&a));
    let waiter = {
        let table = Arc::clone(&table);
        std::thread::spawn(move || live(table.open(&a)))
    };
    until(|| table.waiting() == 1);
    let _ = held.registered(kernel.registration());
    assert_eq!(waiter.join().unwrap(), 100);
    assert_eq!(table.opens(&a), 2);
}

/// Asserts that a registration that is given up (the ticket dropped: the fill failed) lets the open
/// that waited for the blob register it itself.
#[test]
fn a_registration_given_up_lets_a_waiting_open_register() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-giveup");
    let table = table(&dir, 10);
    let a = blob("a");
    let held = ticket(table.open(&a));
    let waiter = {
        let table = Arc::clone(&table);
        std::thread::spawn(move || {
            let _ = ticket(table.open(&a));
        })
    };
    until(|| table.waiting() == 1);
    drop(held);
    waiter.join().unwrap();
    assert_eq!(table.live(), 0);
}

/// Asserts the limit: with `most` ids live an open of another blob does not wait (the release that
/// would end the wait needs a worker): it is a cached open, and so is every open of that blob until
/// the cached ones are released, while a blob that has an id still shares it. After that the blob
/// registers once an id is free.
#[test]
fn past_the_most_ids_an_open_is_cached_and_the_blob_stays_cached_while_such_opens_live() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-most");
    let table = table(&dir, 1);
    let kernel = FakeKernel::new();
    let (a, b) = (blob("a"), blob("b"));
    let _ = ticket(table.open(&a)).registered(kernel.registration());
    assert_eq!(live(table.open(&a)), 100);
    assert!(matches!(table.open(&b), Slot::Spill));
    assert_eq!(
        (table.live(), table.waiting(), table.spilled(&b)),
        (1, 0, 1)
    );
    table.release(&a);
    table.release(&a);
    assert_eq!(table.live(), 0);
    // An id is free now, but a cached open of the blob lives: no inode of it may go passthrough.
    assert!(matches!(table.open(&b), Slot::Spill));
    assert_eq!(table.spilled(&b), 2);
    table.release(&b);
    table.release(&b);
    assert_eq!(table.spilled(&b), 0);
    assert_eq!(
        ticket(table.open(&b)).registered(kernel.registration()).id,
        101
    );
    assert_eq!(text(dir.path()), format!("{b} 101 1\n"));
}

/// Asserts that a file that cannot be written does not stop the table.
#[test]
fn a_table_file_that_cannot_be_written_does_not_stop_the_table() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-nofile");
    let table = BackingTable::new(dir.path().join("no/such/dir").join(FILE), 10);
    let kernel = FakeKernel::new();
    let a = blob("a");
    let _ = ticket(table.open(&a)).registered(kernel.registration());
    assert_eq!(live(table.open(&a)), 100);
}

/// Asserts that the guard of an id has the type it was registered with, and no other.
#[test]
fn the_guard_of_an_id_is_had_by_its_type() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-guard");
    let table = table(&dir, 10);
    let backing = ticket(table.open(&blob("a"))).registered(super::Registered {
        id: 5,
        guard: Arc::new(String::from("guard")),
    });
    assert_eq!(backing.guard::<String>().map(String::as_str), Some("guard"));
    assert!(backing.guard::<u32>().is_none());
    assert_eq!(format!("{backing:?}"), "Backing { id: 5 }");
}

/// The table of two blobs, `a` and `b`, as a release wrote it at a clean stop: the fixture.
const FIXTURE: &str = include_str!("../../../tests/stored_formats/fixtures/fs_backing_table.txt");

/// The same with words a later release adds to a line, and a line that is nothing.
const NEWER: &str =
    include_str!("../../../tests/stored_formats/fixtures/fs_backing_table_newer.txt");

/// Asserts that the fixture reads to its two lines (one counted exactly), that a later release's
/// extra words are ignored, that a line that is not a line is left out, and that a missing file
/// is no lines.
#[test]
fn the_file_is_read_line_by_line_and_a_later_releases_words_are_ignored() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-saved");
    let expected = vec![
        Saved {
            blob: blob("b"),
            id: 101,
            opens: 1,
            exact: false,
        },
        Saved {
            blob: blob("a"),
            id: 100,
            opens: 2,
            exact: true,
        },
    ];
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), Vec::new());
    std::fs::write(dir.path().join(FILE), FIXTURE).unwrap();
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), expected);
    std::fs::write(dir.path().join(FILE), NEWER).unwrap();
    assert_eq!(saved(&dir.path().join(FILE)).unwrap(), expected);
    for bad in [
        "x 1 1",
        &format!("{} one 1", blob("a")),
        &format!("{} 1", blob("a")),
    ] {
        std::fs::write(dir.path().join(FILE), bad).unwrap();
        assert_eq!(saved(&dir.path().join(FILE)).unwrap(), Vec::new(), "{bad}");
    }
    // A path that is not a file is an error, not an empty table.
    assert!(saved(dir.path()).is_err());
}

/// Asserts that an adopted table written exactly gives back the bytes of the fixture, that a
/// table written between (a plain write) carries the counts but not the mark, and that the file of
/// a table that is adopted is written at once, as a table that is made is.
#[test]
fn an_adopted_table_is_written_back_as_the_fixture_has_it() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-adopted");
    std::fs::write(dir.path().join(FILE), FIXTURE).unwrap();
    let lines = saved(&dir.path().join(FILE)).unwrap();
    let kernel = FakeKernel::new();
    let table = BackingTable::adopt(dir.path().join(FILE), 10, &lines, &|id| kernel.wrap(id));
    assert_eq!(table.live(), 2);
    assert_eq!(text(dir.path()), FIXTURE.replace(" exact", ""));
    table.write_exact();
    assert_eq!(text(dir.path()), FIXTURE);
}

/// Asserts that the releases of an adopted blob's earlier opens close its id at the last, count
/// them down one by one, and do not close an id whose count was not exact; and that an open of this
/// process on the blob holds the id up.
#[test]
fn the_earlier_opens_of_an_adopted_blob_are_counted_down_to_the_close() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-foreign");
    std::fs::write(dir.path().join(FILE), FIXTURE).unwrap();
    let lines = saved(&dir.path().join(FILE)).unwrap();
    let kernel = FakeKernel::new();
    let table = BackingTable::adopt(dir.path().join(FILE), 10, &lines, &|id| kernel.wrap(id));
    let (a, b) = (blob("a"), blob("b"));
    assert_eq!(live(table.open(&a)), 100);
    table.release_foreign(&a);
    table.release_foreign(&a);
    assert!(locked(&kernel.closed).is_empty());
    table.release(&a);
    assert_eq!(*locked(&kernel.closed), [100]);
    assert_eq!(table.live(), 1);
    // The count of b was not exact: no number of releases closes it.
    table.release_foreign(&b);
    table.release_foreign(&b);
    assert_eq!(table.live(), 1);
    assert_eq!(live(table.open(&b)), 101);
    table.release(&b);
    assert_eq!(table.live(), 1);
    // A blob with no id, and a release for one, are nothing.
    table.release_foreign(&blob("c"));
    assert_eq!(text(dir.path()).lines().count(), 1);
}
