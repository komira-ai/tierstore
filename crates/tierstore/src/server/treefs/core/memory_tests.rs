//! The trees in memory: one read of a tree for every request that asks for it at the same time,
//! the lookups counted with no lock, the uncounted trees and the sweep that lets
//! them go, and the line a read says.

use std::sync::Arc;
use std::time::Duration;

use super::super::{Core, Errno, Looked};
use super::{SLOW_READ_MS, STOPPED, said};
use crate::server::local_cache::manifest_path;
use crate::server::treefs::core::blob::SAY_EVERY_MS;
use crate::server::treefs::core::placed::Unread;
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Logged, Rig, TREE_IDLE, logged, sample, until};
use std::sync::atomic::Ordering;

const RUN: &str = "run-1";

/// The inode of the root of the tree numbered 1.
fn root_ino() -> u64 {
    ino::of(1, 0)
}

/// A rig over the sample tree with `RUN` attached (tree number 1), and the inode of `bin` below
/// its root, looked up once.
fn attached(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    let bin = found(&rig.core, root_ino(), "bin");
    (rig, bin)
}

fn found(core: &Core, parent: u64, name: &str) -> u64 {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{name}: {other:?}"),
    }
}

/// Takes the tree of `attached` out of memory, counted exactly: the run detached and the one
/// lookup of `bin` forgotten. The manifests are taken out of the cache too, so that the next read
/// asks the fill socket for each, and the test can hold it there.
fn out_of_memory(rig: &Rig, bin: u64) {
    assert!(rig.core.detach(RUN).is_some());
    rig.core.forget(bin, 1);
    assert_eq!(rig.core.trees_in_memory(), 0);
    let manifests: Vec<String> = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .manifests
        .keys()
        .cloned()
        .collect();
    for sha in manifests {
        std::fs::remove_file(manifest_path(&rig.cache, &sha)).unwrap();
    }
}

/// Asks for the attributes of `ino` from `threads` threads at once.
fn ask_from(
    rig: &Rig,
    threads: usize,
    ino: u64,
) -> Vec<std::thread::JoinHandle<Result<u64, Errno>>> {
    (0..threads)
        .map(|_| {
            let core = Arc::clone(&rig.core);
            std::thread::spawn(move || core.getattr(ino).map(|entry| entry.attr.ino))
        })
        .collect()
}

/// The fills of the root manifest of the sample tree the fill socket was asked for.
fn root_fills(rig: &Rig) -> usize {
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let line = format!("fill {root}");
    rig.upstream
        .asked("fill")
        .iter()
        .filter(|asked| **asked == line)
        .count()
}

/// Asserts that requests that name inodes of a tree that left memory, all at once, make one read
/// of the tree between them: the first reads it, the others wait for that read and are answered
/// from it, and the fill socket is asked for the root manifest once. Catches a core that reads a
/// tree once for each request, which kept every FUSE thread flattening the same tree.
#[test]
fn requests_at_once_for_a_tree_out_of_memory_read_it_once() {
    let (rig, bin) = attached("tfs-memory-once");
    out_of_memory(&rig, bin);
    let before = root_fills(&rig);
    let fills = rig.upstream.asked("fill").len();
    rig.upstream.gate_manifests();
    let asking = ask_from(&rig, 4, bin);
    // The first read is at the fill socket, held there (the gate counts every fill of a
    // manifest, those of the attach too); the three other requests wait for it.
    rig.upstream.wait_for_manifest_fills(fills + 1);
    until(|| rig.core.waiting_for_reads() == 3);
    rig.upstream.permit_manifests(1_000);
    for asked in asking {
        assert_eq!(asked.join().unwrap(), Ok(bin));
    }
    assert_eq!(root_fills(&rig), before + 1);
    // The one read asks for the two directories below the root, `bin` and `include`, at once
    // (a level of the tree at a time); four reads would have had four roots in flight.
    assert!(rig.upstream.most_manifest_fills() <= 2);
    assert_eq!(rig.core.waiting_for_reads(), 0);
    // Read at the attach and once again now.
    assert_eq!(rig.core.times_read(1), 2);
    assert_eq!(rig.core.trees_in_memory(), 1);
}

/// Asserts that requests that waited for a read that failed get its failure (`EIO`) without a
/// read of their own, and that a failed read is not kept: the next request reads the tree again.
#[test]
fn requests_that_waited_for_a_failed_read_fail_with_it_and_the_next_reads_again() {
    let (rig, bin) = attached("tfs-memory-failed");
    out_of_memory(&rig, bin);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.upstream
        .failing
        .lock()
        .unwrap()
        .insert(root, "upstream 503".to_owned());
    let before = root_fills(&rig);
    let fills = rig.upstream.asked("fill").len();
    rig.upstream.gate_manifests();
    let asking = ask_from(&rig, 3, bin);
    rig.upstream.wait_for_manifest_fills(fills + 1);
    until(|| rig.core.waiting_for_reads() == 2);
    rig.upstream.permit_manifests(1_000);
    for asked in asking {
        assert_eq!(asked.join().unwrap(), Err(Errno::Io));
    }
    assert_eq!(root_fills(&rig), before + 1);
    assert_eq!(rig.core.trees_in_memory(), 0);
    rig.upstream.failing.lock().unwrap().clear();
    assert_eq!(rig.core.getattr(bin).unwrap().attr.ino, bin);
    assert_eq!(root_fills(&rig), before + 2);
}

/// Asserts that a lookup counted on a tree that left memory after the caller took it puts the
/// tree back, with the lookup: the forget of that lookup is what takes it out again. Catches a
/// count raised on a tree no longer in memory, which lost the lookup, so that a later forget took
/// the tree out while the kernel held its inodes.
#[test]
fn a_lookup_counted_on_a_tree_that_just_left_memory_puts_it_back() {
    let (rig, bin) = attached("tfs-memory-count");
    let core = &rig.core;
    let taken = core.tree(1).unwrap();
    assert!(core.detach(RUN).is_some());
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 0);
    core.count_lookup(1, &taken);
    assert_eq!(core.trees_in_memory(), 1);
    assert_eq!(core.getattr(bin).unwrap().attr.ino, bin);
    assert_eq!(
        core.times_read(1),
        1,
        "the tree came back from the lookup, not a read"
    );
    core.forget(bin, 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts that a forget of more lookups than were counted (the kernel holds inodes this process
/// did not count, as after a restart) keeps the tree in memory through every later forget, and
/// that the sweep for idle trees takes it out once no request has named it for
/// `fs.tree_idle_secs` and nothing has it attached, and not before.
#[test]
fn an_uncounted_tree_stays_until_it_is_idle_and_unattached() {
    let (rig, bin) = attached("tfs-memory-uncounted");
    let core = &rig.core;
    core.forget(bin, 5);
    assert!(core.detach(RUN).is_some());
    core.forget(bin, 1);
    core.forget(root_ino(), 1);
    assert_eq!(core.trees_in_memory(), 1);
    rig.clock.advance(TREE_IDLE - Duration::from_millis(1));
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 1);
    // A request names it: idle again from now.
    assert_eq!(core.getattr(bin).unwrap().attr.ino, bin);
    rig.clock.advance(TREE_IDLE - Duration::from_millis(1));
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 1);
    rig.clock.advance(Duration::from_millis(1));
    let _ = core.detach_idle();
    assert_eq!(core.trees_in_memory(), 0);
    assert_eq!(core.times_read(1), 1);
}

/// Asserts that an uncounted tree that a run has attached is not taken out by the sweep, however
/// idle it is.
#[test]
fn an_uncounted_tree_that_is_attached_stays() {
    let (rig, bin) = attached("tfs-memory-uncounted-run");
    let core = &rig.core;
    core.forget(bin, 2);
    rig.clock.advance(TREE_IDLE * 2);
    assert_eq!(core.drop_idle_uncounted(1), 0);
    assert_eq!(core.trees_in_memory(), 1);
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.drop_idle_uncounted(1), 1);
    assert_eq!(core.trees_in_memory(), 0);
}

/// Asserts the line a read says: none for the first read of a tree that was quick; the entries,
/// the number of reads, the time and the requests that waited for a read after the first or a slow
/// one; and the failure of a read that failed.
#[test]
fn a_read_again_or_a_slow_read_is_said_with_its_numbers() {
    let (rig, _) = attached("tfs-memory-said");
    let loaded = rig.core.tree(1).unwrap();
    let digest = loaded.digest;
    let entries = loaded.tree.len();
    let ok = Ok(Arc::clone(&loaded));
    assert_eq!(said((&digest, 1), &ok, (1, SLOW_READ_MS - 1, 0)), None);
    assert_eq!(
        said((&digest, 1), &ok, (2, 15, 3)).unwrap(),
        format!(
            "tierstore fs: read the tree {digest} (number 1, read 2 times in this process): \
             {entries} entries, in 15 ms, while 3 requests waited for it"
        )
    );
    assert_eq!(
        said((&digest, 7), &ok, (1, SLOW_READ_MS, 0)).unwrap(),
        format!(
            "tierstore fs: read the tree {digest} (number 7, read 1 times in this process): \
             {entries} entries, in {SLOW_READ_MS} ms, while 0 requests waited for it"
        )
    );
    let failed = Err(Unread::Failed(STOPPED.to_owned()));
    assert_eq!(
        said((&digest, 1), &failed, (3, 0, 1)).unwrap(),
        format!(
            "tierstore fs: read the tree {digest} (number 1, read 3 times in this process): \
             failed: {STOPPED}, in 0 ms, while 1 requests waited for it"
        )
    );
}

/// Asserts that a request for a tree number the table does not know is `ENOENT`, read by none.
#[test]
fn a_number_the_table_does_not_know_is_no_entry() {
    let (rig, _) = attached("tfs-memory-unknown");
    assert_eq!(rig.core.tree(9).unwrap_err(), Errno::NoEnt);
    assert_eq!(rig.core.times_read(9), 0);
}

/// Asserts that a read of a tree that takes `SLOW_READ_MS` or more on the core's clock (held here
/// at the fill socket while the test moves the clock) still answers its request and is counted as
/// a read, the one that is said at warn.
#[test]
fn a_slow_read_again_answers_its_request() {
    let (rig, bin) = attached("tfs-memory-slow");
    out_of_memory(&rig, bin);
    let before = root_fills(&rig);
    let fills = rig.upstream.asked("fill").len();
    rig.upstream.gate_manifests();
    let asking = ask_from(&rig, 1, bin);
    rig.upstream.wait_for_manifest_fills(fills + 1);
    rig.clock.advance(Duration::from_millis(SLOW_READ_MS));
    rig.upstream.permit_manifests(1_000);
    for asked in asking {
        assert_eq!(asked.join().unwrap(), Ok(bin));
    }
    assert_eq!(root_fills(&rig), before + 1);
    assert_eq!(rig.core.times_read(1), 2);
}

/// Asserts that a lookup of `trees/<digest>` for a tree that has a number and is out of memory
/// reads it through the one read of the tree: a read that fails is `EIO` and leaves nothing
/// attached, and once the manifests can be had the lookup reads the tree and attaches it.
#[test]
fn a_lookup_of_a_numbered_tree_out_of_memory_reads_it_once_or_fails() {
    let (rig, bin) = attached("tfs-memory-implicit");
    out_of_memory(&rig, bin);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.upstream
        .failing
        .lock()
        .unwrap()
        .insert(root.clone(), "upstream 503".to_owned());
    assert_eq!(rig.core.lookup(ino::TREES, &root).unwrap_err(), Errno::Io);
    assert_eq!(rig.core.trees_in_memory(), 0);
    rig.upstream.failing.lock().unwrap().clear();
    assert_eq!(found(&rig.core, ino::TREES, &root), root_ino());
    assert_eq!(rig.core.times_read(1), 3);
}

/// Asserts that requests that waited for a read whose leader stopped before it ended (it
/// panicked) are answered `EIO`, from the failure such a read leaves (`STOPPED`), and not left
/// waiting; that the stopped read is out of the table; and that the next request starts a read of
/// its own, which reads the tree.
#[test]
fn requests_that_waited_for_a_read_that_stopped_fail_and_the_next_reads_again() {
    let (rig, bin) = attached("tfs-memory-stopped");
    out_of_memory(&rig, bin);
    let before = root_fills(&rig);
    let fills = rig.upstream.asked("fill").len();
    rig.upstream.gate_manifests();
    rig.core.reads.stop_next.store(true, Ordering::Release);
    let leader = ask_from(&rig, 1, bin);
    rig.upstream.wait_for_manifest_fills(fills + 1);
    let followers = ask_from(&rig, 2, bin);
    until(|| rig.core.waiting_for_reads() == 2);
    rig.upstream.permit_manifests(1_000);
    for asked in leader {
        assert!(asked.join().is_err(), "the leader stops");
    }
    for asked in followers {
        assert_eq!(asked.join().unwrap(), Err(Errno::Io));
    }
    assert_eq!(rig.core.waiting_for_reads(), 0);
    assert_eq!(
        rig.core.trees_in_memory(),
        1,
        "the stopped read had kept the tree"
    );
    assert_eq!(root_fills(&rig), before + 1);
    // The tree it kept leaves at the next forget; a new request then reads it again.
    let _ = found(&rig.core, root_ino(), "bin");
    rig.core.forget(bin, 1);
    assert_eq!(rig.core.trees_in_memory(), 0);
    assert_eq!(rig.core.getattr(bin).unwrap().attr.ino, bin);
    // The read of the attach and this one; the stopped read never ended, so is not counted. (Its
    // fills left the manifests in the cache, so this read asks the fill socket for none.)
    assert_eq!(rig.core.times_read(1), 2);
}

/// Asserts the failure a stopped read leaves to those that waited for it, word for word.
#[test]
fn a_stopped_read_says_why() {
    assert_eq!(
        Unread::Failed(STOPPED.to_owned()).to_string(),
        "the read of the tree stopped before it ended"
    );
}

/// The line a read of the tree numbered 1 says when it ended with `how` (`failed` or `gone`) for
/// `why`, the `times`-th read of it, on a clock that did not move and with no request waiting.
fn read_line(rig: &Rig, times: u64, how: &str, why: &str) -> String {
    let digest = rig.upstream.built.lock().unwrap().root.clone();
    format!(
        "tierstore fs: read the tree {digest} (number 1, read {times} times in this process): \
         {how}: {why}, in 0 ms, while 0 requests waited for it"
    )
}

/// A core that has not read the tree numbered 1 in its process (the successor of the rig's, over
/// the same table), over a cache and an upstream that have no manifest of it, and where the
/// upstream answers the manifest of the tree with `answer`: none to leave it with no manifest at
/// all.
fn unread_tree(name: &str, answer: Option<&str>) -> (Rig, Arc<Core>) {
    let (rig, bin) = attached(name);
    out_of_memory(&rig, bin);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    match answer {
        Some(answer) => {
            rig.upstream
                .failing
                .lock()
                .unwrap()
                .insert(root, answer.to_owned());
        }
        None => rig.upstream.built.lock().unwrap().manifests.clear(),
    }
    let successor = rig.successor();
    (rig, successor)
}

/// Asserts that a tree that cannot be read now (its root manifest refused with a 503) answers
/// `EIO` to a `getattr` and an `open` of its inodes, reading it again for each, and that its
/// first read in the process is said at warn with the whole reason, which it was not before; the
/// second, at the same time, is not said.
#[test]
fn a_tree_that_cannot_be_read_now_is_an_io_error_and_its_first_read_is_said() {
    let (rig, core) = unread_tree("tfs-memory-unreadable", Some("upstream 503"));
    let why = format!(
        "cannot fetch manifest {}: upstream 503",
        rig.upstream.built.lock().unwrap().root
    );
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    assert_eq!(core.times_read(1), 1);
    assert_eq!(
        log.text(),
        format!("{}\n", read_line(&rig, 1, "failed", &why))
    );
    assert_eq!(core.open(ino::of(1, 3), false).err(), Some(Errno::Io));
    drop(guard);
    assert_eq!(core.times_read(1), 2);
    assert_eq!(
        log.text(),
        format!("{}\n", read_line(&rig, 1, "failed", &why))
    );
}

/// Asserts that a tree that is gone (its root manifest is not upstream) answers `ENOENT` to a
/// `getattr`, a lookup below its root, an `open` of a file inode of it and a `readdir`, reading it
/// again for each, and that its first read in the process is said at warn as gone, with the whole
/// reason, and the three after it, at the same time, are not.
#[test]
fn a_tree_that_is_gone_is_no_entry_everywhere_and_its_first_read_is_said() {
    let (rig, core) = unread_tree("tfs-memory-gone", None);
    let why = format!(
        "cannot fetch manifest {}: upstream 404",
        rig.upstream.built.lock().unwrap().root
    );
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    assert_eq!(core.getattr(root_ino()), Err(Errno::NoEnt));
    assert_eq!(
        log.text(),
        format!("{}\n", read_line(&rig, 1, "gone", &why))
    );
    assert_eq!(core.lookup(root_ino(), "bin").err(), Some(Errno::NoEnt));
    assert_eq!(core.open(ino::of(1, 3), false).err(), Some(Errno::NoEnt));
    assert_eq!(
        core.readdir(root_ino(), 0, &mut |_, _| true),
        Err(Errno::NoEnt)
    );
    drop(guard);
    assert_eq!(core.times_read(1), 4);
    assert_eq!(
        log.text(),
        format!("{}\n", read_line(&rig, 1, "gone", &why))
    );
}

/// Asserts that the failed reads of a tree are said at most once in `SAY_EVERY_MS` of the core's
/// clock: the first at once; none of those within that time of it, a millisecond short of it
/// included; the next at that time, with the count of the reads not said (three, then one); and
/// that a read that succeeds after them says nothing at warn.
#[test]
fn the_failed_reads_of_a_tree_are_said_once_in_ten_seconds_with_the_count_not_said() {
    let (rig, core) = unread_tree("tfs-memory-twice", Some("upstream 503"));
    let why = format!(
        "cannot fetch manifest {}: upstream 503",
        rig.upstream.built.lock().unwrap().root
    );
    let every = Duration::from_millis(SAY_EVERY_MS);
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    for _ in 0..3 {
        assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    }
    rig.clock.advance(every - Duration::from_millis(1));
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    assert_eq!(log.text().lines().count(), 1, "{}", log.text());
    rig.clock.advance(Duration::from_millis(1));
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    rig.clock.advance(every);
    assert_eq!(core.getattr(root_ino()), Err(Errno::Io));
    let three = "; 3 more failed reads of this tree since the last line";
    let one = "; 1 more failed read of this tree since the last line";
    assert_eq!(
        log.text(),
        format!(
            "{}\n{}{three}\n{}{one}\n",
            read_line(&rig, 1, "failed", &why),
            read_line(&rig, 5, "failed", &why),
            read_line(&rig, 7, "failed", &why)
        )
    );
    rig.upstream.failing.lock().unwrap().clear();
    rig.clock.advance(every);
    assert!(core.getattr(root_ino()).is_ok());
    drop(guard);
    assert_eq!(core.times_read(1), 8);
    assert_eq!(log.text().lines().count(), 3);
}
