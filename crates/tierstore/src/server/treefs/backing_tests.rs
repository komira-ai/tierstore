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

/// Asserts that a table which leaves the connection to its successor leaves the ids open in the
/// kernel: the ids stay open when the table and every open that held one are dropped, and a table
/// that does not abandon them closes each one.
#[test]
fn an_abandoned_table_leaves_its_ids_open_when_it_is_dropped() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-abandon");
    let kernel = FakeKernel::new();
    let (a, b) = (blob("a"), blob("b"));
    let lines = [
        Saved {
            blob: a,
            id: 7,
            opens: 1,
            exact: true,
        },
        Saved {
            blob: b,
            id: 8,
            opens: 0,
            exact: true,
        },
    ];
    let kept = BackingTable::adopt(dir.path().join(FILE), 10, &lines, &|id| kernel.wrap(id));
    let held = ticket(kept.open(&blob("c"))).registered(kernel.registration());
    kept.abandon();
    drop(held);
    drop(kept);
    assert_eq!(*locked(&kernel.closed), Vec::<u32>::new());

    let dropped = BackingTable::adopt(dir.path().join(FILE), 10, &lines, &|id| kernel.wrap(id));
    drop(dropped);
    let mut closed = locked(&kernel.closed).clone();
    closed.sort_unstable();
    assert_eq!(closed, [7, 8]);
}

/// Asserts that a table written exact and then written again without the mark has no line marked:
/// the counts of a process that serves on are a lower bound again.
#[test]
fn a_table_written_exact_is_written_again_without_the_mark() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-inexact");
    let table = table(&dir, 10);
    let kernel = FakeKernel::new();
    let a = blob("a");
    let _ = ticket(table.open(&a)).registered(kernel.registration());
    table.write_exact();
    assert_eq!(text(dir.path()), format!("{a} 100 1 exact\n"));
    assert_eq!(live(table.open(&a)), 100);
    table.write_inexact();
    assert_eq!(text(dir.path()), format!("{a} 100 2\n"));
}

/// A table adopted from `count` lines, each a blob with an id and no open: more text than a pipe
/// holds (64 KiB), so that a write into a FIFO stays in its write until the test reads the rest.
#[cfg(target_os = "linux")]
fn adopted(dir: &TestDir, kernel: &FakeKernel, count: u32) -> (Arc<BackingTable>, Vec<Saved>) {
    let lines: Vec<Saved> = (0..count)
        .map(|n| Saved {
            blob: blob(&n.to_string()),
            id: n + 1,
            opens: 0,
            exact: true,
        })
        .collect();
    let table = BackingTable::adopt(dir.path().join(FILE), 2 * count as usize, &lines, &|id| {
        kernel.wrap(id)
    });
    (table, lines)
}

/// The text of the file of a table whose blobs are `lines`, with no open.
#[cfg(target_os = "linux")]
fn file_of(lines: &[Saved]) -> String {
    let mut sorted: Vec<String> = lines
        .iter()
        .map(|line| format!("{} {} 0\n", line.blob, line.id))
        .collect();
    sorted.sort();
    sorted.concat()
}

/// Puts a FIFO where the table writes its next file, and gives the release of the one open of
/// `gone`'s blob, the last, which writes into it from a thread of its own; returns with that
/// thread inside the write (the test has read its first byte), and the FIFO's reading end.
#[cfg(target_os = "linux")]
fn release_into_fifo(
    dir: &TestDir,
    table: &Arc<BackingTable>,
    gone: &Saved,
) -> (std::thread::JoinHandle<()>, std::fs::File, u8) {
    use std::io::Read;
    let new = dir.path().join(format!("{FILE}.new"));
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    rustix::fs::mkfifoat(rustix::fs::CWD, &new, mode).unwrap();
    assert_eq!(live(table.open(&gone.blob)), gone.id);
    let releasing = {
        let (table, blob) = (Arc::clone(table), gone.blob);
        std::thread::spawn(move || table.release(&blob))
    };
    // Opening the reading end returns once the writer has opened the other.
    let mut fifo = std::fs::File::open(&new).unwrap();
    let mut first = [0_u8; 1];
    fifo.read_exact(&mut first).unwrap();
    (releasing, fifo, first[0])
}

/// The rest of the text in `fifo`, after `first`.
#[cfg(target_os = "linux")]
fn drained(mut fifo: std::fs::File, first: u8) -> String {
    use std::io::Read;
    let mut text = vec![first];
    fifo.read_to_end(&mut text).unwrap();
    String::from_utf8(text).unwrap()
}

/// Asserts that while a last release is writing the file, the table is not locked: an open of a
/// blob with a live id, and a release that is not the last, are answered at once and do not wait
/// for the write; and that the write holds the table without the released blob, whose id is closed.
/// Catches a table that writes its file with its lock held, which makes every open of the mount
/// wait for each first open's and last release's write.
#[cfg(target_os = "linux")]
#[test]
fn an_open_is_answered_while_a_release_writes_the_file() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-unlocked");
    let kernel = FakeKernel::new();
    let (table, lines) = adopted(&dir, &kernel, 1200);
    let (gone, rest) = lines.split_first().unwrap();
    let (releasing, fifo, first) = release_into_fifo(&dir, &table, gone);

    assert!(table.unlocked());
    let other = rest[0];
    assert_eq!(live(table.open(&other.blob)), other.id);
    assert_eq!(live(table.open(&other.blob)), other.id);
    table.release(&other.blob);
    assert_eq!(table.opens(&other.blob), 1);

    assert_eq!(drained(fifo, first), file_of(rest));
    releasing.join().unwrap();
    assert_eq!(*locked(&kernel.closed), [gone.id]);
    assert_eq!(table.live(), rest.len());
}

/// Asserts that last releases made while another release is writing the file are in the file
/// when they return: the write that was in progress holds the table as it was before them, and the
/// next one holds it without any of the three blobs. Both releases changed the table before the
/// write in progress ended, so the first of them to write takes both changes.
#[cfg(target_os = "linux")]
#[test]
fn releases_during_a_write_are_in_the_file_when_they_return() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-overlap");
    let kernel = FakeKernel::new();
    let (table, lines) = adopted(&dir, &kernel, 1200);
    let (gone, rest) = lines.split_first().unwrap();
    let (releasing, fifo, first) = release_into_fifo(&dir, &table, gone);

    let later: Vec<_> = rest[..2]
        .iter()
        .map(|line| {
            assert_eq!(live(table.open(&line.blob)), line.id);
            let (table, blob) = (Arc::clone(&table), line.blob);
            std::thread::spawn(move || table.release(&blob))
        })
        .collect();
    // Both have taken their blob out of the table, and wait for the write in progress.
    until(|| table.live() == rest.len() - 2);
    assert_eq!(drained(fifo, first), file_of(rest));
    releasing.join().unwrap();
    later.into_iter().for_each(|thread| thread.join().unwrap());
    assert_eq!(text(dir.path()), file_of(&rest[2..]));
    let mut closed = locked(&kernel.closed).clone();
    closed.sort_unstable();
    assert_eq!(closed, [gone.id, rest[0].id, rest[1].id]);
}

/// Asserts that an open that waits for another open's registration of the same blob is answered
/// with the registered id while that registration is still writing the file: it does not wait for
/// the write. Catches a registration that wakes the waiting opens only after its write.
#[cfg(target_os = "linux")]
#[test]
fn an_open_waiting_for_a_registration_is_answered_before_its_write() {
    use std::io::Read;
    let dir = TestDir::under(std::env::temp_dir(), "tfs-backing-wake");
    let kernel = FakeKernel::new();
    let (table, _) = adopted(&dir, &kernel, 1200);
    let a = blob("a");
    let first = ticket(table.open(&a));
    let waiter = {
        let table = Arc::clone(&table);
        std::thread::spawn(move || live(table.open(&a)))
    };
    until(|| table.waiting() == 1);

    let new = dir.path().join(format!("{FILE}.new"));
    let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
    rustix::fs::mkfifoat(rustix::fs::CWD, &new, mode).unwrap();
    let registration = kernel.registration();
    let id = registration.id;
    let registering = std::thread::spawn(move || first.registered(registration).id);
    // Opening the reading end returns once the writer has opened the other; the first byte read
    // means the registration is inside its write, which stays there until the rest is read.
    let mut fifo = std::fs::File::open(&new).unwrap();
    let mut first_byte = [0_u8; 1];
    fifo.read_exact(&mut first_byte).unwrap();

    until(|| waiter.is_finished());
    assert_eq!(waiter.join().unwrap(), id);
    assert_eq!(table.waiting(), 0);

    let _ = drained(fifo, first_byte[0]);
    assert_eq!(registering.join().unwrap(), id);
    assert_eq!(table.opens(&a), 2);
}
