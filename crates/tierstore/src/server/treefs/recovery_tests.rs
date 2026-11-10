//! A process that takes a connection over, with the kernel's end of `/dev/fuse` played by the
//! test (`fakedev.rs`): no mount, no privilege. The connection was set up by an earlier process,
//! which left its state in the runtime directory.

use fuser::InitFlags;

use super::backing;
use super::config::FsSettings;
use super::fakedev::{self, Message};
use super::init_record::{self, Record, RecordError};
use super::ino;
use super::run::{Prepared, Running, StartError};
use super::testing::{FakeKernel, Rig, sample, sha};

/// The rig and its settings, with the record the process that mounted left.
fn left_behind(name: &str, resend: bool) -> (Rig, FsSettings) {
    let rig = Rig::new(name, sample());
    let record = Record::first(fakedev::outcome(resend), "mount-1".to_owned());
    init_record::write(&rig.runtime, &record).unwrap();
    let settings = rig.settings();
    (rig, settings)
}

/// A process over the connection of `device`.
fn adopt(settings: &FsSettings, device: std::os::fd::OwnedFd, resend: bool) -> Running {
    let prepared = Prepared::load(settings).unwrap();
    Running::adopt(settings, prepared, device, (None, resend)).unwrap()
}

/// Asserts that a reply answers the request `unique` with success.
fn answered(message: &Message, unique: u64) {
    assert_eq!((message.unique, message.error), (unique, 0), "{message:?}");
}

/// Asserts that a process that takes a connection over serves the request that was queued while
/// no one read, with no handshake: the kernel sent `INIT` once and it was not sent again.
#[test]
fn a_takeover_serves_what_was_queued_with_no_handshake() {
    let (_rig, settings) = left_behind("tfs-rec-queued", false);
    let (device, kernel) = fakedev::pair();
    kernel.getattr(5, ino::ROOT);
    let running = adopt(&settings, device, false);
    answered(&kernel.reply(), 5);
    kernel.lookup(6, ino::ROOT, "trees");
    answered(&kernel.reply(), 6);
    drop(running);
}

/// Asserts that a takeover records itself as the next generation, going on with the mount id of the
/// mount, and keeps the outcome of the handshake.
#[test]
fn a_takeover_records_the_next_generation_and_keeps_the_mount_id() {
    let (rig, settings) = left_behind("tfs-rec-generation", false);
    let (device, _kernel) = fakedev::pair();
    let running = adopt(&settings, device, false);
    let record = init_record::read(&rig.runtime).unwrap();
    assert_eq!(record.generation, 2);
    assert_eq!(record.mount_id, "mount-1");
    assert_eq!(record.outcome, fakedev::outcome(false));
    assert_eq!(running.outcome(), fakedev::outcome(false));
    let (device, _kernel) = fakedev::pair();
    let again = adopt(&settings, device, false);
    assert_eq!(init_record::read(&rig.runtime).unwrap().generation, 3);
    drop(again);
}

/// Asserts the resend: where the connection was set up with `FUSE_NOTIFY_RESEND` and the takeover
/// follows a crash, the kernel is sent the notification (code 7, the header alone); where the
/// connection was not, or the takeover follows a hand-over, nothing is sent.
#[test]
fn a_takeover_asks_for_the_requests_again_only_where_the_kernel_can_send_them() {
    let (_rig, settings) = left_behind("tfs-rec-resend", true);
    let (device, kernel) = fakedev::pair();
    let running = adopt(&settings, device, true);
    let note = kernel.reply();
    assert_eq!((note.unique, note.error, note.body.len()), (0, 7, 0));
    assert!(running.outcome().flags.contains(InitFlags::FUSE_HAS_RESEND));
    // A request the kernel sent again carries the resend bit and is answered under its id.
    let again = 9 | (1 << 63);
    kernel.getattr(again, ino::ROOT);
    answered(&kernel.reply(), again);
    drop(running);

    let (device, kernel) = fakedev::pair();
    let after_handover = adopt(&settings, device, false);
    kernel.assert_silent();
    drop(after_handover);

    let (_rig, settings) = left_behind("tfs-rec-no-resend", false);
    let (device, kernel) = fakedev::pair();
    let no_resend = adopt(&settings, device, true);
    kernel.assert_silent();
    drop(no_resend);
}

/// Asserts that a notification the kernel refuses is only said: the takeover goes on.
#[test]
fn a_refused_resend_is_said_and_the_takeover_goes_on() {
    let (_rig, settings) = left_behind("tfs-rec-refused", true);
    let (device, kernel) = fakedev::pair();
    // The kernel's end is gone, so the notification cannot be written.
    drop(kernel);
    let running = adopt(&settings, device, true);
    assert!(running.resend_failed());
    drop(running);
}

/// Asserts the order of a takeover after a crash: the kernel is asked to send the requests again
/// before any reader starts, so that a request the new readers have taken is not put back and run
/// twice. The first thing the kernel hears is the notification, and a request that was queued is
/// answered once, after it.
#[test]
fn the_kernel_is_asked_to_send_requests_again_before_the_readers_start() {
    let (_rig, settings) = left_behind("tfs-rec-resend-first", true);
    let (device, kernel) = fakedev::pair();
    kernel.getattr(5, ino::ROOT);
    let running = adopt(&settings, device, true);
    let note = kernel.reply();
    assert_eq!((note.unique, note.error), (0, 7));
    answered(&kernel.reply(), 5);
    kernel.assert_silent();
    assert!(!running.resend_failed());
    drop(running);
}

/// Asserts the pause of a hand-over: after `quiesce` no request is read (a request that comes waits
/// for the next reader), and `resume` reads again and answers what waited.
#[test]
fn a_quiesced_process_reads_nothing_and_a_resumed_one_answers_what_waited() {
    let (_rig, settings) = left_behind("tfs-rec-quiesce", false);
    let (device, kernel) = fakedev::pair();
    let running = adopt(&settings, device, false);
    kernel.getattr(1, ino::ROOT);
    answered(&kernel.reply(), 1);
    running.quiesce();
    kernel.getattr(2, ino::ROOT);
    kernel.assert_silent();
    running.resume().unwrap();
    answered(&kernel.reply(), 2);
    kernel.getattr(3, ino::ROOT);
    answered(&kernel.reply(), 3);
}

/// Asserts that the descriptor and the control socket a hand-over offers are copies of the process's
/// own: the descriptor is the same connection.
#[test]
fn the_offer_of_a_hand_over_is_copies_of_the_connection_and_the_control_socket() {
    let (_rig, settings) = left_behind("tfs-rec-share", false);
    let (device, kernel) = fakedev::pair();
    let running = adopt(&settings, device, false);
    let (copy, listener) = running.share().unwrap();
    assert!(std::os::unix::net::UnixStream::connect(&settings.control_socket).is_ok());
    drop(listener);
    // The process quiesces; a successor serving the copy answers what waits.
    running.quiesce();
    kernel.getattr(4, ino::ROOT);
    let next = adopt(&settings, copy, false);
    answered(&kernel.reply(), 4);
    drop(next);
    running.give_up();
}

/// Asserts the handles of a process before: a file open in it is read and released through the
/// next, which makes the open again from the inode.
#[test]
fn a_file_open_across_a_takeover_is_read_and_released_by_the_next_process() {
    let (rig, settings) = left_behind("tfs-rec-handles", false);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let (device, kernel) = fakedev::pair();
    let first = adopt(&settings, device, false);
    let _ = first.core().attach(&root, "run-1").unwrap();
    let header = ino::of(1, 7);
    kernel.open(1, header);
    let opened = kernel.reply();
    answered(&opened, 1);
    let fh = opened.handle();
    kernel.read(2, header, (fh, 0, 6));
    let first_read = kernel.reply();
    assert_eq!(first_read.body, b"int a;");
    first.quiesce();
    let copy = first.device().unwrap();
    drop(first);

    // The next process knows no such handle; the first read makes the open again.
    let next = adopt(&settings, copy, false);
    assert_eq!(next.core().open_files(), 0);
    kernel.read(3, header, (fh, 2, 100));
    let second_read = kernel.reply();
    answered(&second_read, 3);
    assert_eq!(second_read.body, b"t a;");
    assert_eq!(next.core().open_files(), 1);
    kernel.release(4, header, fh);
    answered(&kernel.reply(), 4);
    assert_eq!(next.core().open_files(), 0);
}

/// Asserts that what a takeover cannot read is an error that names it, and nothing is served: no
/// record, a table of another layout, a cache that is not there.
#[test]
fn a_takeover_that_cannot_read_the_state_says_what_and_serves_nothing() {
    let rig = Rig::new("tfs-rec-nostate", sample());
    let settings = rig.settings();
    let error = Prepared::load(&settings).err().unwrap();
    assert!(matches!(error, StartError::Record(RecordError::Io { .. })));
    assert!(error.to_string().contains("/init"), "{error}");
    init_record::write(
        &rig.runtime,
        &Record::first(fakedev::outcome(false), "m".to_owned()),
    )
    .unwrap();
    std::fs::write(rig.runtime.join("trees"), b"not a table").unwrap();
    let error = Prepared::load(&settings).err().unwrap();
    assert!(matches!(error, StartError::Table(_)));
    std::fs::remove_file(rig.runtime.join("trees")).unwrap();
    let error = Prepared::load(&settings).err().unwrap();
    assert!(matches!(error, StartError::Table(_)));

    let (rig, settings) = left_behind("tfs-rec-nocache", false);
    let prepared = Prepared::load(&settings).unwrap();
    let (device, _kernel) = fakedev::pair();
    let lost = FsSettings {
        cache_path: rig.dir.path().join("none"),
        ..settings
    };
    let error = Running::adopt(&lost, prepared, device, (None, false))
        .err()
        .unwrap();
    assert!(error.to_string().contains("fs.cache_path"), "{error}");
}

/// Asserts that the table of backing ids the process before wrote is adopted where the connection
/// has passthrough, and left alone where it does not.
#[test]
fn the_backing_ids_of_the_process_before_are_adopted_where_the_connection_has_passthrough() {
    let (rig, settings) = left_behind("tfs-rec-backing", false);
    let table = format!("{} 7 2 exact\n{} 8 1\n", sha(b"tool bytes"), sha(b"int a;"));
    std::fs::write(rig.runtime.join("backing"), &table).unwrap();
    let (device, _kernel) = fakedev::pair();
    let plain = adopt(&settings, device, false);
    assert!(plain.core().backing_table().is_none());
    drop(plain);

    let mut with = Record::first(fakedev::outcome(false), "mount-1".to_owned());
    with.outcome.flags |= InitFlags::FUSE_PASSTHROUGH;
    with.outcome.capabilities |= InitFlags::FUSE_PASSTHROUGH;
    with.outcome.max_stack_depth = 1;
    init_record::write(&rig.runtime, &with).unwrap();
    let (device, _kernel) = fakedev::pair();
    let backed = adopt(&settings, device, false);
    assert_eq!(backed.core().backing_table().unwrap().live(), 2);
    // The table was written again as it was read.
    let kept = std::fs::read_to_string(rig.runtime.join("backing")).unwrap();
    assert_eq!(kept.lines().count(), 2);
    assert!(kept.contains(" 7 2"), "{kept}");
}

/// The process of `left_behind`, over a connection that has passthrough with the ids of `table`
/// adopted, and the file of the ids as the process before left it.
fn with_ids(name: &str, table: &str) -> (Running, Rig, FakeKernel) {
    let (rig, settings) = left_behind(name, false);
    std::fs::write(rig.runtime.join("backing"), table).unwrap();
    let (device, _kernel) = fakedev::pair();
    let running = adopt(&settings, device, false);
    let kernel = FakeKernel::new();
    let saved = backing::saved(&rig.runtime.join("backing")).unwrap();
    running
        .core()
        .adopt_passthrough(&saved, &|id| kernel.wrap(id));
    (running, rig, kernel)
}

/// Asserts that a process that leaves the connection at a signal writes the counts of the backing
/// table exact (so that its successor closes the ids when the last open of each is released), and
/// closes no id.
#[test]
fn a_process_that_leaves_the_connection_writes_the_counts_exact_and_closes_no_id() {
    let table = format!("{} 7 2 exact\n", sha(b"tool bytes"));
    let (running, rig, kernel) = with_ids("tfs-rec-leave", &table);
    running.leave();
    let kept = std::fs::read_to_string(rig.runtime.join("backing")).unwrap();
    assert_eq!(kept, table);
    assert!(super::locked(&kernel.closed).is_empty());
}

/// Asserts that a hand-over which does not complete, after the counts were written exact, writes
/// the table again with no mark: the process serves on, and its successor after a crash must not
/// trust the counts.
#[test]
fn a_hand_over_that_does_not_complete_leaves_the_counts_unmarked() {
    let table = format!("{} 7 2 exact\n", sha(b"tool bytes"));
    let (running, rig, _kernel) = with_ids("tfs-rec-resumed", &table);
    running.quiesce();
    let paused = std::fs::read_to_string(rig.runtime.join("backing")).unwrap();
    assert!(paused.ends_with(" exact\n"), "{paused}");
    running.resume().unwrap();
    let kept = std::fs::read_to_string(rig.runtime.join("backing")).unwrap();
    assert!(!kept.contains("exact"), "{kept}");
}

/// Asserts that a takeover asks nothing of the mount point: it is the live mount, whose root the
/// kernel does not cache, so a `stat` or a `mkdir` of it is a request to the process that is not
/// serving yet. The mount point here is a path under a file, which every call that looks at it
/// refuses; a takeover that serves the connection never looks.
#[test]
fn a_takeover_never_looks_at_the_mount_point() {
    let (rig, settings) = left_behind("tfs-rec-nomount", false);
    let file = rig.dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let settings = FsSettings {
        mountpoint: file.join("mnt"),
        ..settings
    };
    let (device, kernel) = fakedev::pair();
    kernel.getattr(5, ino::ROOT);
    let running = adopt(&settings, device, false);
    answered(&kernel.reply(), 5);
    assert!(!settings.mountpoint.exists());
    assert!(!file.join("mnt").is_dir());
    drop(running);
}
