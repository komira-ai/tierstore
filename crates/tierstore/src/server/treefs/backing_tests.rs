//! The backing table: one id per blob while any open of it lives, the waits, and the file.

use std::path::Path;
use std::sync::Arc;

use tierstore_testkit::dir::TestDir;

use super::{BackingTable, FILE, Slot};
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
    }
}

/// The id of a slot that must be live.
fn live(slot: Slot) -> u32 {
    match slot {
        Slot::Live(backing) => backing.id,
        Slot::Register(_) => panic!("a registration where a live id was due"),
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

/// Asserts the limit: with `most` ids live an open of another blob waits for a release, an open of
/// a blob that has an id does not, and the release lets the waiting open register.
#[test]
fn past_the_most_ids_an_open_waits_for_a_release() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-most");
    let table = table(&dir, 1);
    let kernel = FakeKernel::new();
    let (a, b) = (blob("a"), blob("b"));
    let _ = ticket(table.open(&a)).registered(kernel.registration());
    assert_eq!(live(table.open(&a)), 100);
    let waiter = {
        let table = Arc::clone(&table);
        std::thread::spawn(move || {
            let _ = ticket(table.open(&b)).registered(FakeKernel::new().registration());
        })
    };
    until(|| table.waiting() == 1);
    assert_eq!(table.live(), 1);
    table.release(&a);
    assert_eq!(table.waiting(), 1);
    table.release(&a);
    waiter.join().unwrap();
    assert_eq!((table.live(), table.opens(&b)), (1, 1));
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
