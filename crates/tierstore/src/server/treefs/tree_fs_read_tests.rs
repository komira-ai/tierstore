//! READ on the tree mount against a real tree (the rig of `tierstore-tree`) and no kernel: the
//! harness of `tree_fs_tests.rs` hands raw requests to [`TreeFs::handle`] and reads the replies
//! from the kernel's end of a socket pair. It proves:
//! - a READ of `present` cells answers the file's exact bytes, by `pread` below 16 KiB and by
//!   splice from it up, across a cell boundary, short at the end of the file, empty past it, empty
//!   for an empty file; `EISDIR` for a directory, `ENOENT` for an inode that is not an entry, `EIO`
//!   at once for a `bad` cell.
//! - a READ of cells not `present` is parked with no reply, answered with its bytes once the cells
//!   are published and the wait answered, answered once however many waits it makes, `EIO` when a
//!   wait answers `Eio`, answered to the request's own device.
//! - the lost path: a wait answered `lost` answers every parked read of the tree `EIO`, whatever
//!   pack it waits on, and from then on READ, STATFS, READDIR and READDIRPLUS are `EIO` while
//!   LOOKUP, GETATTR and READLINK still answer.
//! - an INTERRUPT of a parked READ answers `EINTR` once and nothing after the pack is answered.
//!
//! Catches: a splice reply with the wrong length or header, a short read at the end of a file
//! that is padded or dropped, a parked read answered twice or never, a lost pack that frees only
//! its own readers, a lost tree that still serves reads of cells it has, and an interrupted read
//! that answers again.

use tierstore_fetch_api::engine::WaitOutcome;
use tsfuse::{Handled, Interrupted, ReplyDevice};

use super::TreeFsStats;
use super::tree_fs_harness::*;

const EIO: i32 = 5;
const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EINTR: i32 = 4;

/// Reads `size` bytes at `offset` of `path` and returns the reply's bytes.
fn read_ok(h: &mut Harness, path: &str, offset: u64, size: u32) -> Vec<u8> {
    let node = h.resolve(path).nodeid;
    h.call(READ, node, &read_body(offset, size)).ok().to_vec()
}

/// Asserts that a READ of `present` cells answers exactly the file's bytes at the offset, below
/// the splice threshold (`pread`), across the boundary of two cells, and in the middle of a file.
#[test]
fn read_of_present_cells_below_the_splice_threshold_answers_the_exact_bytes() {
    let mut h = Harness::new("read-pread", &sample());
    h.publish_file("bin/cc");
    h.publish_file("lib/a.txt");
    let cc = h.content("bin/cc");
    assert_eq!(read_ok(&mut h, "bin/cc", 0, 4096), cc[..4096]);
    assert_eq!(read_ok(&mut h, "bin/cc", 1000, 5000), cc[1000..6000]);
    // Across the end of cell 0 into cell 1.
    let at = MIB - 100;
    assert_eq!(
        read_ok(&mut h, "bin/cc", at, 110),
        cc[at as usize..at as usize + 110]
    );
    assert_eq!(
        read_ok(&mut h, "lib/a.txt", 10, 20),
        h.content("lib/a.txt")[10..30]
    );
    let counts = h.fs.reply.counts();
    assert_eq!(
        counts.splice_bytes, 0,
        "below the threshold nothing is spliced"
    );
    assert_eq!(counts.pread_bytes, 4096 + 5000 + 110 + 20);
    assert_eq!(h.tree.stats().served, 4);
}

/// Asserts that a READ of 16 KiB and more answers the exact bytes by splice into the device,
/// within a cell and across two, and that the reply's header length is the data's.
#[test]
fn read_of_16_kib_and_more_is_spliced_with_the_exact_bytes() {
    let mut h = Harness::new("read-splice", &sample());
    h.publish_file("bin/cc");
    let cc = h.content("bin/cc");
    assert_eq!(read_ok(&mut h, "bin/cc", 0, 16 * 1024), cc[..16 * 1024]);
    assert_eq!(
        read_ok(&mut h, "bin/cc", 100_000, 64 * 1024),
        cc[100_000..100_000 + 64 * 1024]
    );
    // Across the end of cell 0 into cell 1, to the end of the file.
    let at = MIB - 20_000;
    assert_eq!(read_ok(&mut h, "bin/cc", at, 20_010), cc[at as usize..]);
    // 16 KiB less one is the last size that goes by pread.
    assert_eq!(
        read_ok(&mut h, "bin/cc", 7, 16 * 1024 - 1),
        cc[7..7 + 16 * 1024 - 1]
    );
    let counts = h.fs.reply.counts();
    assert_eq!(counts.splice_bytes, 16 * 1024 + 64 * 1024 + 20_010);
    assert_eq!(counts.pread_bytes, 16 * 1024 - 1);
}

/// Asserts that a READ that runs past the end of the file is cut to it, a READ at the end or past
/// it is empty, and a READ of an empty file is empty with no pack touched.
#[test]
fn read_at_the_end_is_short_and_past_it_empty() {
    let mut h = Harness::new("read-eof", &sample());
    h.publish_file("bin/ld");
    let ld = h.content("bin/ld");
    assert_eq!(ld.len() as u64, MIB);
    assert_eq!(
        read_ok(&mut h, "bin/ld", MIB - 100, 4096),
        ld[ld.len() - 100..]
    );
    // The same, over the splice threshold.
    assert_eq!(
        read_ok(&mut h, "bin/ld", MIB - 20_000, 64 * 1024),
        ld[ld.len() - 20_000..]
    );
    assert!(read_ok(&mut h, "bin/ld", MIB, 4096).is_empty());
    assert!(read_ok(&mut h, "bin/ld", MIB + 9999, 4096).is_empty());
    assert!(read_ok(&mut h, "bin/ld", 0, 0).is_empty());
    // No cell of the empty file, nor of lib/a.txt's pack, is present: no park, no wait.
    assert_eq!(
        read_ok(&mut h, "lib/empty", 0, 4096),
        h.content("lib/empty")
    );
    assert!(h.content("lib/empty").is_empty());
    assert_eq!(h.tree.stats().waits, 0);
    assert_eq!(h.tree.stats().parked, 0);
}

/// Asserts that a READ of a directory or of a link (neither is a file) is `EISDIR`, and of inode
/// 0 and of an inode past the last entry `ENOENT`.
#[test]
fn read_of_a_directory_or_a_missing_inode_is_refused() {
    let mut h = Harness::new("read-refused", &sample());
    let lib = h.resolve("lib").nodeid;
    let link = h.resolve("lib/self").nodeid;
    let past = h.tree.image().len() + 1;
    assert!(h.content("lib").is_empty(), "a directory has no bytes");
    assert_eq!(h.call(READ, ROOT, &read_body(0, 4096)).errno(), EISDIR);
    assert_eq!(h.call(READ, lib, &read_body(0, 4096)).errno(), EISDIR);
    assert_eq!(h.call(READ, link, &read_body(0, 4096)).errno(), EISDIR);
    for nodeid in [0, past, u64::MAX] {
        assert_eq!(
            h.call(READ, nodeid, &read_body(0, 4096)).errno(),
            ENOENT,
            "inode {nodeid}"
        );
    }
}

/// Asserts that a READ of a cell marked `bad` is `EIO` at once (not parked), while a file whose
/// cells are not covered by it still reads.
#[test]
fn read_of_a_bad_cell_is_eio_at_once() {
    let mut h = Harness::new("read-bad", &sample());
    h.rig.mark_bad(0, 0);
    h.publish_file("lib/a.txt");
    let node = h.resolve("bin/cc").nodeid;
    let msg = h.call(READ, node, &read_body(0, 4096));
    assert_eq!(msg.errno(), EIO);
    assert_eq!(h.tree.stats().parked, 0, "answered inline, not parked");
    assert_eq!(h.fs.stats(), TreeFsStats::default());
    assert_eq!(read_ok(&mut h, "lib/a.txt", 0, 60), h.content("lib/a.txt"));
}

/// Asserts that a READ of cells not `present` is parked with no reply, and once its cells are
/// published and the wait answered `Ready` it is answered with the file's bytes, counted as a
/// parked answer, and with nothing after.
#[test]
fn a_parked_read_is_answered_with_its_bytes_after_publish_and_ready() {
    let mut h = Harness::new("park-ready", &sample());
    let node = h.resolve("bin/cc").nodeid;
    let (unique, handled) = h.submit(READ, node, &read_body(4096, 8192));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(0, 1);
    assert!(!h.has_reply(), "no reply while the cell is absent");
    assert_eq!(h.fs.stats().parked_answered, 0);
    h.publish_file("bin/cc");
    assert_eq!(h.rig.answer(0, WaitOutcome::Ready), 1);
    let msg = h.recv();
    assert_eq!((msg.unique, msg.errno()), (unique, 0));
    assert_eq!(msg.payload, h.content("bin/cc")[4096..4096 + 8192]);
    h.until_answered(1);
    assert!(!h.has_reply(), "one reply");
    let stats = h.tree.stats();
    assert_eq!((stats.parked, stats.served, stats.waits), (1, 1, 1));
}

/// Asserts that a parked READ spliced on its second try is answered with the exact bytes.
#[test]
fn a_parked_read_over_the_splice_threshold_is_answered_by_splice() {
    let mut h = Harness::new("park-splice", &sample());
    let node = h.resolve("bin/ld").nodeid;
    let (unique, handled) = h.submit(READ, node, &read_body(0, 100_000));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(0, 1);
    h.publish_file("bin/ld");
    h.rig.answer(0, WaitOutcome::Ready);
    let msg = h.recv();
    assert_eq!((msg.unique, msg.errno()), (unique, 0));
    assert_eq!(msg.payload, h.content("bin/ld")[..100_000]);
    assert_eq!(h.fs.reply.counts().splice_bytes, 100_000);
}

/// Asserts that a READ whose wait is answered `Ready` before its cells are published parks again
/// and is answered once, with its bytes, when they are.
#[test]
fn a_read_that_waits_twice_is_answered_once() {
    let mut h = Harness::new("park-twice", &sample());
    let node = h.resolve("lib/a.txt").nodeid;
    let (unique, handled) = h.submit(READ, node, &read_body(0, 60));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(1, 1);
    // Ready with the cell still absent: the read parks again.
    assert_eq!(h.rig.answer(1, WaitOutcome::Ready), 1);
    h.rig.until_parked(1, 1);
    assert_eq!(h.tree.stats().waits, 2);
    assert!(!h.has_reply());
    h.publish_file("lib/a.txt");
    assert_eq!(h.rig.answer(1, WaitOutcome::Ready), 1);
    let msg = h.recv();
    assert_eq!((msg.unique, msg.errno()), (unique, 0));
    assert_eq!(msg.payload, h.content("lib/a.txt"));
    h.until_answered(1);
    assert!(!h.has_reply(), "answered once");
}

/// Asserts that a parked READ whose wait is answered `Eio` is answered `EIO`, and the tree is not
/// lost by it: another file of the pack with its cells present still reads.
#[test]
fn a_parked_read_whose_wait_fails_is_eio_without_losing_the_tree() {
    let mut h = Harness::new("park-eio", &sample());
    let node = h.resolve("bin/cc").nodeid;
    let (unique, handled) = h.submit(READ, node, &read_body(0, 4096));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(0, 1);
    assert_eq!(h.rig.answer(0, WaitOutcome::Eio { cell: 0 }), 1);
    let msg = h.recv();
    assert_eq!((msg.unique, msg.errno()), (unique, EIO));
    h.until_answered(1);
    assert!(!h.tree.is_lost());
    h.publish_file("lib/a.txt");
    assert_eq!(read_ok(&mut h, "lib/a.txt", 0, 60), h.content("lib/a.txt"));
}

/// Asserts that a parked READ is answered on the device its request came from, not on another
/// one the mount has served.
#[test]
fn a_parked_read_is_answered_on_its_own_device() {
    let mut h = Harness::new("park-device", &sample());
    let (other, other_kernel) = device();
    let node = h.resolve("lib/a.txt").nodeid;
    let device: std::sync::Arc<dyn ReplyDevice> = other.clone();
    let (unique, handled) = h.submit_on(device, READ, node, &read_body(0, 60));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(1, 1);
    h.publish_file("lib/a.txt");
    h.rig.answer(1, WaitOutcome::Ready);
    let msg = recv_from(&other_kernel);
    assert_eq!((msg.unique, msg.errno()), (unique, 0));
    assert_eq!(msg.payload, h.content("lib/a.txt"));
    h.until_answered(1);
    assert!(!h.has_reply(), "nothing on the harness's own device");
}

/// Asserts the lost path. Two READs parked on files of two packs; the first pack's wait is
/// answered `Lost`: both are answered `EIO` (the second without its pack being answered); from
/// then on a READ even of a file whose cells are present, STATFS, READDIR and READDIRPLUS of the
/// root and of a subdirectory are `EIO`; LOOKUP, GETATTR and READLINK still answer from the image.
#[test]
fn a_lost_wait_answers_every_parked_read_eio_and_every_later_read_and_listing_too() {
    let mut h = Harness::new("lost", &sample());
    let cc = h.resolve("bin/cc").nodeid;
    let a_txt = h.resolve("lib/a.txt").nodeid;
    let lib = h.resolve("lib").nodeid;
    let link = h.resolve("lib/self").nodeid;
    let (u0, handled) = h.submit(READ, cc, &read_body(0, 4096));
    assert!(matches!(handled, Handled::Parked));
    let (u1, handled) = h.submit(READ, a_txt, &read_body(0, 60));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(0, 1);
    h.rig.until_parked(1, 1);
    assert!(!h.tree.is_lost());
    assert_eq!(h.rig.answer(0, WaitOutcome::Lost), 1);
    let mut got = [h.recv(), h.recv()];
    got.sort_by_key(|m| m.unique);
    assert_eq!((got[0].unique, got[0].errno()), (u0, EIO));
    assert_eq!((got[1].unique, got[1].errno()), (u1, EIO));
    h.until_answered(2);
    assert_eq!(h.rig.parked(1), 1, "the second pack was not answered");
    assert!(h.tree.stats().lost);

    // A file whose cells are present is not read any more.
    h.publish_file("lib/a.txt");
    assert_eq!(h.call(READ, a_txt, &read_body(0, 60)).errno(), EIO);
    assert_eq!(h.call(READ, cc, &read_body(0, 4096)).errno(), EIO);
    assert_eq!(h.call(READ, ROOT, &read_body(0, 4096)).errno(), EIO);
    assert_eq!(h.call(STATFS, ROOT, &[]).errno(), EIO);
    for dir in [ROOT, lib] {
        assert_eq!(
            h.call(READDIR, dir, &read_body(0, 4096)).errno(),
            EIO,
            "READDIR of {dir}"
        );
        assert_eq!(
            h.call(READDIRPLUS, dir, &read_body(0, 4096)).errno(),
            EIO,
            "READDIRPLUS of {dir}"
        );
    }
    // The image is intact: names, attributes and links still answer.
    let looked = h.lookup(lib, "a.txt").entry();
    assert_eq!(looked.nodeid, a_txt);
    assert_eq!(h.lookup(lib, "missing").entry().nodeid, 0);
    assert_eq!(h.getattr(a_txt).attr().0.size, 60);
    assert_eq!(h.getattr(ROOT).errno(), 0);
    assert_eq!(h.call(READLINK, link, &[]).ok(), b"a.txt");
    assert_eq!(h.call(GETATTR, cc, &[0; 16]).errno(), 0);
    assert_eq!(h.call(LOOKUP, ROOT, b"lib\0").errno(), 0);
}

/// Asserts that a lost tree answers a READ that was never parked `EIO` too, and that a wait that
/// starts after the loss is answered at once.
#[test]
fn a_read_parked_after_the_tree_was_lost_is_never_parked() {
    let mut h = Harness::new("lost-first", &sample());
    let cc = h.resolve("bin/cc").nodeid;
    let (u0, handled) = h.submit(READ, cc, &read_body(0, 4096));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(0, 1);
    h.rig.answer(0, WaitOutcome::Lost);
    assert_eq!(h.recv().unique, u0);
    // Cells of the other pack are absent, but the tree is lost: EIO inline, not parked.
    let a_txt = h.resolve("lib/a.txt").nodeid;
    let msg = h.call(READ, a_txt, &read_body(0, 60));
    assert_eq!(msg.errno(), EIO);
    assert_eq!(h.rig.parked(1), 0);
}

/// Asserts that an INTERRUPT of a parked READ answers `EINTR` once, and that nothing more is
/// written when its pack is answered afterwards: the next reply on the device is the next
/// request's.
#[test]
fn an_interrupted_parked_read_gets_eintr_once_and_no_second_reply() {
    let mut h = Harness::new("interrupt", &sample());
    let node = h.resolve("lib/a.txt").nodeid;
    let (unique, handled) = h.submit(READ, node, &read_body(0, 60));
    assert!(matches!(handled, Handled::Parked));
    h.rig.until_parked(1, 1);
    let outcome = h.tokens.interrupt(unique, 9999, &*h.dev);
    assert!(matches!(outcome, Interrupted::Cancelled));
    let msg = h.recv();
    assert_eq!((msg.unique, msg.errno()), (unique, EINTR));
    // The parked task drops its wait: its count of waits in flight returns to 0.
    let waiting = || {
        std::thread::yield_now();
        h.tree.stats().waiting
    };
    let _ = std::iter::repeat_with(waiting).find(|&n| n == 0);
    h.publish_file("lib/a.txt");
    h.rig.answer(1, WaitOutcome::Ready);
    // Replies are in order on the device: the next one is this LOOKUP's, not a late READ's.
    let next = h.lookup(ROOT, "lib");
    assert_eq!(next.errno(), 0);
    assert!(next.payload.len() > 16);
    assert!(!h.has_reply());
    assert_eq!(
        h.fs.stats().parked_answered,
        0,
        "an interrupted read is not an answered one"
    );
}
