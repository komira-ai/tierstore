//! The tests of `core/blob.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::server::treefs::core::Settings;
use crate::server::treefs::fill_client::FillClient;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{Logged, Rig, file, logged, open_cached, sample};
use crate::server::treefs::{ino, testing};

const RUN: &str = "run-1";

/// `EMFILE`: the process has no descriptor left.
const EMFILE: i32 = 24;

/// `ENFILE`: the system has no descriptor left.
const ENFILE: i32 = 23;

/// `EACCES`: a file that may not be opened.
const EACCES: i32 = 13;

/// The pauses a fill client took, in order.
type Pauses = Arc<std::sync::Mutex<Vec<Duration>>>;

/// A rig over the sample tree with `RUN` attached, whose core opens the files of the cache with
/// `open` (its own table of tree numbers, the same fill socket and clock).
fn rig_opening(
    name: &str,
    open: impl Fn(&Path) -> io::Result<File> + Send + Sync + 'static,
) -> Rig {
    rig_pausing(name, open).0
}

/// [`rig_opening`], and the pauses its core's fill client takes, which take no time.
fn rig_pausing(
    name: &str,
    open: impl Fn(&Path) -> io::Result<File> + Send + Sync + 'static,
) -> (Rig, Pauses) {
    let mut rig = Rig::new(name, sample());
    let runtime = rig.dir.path().join("run-opening");
    std::fs::create_dir_all(&runtime).unwrap();
    let pauses = Pauses::default();
    let taken = Arc::clone(&pauses);
    let fills = FillClient::pausing(
        rig.dir.path().join("fill.sock"),
        3,
        Duration::from_secs(30),
        Arc::new(move |pause| taken.lock().unwrap().push(pause)),
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

/// A rig whose files open with `File::open`, and the number of times the opener was called.
fn counted(name: &str) -> (Rig, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let rig = rig_opening(name, move |path| {
        let _ = seen.fetch_add(1, Ordering::SeqCst);
        File::open(path)
    });
    (rig, calls)
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

/// The inodes of the files of the sample tree that have bytes: big.bin, bin/cc, bin/tool,
/// include/a.h.
fn files() -> [u64; 4] {
    [ino::of(1, 1), ino::of(1, 3), ino::of(1, 4), ino::of(1, 7)]
}

/// Asserts that a hundred cold opens of the files of one tree at once, whose fills all wait for
/// the node, kept open together by a process whose descriptors are just enough for one per blob,
/// all succeed once the fills are answered and hold one descriptor per blob: the opener is called
/// once per blob. Catches the open that took a descriptor for every handle,
/// which ran out of them past the budget and failed those opens with `EIO`.
#[test]
fn a_hundred_opens_of_one_tree_hold_one_descriptor_per_blob() {
    let budget = files().len();
    let opened = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (ok, seen) = (Arc::clone(&opened), Arc::clone(&calls));
    let rig = rig_opening("tfs-blob-herd", move |path| {
        let _ = seen.fetch_add(1, Ordering::SeqCst);
        if ok.fetch_add(1, Ordering::SeqCst) >= budget {
            let _ = ok.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::from_raw_os_error(EMFILE));
        }
        File::open(path)
    });
    // Every open is cold: each asks for its fill, and the fills wait at the gate until all the
    // hundred have come, then are answered at once.
    let core = &rig.core;
    rig.upstream.gate_verb("fill");
    let more: Vec<Result<u64, Errno>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..100)
            .map(|i| scope.spawn(move || open_cached(core, files()[i % budget], false)))
            .collect();
        rig.upstream.wait_for_verb("fill", 100);
        assert_eq!(core.open_descriptors(), 0);
        rig.upstream.permit_verb("fill", 100);
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    assert!(more.iter().all(Result::is_ok), "{more:?}");
    let handles: Vec<u64> = more.into_iter().map(Result::unwrap).collect();
    assert_eq!(handles.len(), 100);
    assert_eq!(core.open_descriptors(), budget);
    assert_eq!(calls.load(Ordering::SeqCst), budget);
    assert_eq!(opened.load(Ordering::SeqCst), budget);
    for fh in handles {
        core.release(fh);
    }
    assert_eq!(core.open_descriptors(), 0);
}

/// Asserts that the descriptor of a blob is shared by its handles and closed when the last of
/// them is released.
#[test]
fn a_shared_descriptor_is_closed_when_the_last_handle_is_released() {
    let (rig, calls) = counted("tfs-blob-release");
    let core = &rig.core;
    let first = open_cached(core, ino::of(1, 3), false).unwrap();
    let second = open_cached(core, ino::of(1, 3), false).unwrap();
    assert_eq!(core.open_descriptors(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    core.release(first);
    assert_eq!(core.open_descriptors(), 1);
    core.release(second);
    assert_eq!(core.open_descriptors(), 0);
}

/// Asserts that a file of the cache replaced at its path (removed and written again with the same
/// bytes, a new inode) is opened again by the next open, and that the open before it is not
/// shared with it.
#[test]
fn a_cache_file_replaced_at_its_path_is_opened_again() {
    let (rig, calls) = counted("tfs-blob-replaced");
    let core = &rig.core;
    let cc = blob_for(&rig, 3000);
    let first = open_cached(core, ino::of(1, 3), false).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let bytes = core.read(first, 0, 3000).unwrap();
    let path = blob_path(&rig.cache, &cc);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let second = open_cached(core, ino::of(1, 3), false).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(core.read(second, 0, 3000).unwrap(), bytes);
    // Unchanged since: shared again.
    let third = open_cached(core, ino::of(1, 3), false).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for fh in [first, second, third] {
        core.release(fh);
    }
}

/// Asserts, word by word, which answers of a fill may pass if it is asked again and which may not.
#[test]
fn passes_names_the_reasons_a_fill_may_pass() {
    let passing = [
        "busy",
        "short",
        "read-only",
        "changed",
        "aborted",
        "io",
        "io: Too many open files (os error 24)",
        "no space",
        "the filled range is no longer in the cache",
        "the fill socket gave no answer in time",
        "upstream 503",
        "upstream 500",
        "fill: busy",
        "fill: upstream 503",
    ];
    for reason in passing {
        assert!(passes(reason), "{reason}");
    }
    let permanent = [
        "upstream 404",
        "upstream 501",
        "upstream 400",
        "upstream 403",
        "mismatch",
        "fill: mismatch",
        "too large",
        "the fill socket is not answering",
        "the fill socket did not answer within the time the request was given",
        "the file has no source",
        "refused unknown request",
        "upstream abc",
        "",
    ];
    for reason in permanent {
        assert!(!passes(reason), "{reason}");
    }
}

/// Asserts that a fill answered with a reason that can pass and then `ok` is asked again, once,
/// and the open succeeds.
#[test]
fn a_fill_that_passes_is_asked_again_and_the_open_succeeds() {
    let (rig, _) = counted("tfs-blob-retry");
    let core = &rig.core;
    let cc = blob_for(&rig, 3000);
    fail_with(&rig, &cc, "busy");
    rig.upstream.gate_verb("fill");
    let opened = std::thread::scope(|scope| {
        let open = scope.spawn(|| open_cached(core, ino::of(1, 3), false));
        rig.upstream.wait_for_verb("fill", 1);
        rig.upstream.permit_verb("fill", 1);
        // The second request is the fill asked again: the first was answered `busy`.
        rig.upstream.wait_for_verb("fill", 2);
        let _ = rig.upstream.failing.lock().unwrap().remove(&cc);
        rig.upstream.permit_verb("fill", 1);
        open.join().unwrap()
    });
    assert!(opened.is_ok(), "{opened:?}");
    assert_eq!(blob_fills(&rig, &cc), 2);
    assert_eq!(core.detach(RUN).unwrap().failures, Vec::<Failure>::new());
}

/// Asserts that a fill that keeps answering a reason that can pass is asked `FILL_TRIES` times,
/// with a pause of a second between two tries and none after the last, and then fails the open
/// with `EIO`, recorded with the last answer; and that a reason that does not pass is asked once,
/// with no pause.
#[test]
fn a_fill_is_asked_at_most_three_times_and_a_permanent_answer_once() {
    let (rig, pauses) = rig_pausing("tfs-blob-tries", |path| File::open(path));
    let core = &rig.core;
    let cc = blob_for(&rig, 3000);
    let tool = blob_for(&rig, 10);
    fail_with(&rig, &cc, "busy");
    fail_with(&rig, &tool, "mismatch");
    let second = Duration::from_secs(1);
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(blob_fills(&rig, &cc), FILL_TRIES as usize);
    assert_eq!(*pauses.lock().unwrap(), [second, second]);
    assert_eq!(open_cached(core, ino::of(1, 4), false), Err(Errno::Io));
    assert_eq!(blob_fills(&rig, &tool), 1);
    assert_eq!(pauses.lock().unwrap().len(), 2);
    let want = vec![
        Failure {
            path: "/bin/cc".into(),
            digest: cc,
            reason: "busy".into(),
        },
        Failure {
            path: "/bin/tool".into(),
            digest: tool,
            reason: "mismatch".into(),
        },
    ];
    assert_eq!(core.detach(RUN).unwrap().failures, want);
}

/// Asserts the line said at warn for a failed open, whole: said once for the first failure of a
/// tree; nothing for the failures within `SAY_EVERY_MS` of the core's clock after it, a
/// millisecond short of that time included; at that time the next failure is said with the count
/// of those not said (two, then one); and a failure of another tree is said at once.
#[test]
fn a_failed_open_is_said_once_in_ten_seconds_for_each_tree() {
    let rig = rig_opening("tfs-blob-say", |path| File::open(path));
    let core = &rig.core;
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let cc = blob_for(&rig, 3000);
    fail_with(&rig, &cc, "busy");
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    let line = |more: &str| {
        format!(
            "tierstore fs: cannot open /bin/cc in the tree {root} (number 1): the blob {cc} of \
             3000 bytes from packs/one @ 4096: busy (after 3 fills){more}"
        )
    };
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(log.text().lines().collect::<Vec<_>>(), [line("")]);
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(log.text().lines().count(), 1, "{}", log.text());
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    let two = "; 2 more failed opens of this tree since the last line";
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(said, [line(""), line(two)]);
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS - 1));
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(1));
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    let one = "; 1 more failed open of this tree since the last line";
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(said, [line(""), line(two), line(one)]);
    // Another tree has its own count of lines.
    let other = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .add_tree(vec![("x", file(b"other bytes"))]);
    let _ = core.attach(&other, "run-2").unwrap();
    let x = testing::sha(b"other bytes");
    fail_with(&rig, &x, "busy");
    assert_eq!(open_cached(core, ino::of(2, 1), false), Err(Errno::Io));
    drop(guard);
    let text = log.text();
    let last = text.lines().last().unwrap();
    assert_eq!(text.lines().count(), 4, "{text}");
    assert_eq!(
        last,
        format!(
            "tierstore fs: cannot open /x in the tree {other} (number 2): the blob {x} of 11 \
             bytes from packs/one @ 4096: busy (after 3 fills)"
        )
    );
}

/// Asserts that an open whose cache file cannot be opened for lack of descriptors (`EMFILE`, and
/// `ENFILE`) fails with `EIO` at once: recorded as `no descriptor: ` and what the system said,
/// counted, asked of the fill socket once for a blob the cache does not hold (the look at the
/// cache finds no file, so the opener runs only after the fill) and never again, with no pause.
#[test]
fn a_cache_file_that_cannot_be_opened_for_lack_of_descriptors_fails_at_once_as_such() {
    for (errno, name) in [(EMFILE, "emfile"), (ENFILE, "enfile")] {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let (rig, pauses) = rig_pausing(&format!("tfs-blob-{name}"), move |_| {
            let _ = seen.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::from_raw_os_error(errno))
        });
        let core = &rig.core;
        let cc = blob_for(&rig, 3000);
        assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
        assert_eq!(blob_fills(&rig, &cc), 1, "{name}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
        assert_eq!(pauses.lock().unwrap().len(), 0, "{name}");
        assert_eq!(core.descriptor_failures(), 1, "{name}");
        assert_eq!(core.open_descriptors(), 0);
        let reason = format!("no descriptor: {}", io::Error::from_raw_os_error(errno));
        let failures = core.detach(RUN).unwrap().failures;
        assert_eq!(
            failures,
            [Failure {
                path: "/bin/cc".into(),
                digest: cc,
                reason,
            }],
            "{name}"
        );
    }
    assert_eq!(
        format!("no descriptor: {}", io::Error::from_raw_os_error(EMFILE)),
        "no descriptor: Too many open files (os error 24)"
    );
}

/// Asserts that a blob the cache holds whose file cannot be opened for lack of descriptors is not
/// filled at all: the fill would find it there. The open is `EIO`, counted, and the next open,
/// once a descriptor is had again, succeeds and reads the blob.
#[test]
fn a_cached_blob_that_cannot_be_opened_for_lack_of_descriptors_is_not_filled_and_recovers() {
    let short = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refusing = Arc::clone(&short);
    let rig = rig_opening("tfs-blob-emfile-cached", move |path| {
        if refusing.load(Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(EMFILE));
        }
        File::open(path)
    });
    let core = &rig.core;
    let cc = blob_for(&rig, 3000);
    let warm = open_cached(core, ino::of(1, 3), false).unwrap();
    core.release(warm);
    assert_eq!(blob_fills(&rig, &cc), 1);
    short.store(true, Ordering::SeqCst);
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(blob_fills(&rig, &cc), 1);
    assert_eq!(core.descriptor_failures(), 1);
    short.store(false, Ordering::SeqCst);
    let fh = open_cached(core, ino::of(1, 3), false).unwrap();
    assert_eq!(core.read(fh, 0, 4).unwrap().len(), 4);
    assert_eq!(blob_fills(&rig, &cc), 1);
    assert_eq!(core.descriptor_failures(), 1);
}

/// Asserts that an open whose cache file cannot be opened for another reason (`EACCES`) fails with
/// `EIO`, recorded as `io: ` and what the system said, after `FILL_TRIES` tries: the fill is asked
/// each time, and the file is opened twice for each (the look at the cache first, then after the
/// fill; none before the first fill, which finds no file). It is not counted as a lack of
/// descriptors.
#[test]
fn a_cache_file_that_cannot_be_opened_otherwise_is_an_io_failure_tried_three_times() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let rig = rig_opening("tfs-blob-eacces", move |_| {
        let _ = seen.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::from_raw_os_error(EACCES))
    });
    let core = &rig.core;
    let cc = blob_for(&rig, 3000);
    assert_eq!(open_cached(core, ino::of(1, 3), false), Err(Errno::Io));
    assert_eq!(blob_fills(&rig, &cc), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(core.descriptor_failures(), 0);
    let failures = core.detach(RUN).unwrap().failures;
    assert_eq!(
        failures,
        [Failure {
            path: "/bin/cc".into(),
            digest: cc,
            reason: "io: Permission denied (os error 13)".into(),
        }]
    );
}

/// Asserts that the paths of closed files are dropped from the maps of the shared descriptors once
/// a map holds more than it may: after a thousand and more files of the cache are opened and
/// closed one by one, the maps hold far fewer paths than were opened, none of them open; and that
/// a file held open through the sweeps is still shared by the next open of it.
#[test]
fn the_paths_of_closed_files_are_dropped_and_an_open_one_is_kept() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "tfs-blob-sweep");
    let path = |i: usize| dir.path().join(format!("f{i}"));
    let files = OpenFiles::new();
    std::fs::write(path(0), b"kept").unwrap();
    let kept = files.get(&path(0)).unwrap();
    let many = SHARDS * SWEEP_AT * 2;
    for i in 1..=many {
        std::fs::write(path(i), b"x").unwrap();
        drop(files.get(&path(i)).unwrap());
    }
    assert!(files.entries() < many / 2, "{} paths kept", files.entries());
    assert_eq!(files.len(), 1);
    assert!(Arc::ptr_eq(&kept, &files.get(&path(0)).unwrap()));
}
