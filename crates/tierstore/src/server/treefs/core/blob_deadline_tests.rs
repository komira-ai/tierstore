//! The tests of the one end of an open's fills, of the lines of the reads of open files that
//! fail, and of what the sweep's look tells the node and says (`core/blob.rs`, `core/said.rs`).

use std::time::Duration;

use super::*;
use crate::Mono;
use crate::server::treefs::core::{Handle, Settings};
use crate::server::treefs::fill_client::FillClient;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{Logged, Rig, logged, open_cached, sample, until};

const RUN: &str = "run-1";

/// The time an answer is waited for in the rigs of these tests: the length of an open's end.
const WAIT: Duration = Duration::from_secs(30);

/// The pauses a fill client took, in order.
type Pauses = Arc<Mutex<Vec<Duration>>>;

/// A rig over the sample tree with `RUN` attached, whose core opens the files of the cache with
/// `open`, and whose fill client's pauses move the rig's clock by what they pause, as a real
/// pause takes its time; and those pauses.
fn rig_ticking(
    name: &str,
    open: impl Fn(&Path) -> io::Result<File> + Send + Sync + 'static,
) -> (Rig, Pauses) {
    let mut rig = Rig::new(name, sample());
    let runtime = rig.dir.path().join("run-ticking");
    std::fs::create_dir_all(&runtime).unwrap();
    let pauses = Pauses::default();
    let (taken, clock): (Pauses, Arc<Mono>) = (Arc::clone(&pauses), Arc::clone(&rig.clock));
    let fills = FillClient::pausing(
        rig.dir.path().join("fill.sock"),
        3,
        WAIT,
        Arc::new(move |pause| {
            taken.lock().unwrap().push(pause);
            clock.advance(pause);
        }),
    );
    let settings: Settings = rig.core.settings().clone();
    rig.core = Arc::new(
        Core::new(
            settings,
            Arc::new(fills),
            TreeTable::create(&runtime).unwrap(),
            &runtime,
        )
        .opening(Box::new(open)),
    );
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    (rig, pauses)
}

/// The digest of the blob of `len` bytes of the sample tree.
fn blob_for(rig: &Rig, len: usize) -> String {
    let built = rig.upstream.built.lock().unwrap();
    built
        .blobs
        .iter()
        .find(|(_, bytes)| bytes.len() == len)
        .map(|(sha, _)| sha.clone())
        .unwrap()
}

/// The fills of blobs the fake received, for the blob `sha`.
fn blob_fills(rig: &Rig, sha: &str) -> usize {
    rig.upstream
        .asked("fill")
        .iter()
        .filter(|line| line.split(' ').count() > 2 && line.contains(sha))
        .count()
}

fn fail_with(rig: &Rig, sha: &str, why: &str) {
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(sha.to_owned(), why.to_owned());
}

/// Opens `/bin/cc` while its fills wait at the fake's gate, moving the clock by `taken` while the
/// first fill waits for its answer, and answers every fill; the open's answer.
fn open_taking(rig: &Rig, taken: Duration) -> Result<u64, Errno> {
    rig.upstream.gate_verb("fill");
    std::thread::scope(|scope| {
        let open = scope.spawn(|| open_cached(&rig.core, ino::of(1, 3), false));
        rig.upstream.wait_for_verb("fill", 1);
        rig.clock.advance(taken);
        rig.upstream.permit_verb("fill", 3);
        open.join().unwrap()
    })
}

/// Asserts that a fill whose answer took the whole of `fs.fetch_timeout` (the clock passed the
/// open's end while it waited) is not asked again, whatever reason it answers: the open fails with
/// `EIO` after one fill and no pause. So an open holds a reader no longer than one
/// `fs.fetch_timeout`, however many tries `FILL_TRIES` allows.
#[test]
fn a_fill_that_took_the_whole_fetch_timeout_is_not_asked_again() {
    for why in ["busy", "the fill socket gave no answer in time"] {
        let (rig, pauses) = rig_ticking("tfs-deadline-spent", |path| File::open(path));
        let cc = blob_for(&rig, 3000);
        fail_with(&rig, &cc, why);
        assert_eq!(open_taking(&rig, WAIT), Err(Errno::Io), "{why}");
        assert_eq!(blob_fills(&rig, &cc), 1, "{why}");
        assert_eq!(pauses.lock().unwrap().len(), 0, "{why}");
        let failures = rig.core.detach(RUN).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].reason, why);
    }
}

/// Asserts the boundary of the pause before another try: with a second or less left before the
/// open's end no pause is taken (the try after it could not be sent) and no fill asked again, and
/// the open fails with the last real answer; with a millisecond more than a second left the pause
/// is taken and the fill asked again.
#[test]
fn a_pause_is_taken_only_while_more_than_a_second_is_left_before_the_end() {
    let second = Duration::from_secs(1);
    for (name, taken) in [
        (
            "tfs-deadline-short",
            WAIT - second + Duration::from_millis(1),
        ),
        ("tfs-deadline-exact", WAIT - second),
    ] {
        let (rig, pauses) = rig_ticking(name, |path| File::open(path));
        let cc = blob_for(&rig, 3000);
        fail_with(&rig, &cc, "busy");
        assert_eq!(open_taking(&rig, taken), Err(Errno::Io), "{name}");
        assert_eq!(blob_fills(&rig, &cc), 1, "{name}");
        assert_eq!(pauses.lock().unwrap().len(), 0, "{name}");
        assert_eq!(rig.core.detach(RUN).unwrap()[0].reason, "busy", "{name}");
    }
    let (rig, pauses) = rig_ticking("tfs-deadline-over", |path| File::open(path));
    let cc = blob_for(&rig, 3000);
    fail_with(&rig, &cc, "busy");
    let taken = WAIT - second - Duration::from_millis(1);
    assert_eq!(open_taking(&rig, taken), Err(Errno::Io));
    assert_eq!(*pauses.lock().unwrap(), [second]);
    assert_eq!(blob_fills(&rig, &cc), 2);
}

/// Asserts that with time left a fill that can pass is asked again after a pause, and the open
/// succeeds when the next answer is `ok`: the end bounds the tries, it does not forbid them.
#[test]
fn a_fill_that_passes_with_time_left_is_asked_again_and_the_open_succeeds() {
    let (rig, pauses) = rig_ticking("tfs-deadline-left", |path| File::open(path));
    let cc = blob_for(&rig, 3000);
    fail_with(&rig, &cc, "busy");
    rig.upstream.gate_verb("fill");
    let opened = std::thread::scope(|scope| {
        let open = scope.spawn(|| open_cached(&rig.core, ino::of(1, 3), false));
        rig.upstream.wait_for_verb("fill", 1);
        rig.clock.advance(WAIT / 2);
        rig.upstream.permit_verb("fill", 1);
        rig.upstream.wait_for_verb("fill", 2);
        let _ = rig.upstream.failing.lock().unwrap().remove(&cc);
        rig.upstream.permit_verb("fill", 1);
        open.join().unwrap()
    });
    assert!(opened.is_ok(), "{opened:?}");
    assert_eq!(blob_fills(&rig, &cc), 2);
    assert_eq!(*pauses.lock().unwrap(), [Duration::from_secs(1)]);
}

/// Puts in place of the handle `fh` of `ino` a handle whose descriptor is a directory, so that a
/// read of it fails with `EISDIR`.
fn break_handle(rig: &Rig, fh: u64, ino: u64) {
    let directory = File::open(&rig.cache).unwrap();
    rig.core.handles.insert(
        fh,
        Arc::new(Handle::Cached {
            bytes: FileBytes::whole(directory.into(), 3000),
            ino,
        }),
    );
}

/// Asserts the line said at warn for a read of an open file that fails, whole: the path, the
/// tree, the offset, the blob and where it is read from, and what the system said; that the read
/// is `EIO`; that a second within ten seconds is not said, and the next after them carries the
/// count of those not said.
#[test]
fn a_read_of_an_open_file_that_fails_is_said_with_the_file_and_the_error() {
    let (rig, _) = rig_ticking("tfs-read-said", |path| File::open(path));
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_for(&rig, 3000);
    let fh = open_cached(core, ino::of(1, 3), false).unwrap();
    break_handle(&rig, fh, ino::of(1, 3));
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    let line = |offset: u64, more: &str| {
        format!(
            "tierstore fs: cannot read /bin/cc in the tree {root} (number 1) at {offset}: the \
             blob {cc} of 3000 bytes from packs/one @ 4096: Is a directory (os error 21){more}"
        )
    };
    assert_eq!(core.read(fh, 10, 4), Err(Errno::Io));
    assert_eq!(core.read(fh, 20, 4), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS - 1));
    assert_eq!(core.read(fh, 30, 4), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(1));
    assert_eq!(core.read(fh, 40, 4), Err(Errno::Io));
    drop(guard);
    let more = "; 2 more failed reads of open files of this tree since the last line";
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(said, [line(10, ""), line(40, more)]);
}

/// Asserts that the line after one failed read not said carries the count in the singular.
#[test]
fn one_failed_read_not_said_is_counted_in_the_singular() {
    let (rig, _) = rig_ticking("tfs-read-one-more", |path| File::open(path));
    let core = &rig.core;
    let fh = open_cached(core, ino::of(1, 3), false).unwrap();
    break_handle(&rig, fh, ino::of(1, 3));
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    assert_eq!(core.read(fh, 10, 4), Err(Errno::Io));
    assert_eq!(core.read(fh, 20, 4), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    assert_eq!(core.read(fh, 30, 4), Err(Errno::Io));
    drop(guard);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_for(&rig, 3000);
    let line = |offset: u64, more: &str| {
        format!(
            "tierstore fs: cannot read /bin/cc in the tree {root} (number 1) at {offset}: the \
             blob {cc} of 3000 bytes from packs/one @ 4096: Is a directory (os error 21){more}"
        )
    };
    let more = "; 1 more failed read of an open file of this tree since the last line";
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(said, [line(10, ""), line(30, more)]);
}

/// Asserts that a read of an open file of a tree that has left memory names the inode, and that
/// a read of a handle the kernel should read itself (an adopted backing id) is `EIO` and says why.
#[test]
fn a_failed_read_of_a_tree_not_in_memory_names_the_inode_and_an_adopted_read_says_why() {
    let (rig, _) = rig_ticking("tfs-read-inode", |path| File::open(path));
    let core = &rig.core;
    let cc = ino::of(1, 3);
    let fh = open_cached(core, cc, false).unwrap();
    break_handle(&rig, fh, cc);
    let _ = core.lookup(ino::TREES, &rig.upstream.built.lock().unwrap().root.clone());
    assert!(core.detach(RUN).is_some());
    core.forget(ino::of(1, 0), 1);
    assert_eq!(core.trees_in_memory(), 0);
    let adopted = ino::of(1, 7);
    let other = 1 << 40;
    core.handles
        .insert(other, Arc::new(Handle::Adopted { ino: adopted }));
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    assert_eq!(core.read(fh, 5, 4), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    assert_eq!(core.read(other, 0, 4), Err(Errno::Io));
    drop(guard);
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said,
        [
            format!("tierstore fs: cannot read the inode {cc} at 5: Is a directory (os error 21)"),
            format!(
                "tierstore fs: cannot read the inode {adopted} at 0: the kernel reads this file \
                 itself (a backing id of an earlier release)"
            ),
        ]
    );
}

/// Asserts what a look of the sweep tells the node: `descriptors <mount> <process> <held>
/// <short>`, with the cache files open now and the opens that failed for lack of descriptors
/// since the process started (the same total at the next looks, with none since); and that it says the counts of failed opens not said once
/// their last line is ten seconds old, and not before.
#[test]
fn a_look_reports_the_descriptors_and_says_the_counts_not_said() {
    let short = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refusing = Arc::clone(&short);
    let (rig, _) = rig_ticking("tfs-look", move |path| {
        if refusing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(24));
        }
        File::open(path)
    });
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let (cc, tool) = (ino::of(1, 3), ino::of(1, 4));
    let held = [
        open_cached(core, cc, false).unwrap(),
        open_cached(core, cc, false).unwrap(),
        open_cached(core, tool, false).unwrap(),
    ];
    short.store(true, std::sync::atomic::Ordering::SeqCst);
    let header = ino::of(1, 7);
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    for _ in 0..3 {
        assert_eq!(open_cached(core, header, false), Err(Errno::Io));
    }
    let reports = || rig.upstream.asked("descriptors");
    let process = std::process::id();
    core.look_around();
    until(|| reports().len() == 1);
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS - 1));
    core.look_around();
    until(|| reports().len() == 2);
    rig.clock.advance(Duration::from_millis(1));
    core.look_around();
    until(|| reports().len() == 3);
    drop(guard);
    assert_eq!(
        reports(),
        [
            format!("descriptors mount-1 {process} 2 3"),
            format!("descriptors mount-1 {process} 2 3"),
            format!("descriptors mount-1 {process} 2 3"),
        ]
    );
    let header_blob = testing_sha(b"int a;");
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said,
        [
            format!(
                "tierstore fs: cannot open /include/a.h in the tree {root} (number 1): the blob \
                 {header_blob} of 6 bytes from packs/one @ 4096: no descriptor: Too many open \
                 files (os error 24) (after 1 fills)"
            ),
            format!(
                "tierstore fs: 2 more failed opens of the tree {root} (number 1) since the last \
                 line, not said"
            ),
        ]
    );
    for fh in held {
        core.release(fh);
    }
}

/// Asserts that a tree that leaves memory says the counts of its failures not said as it leaves,
/// within ten seconds of its last line.
#[test]
fn a_tree_that_leaves_memory_says_its_counts_not_said() {
    let (rig, _) = rig_ticking("tfs-leave-said", |path| File::open(path));
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_for(&rig, 3000);
    fail_with(&rig, &cc, "mismatch");
    let _ = core.lookup(ino::TREES, &root);
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    for _ in 0..3 {
        assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    }
    assert!(core.detach(RUN).is_some());
    assert_eq!(log.text().lines().count(), 1, "{}", log.text());
    core.forget(ino::of(1, 0), 1);
    drop(guard);
    assert_eq!(core.trees_in_memory(), 0);
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said[1],
        format!(
            "tierstore fs: 2 more failed opens of the tree {root} (number 1) since the last line, \
             not said"
        )
    );
    assert_eq!(said.len(), 2, "{}", log.text());
}

fn testing_sha(bytes: &[u8]) -> String {
    crate::server::treefs::testing::sha(bytes)
}
