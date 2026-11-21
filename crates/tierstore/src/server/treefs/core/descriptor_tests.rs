//! The tests of `core/blob.rs` that count the process's real descriptors, as `/proc/self/fd` lists
//! them, and one that runs the process out of them.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::server::treefs::attach::Failure;
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Logged, Rig, logged, open_cached, sample};

const RUN: &str = "run-1";

/// How many handles of one file the sharing test holds at once.
const HANDLES: usize = 3000;

/// The descriptors of this process that lead to the file at `path`: how many entries of
/// `/proc/self/fd` link to it. Other tests of the process open other files.
pub(in crate::server::treefs) fn descriptors_on(path: &Path) -> usize {
    // The file may not be there yet: its nearest directory that is, as the system names it.
    let near = path.ancestors().find(|near| near.exists()).unwrap();
    let want = near
        .canonicalize()
        .unwrap()
        .join(path.strip_prefix(near).unwrap());
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| *target == want)
        .count()
}

/// Every descriptor number of this process, apart from the one that lists them.
fn numbers() -> Vec<u64> {
    let listing = Path::new("/proc/self/fd").canonicalize().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            // The directory being listed is a descriptor too, and is gone when the listing ends.
            let target = std::fs::read_link(&path).ok()?;
            if target == listing {
                return None;
            }
            path.file_name()?.to_str()?.parse().ok()
        })
        .collect()
}

/// A rig over the sample tree with `RUN` attached, and the path of the cache file of its blob of
/// `len` bytes.
fn rig_with_blob(name: &str, len: usize) -> (Rig, PathBuf) {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    let sha = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .blobs
        .iter()
        .find(|(_, bytes)| bytes.len() == len)
        .map(|(sha, _)| sha.clone())
        .unwrap();
    let path = crate::server::local_cache::blob_path(&rig.cache, &sha);
    (rig, path)
}

/// Asserts that three thousand handles of one file, all open at once, hold exactly one real
/// descriptor of the cache file (counted in `/proc/self/fd` by where each leads), that handles
/// read the file's bytes, and that one is held until the last handle is released and none after.
#[test]
fn three_thousand_handles_of_one_cache_file_hold_one_real_descriptor() {
    let (rig, path) = rig_with_blob("tfs-fds-shared", 3000);
    let core = &rig.core;
    assert_eq!(descriptors_on(&path), 0);
    let handles: Vec<u64> = (0..HANDLES)
        .map(|_| open_cached(core, ino::of(1, 3), false).unwrap())
        .collect();
    assert_eq!(descriptors_on(&path), 1);
    assert_eq!(core.open_descriptors(), 1);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(core.read(handles[0], 0, 3000).unwrap(), bytes);
    assert_eq!(core.read(handles[HANDLES - 1], 0, 3000).unwrap(), bytes);
    let last = handles.len() - 1;
    for (i, fh) in handles.into_iter().enumerate() {
        core.release(fh);
        let want = usize::from(i < last);
        assert_eq!(descriptors_on(&path), want, "after {} releases", i + 1);
    }
    assert_eq!(core.open_descriptors(), 0);
}

// What a failed open for want of descriptors is: the reason recorded for it and said, the fills it
// asks the fill socket for (none: the cache holds the blob), and the tries it makes (one).
const NO_DESCRIPTOR_REASON: &str = "no descriptor: Too many open files (os error 24)";
const NO_DESCRIPTOR_FILLS: usize = 0;
const NO_DESCRIPTOR_TRIES: u32 = 1;

/// The marker the parent test sets for its child.
const CHILD_MARKER: &str = "TIERSTORE_TEST_DESCRIPTOR_CHILD";

/// The name of the test that starts the child.
const PARENT: &str = "a_process_out_of_descriptors_fails_an_open_with_a_real_emfile_and_recovers";

/// Asserts that a cache file whose open runs the process out of descriptors (the real `EMFILE`: the
/// limit lowered to just the descriptors the process holds) fails the open with `EIO` at once, with
/// no fill asked and no try again, recorded on the tree's attachment and said at warn as a lack of
/// descriptors in the system's own words, and counted; that the
/// handle opened before still reads, and that the same open succeeds once descriptors are free
/// again. The body runs in a child process of this binary (the limit is the process's), which this
/// test starts and whose success it asserts; the coverage counts of the child are kept by the
/// profile file it inherits.
#[test]
fn a_process_out_of_descriptors_fails_an_open_with_a_real_emfile_and_recovers() {
    let exe = std::env::current_exe().unwrap();
    // The name libtest knows the child by: the module path without the name of the crate.
    let here = module_path!().split_once("::").unwrap().1;
    let name = format!("{}::running_out_of_descriptors_fails_the_open", here);
    let output = Command::new(exe)
        .args([
            "--exact",
            &name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, "1")
        .output()
        .unwrap();
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{out}\n{err}");
    // The child ran the test, and did not find nothing to run.
    assert!(out.contains("1 passed"), "{out}\n{err}");
}

/// The body of [`PARENT`]. It lowers the soft limit of descriptors of the process, so it runs in a
/// process of its own, started by that test, which sets the marker; it refuses to run otherwise.
#[test]
#[ignore = "it lowers the process's descriptor limit, so it runs alone in a process of its own, started by a_process_out_of_descriptors_fails_an_open_with_a_real_emfile_and_recovers"]
#[expect(
    clippy::disallowed_methods,
    reason = "the marker the parent test sets for the child it starts: a test's own plumbing, not configuration"
)]
fn running_out_of_descriptors_fails_the_open() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    assert!(
        std::env::var_os(CHILD_MARKER).is_some(),
        "this test runs only as the child of {PARENT}"
    );
    let (rig, path) = rig_with_blob("tfs-fds-emfile", 3000);
    let core = &rig.core;
    // A handle opened before; and the tool, not open yet, is what fails. Its first open and release
    // are before the limit: the fake node writes the file of the cache for the fill it asks (it is
    // in this process, and would run out of descriptors too), and the core keeps the connection
    // of the fill socket, which a new one would not be had for.
    let before = open_cached(core, ino::of(1, 3), false).unwrap();
    let expected = std::fs::read(&path).unwrap();
    let target = ino::of(1, 4);
    core.release(open_cached(core, target, false).unwrap());
    let warm_fills = rig.upstream.asked("fill").len();
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    let was = getrlimit(Resource::Nofile);
    // The limit is the number after the highest descriptor, and the free numbers below it are
    // taken: the next descriptor the process asks for is one too many.
    let highest = numbers().into_iter().max().unwrap();
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(highest + 1),
            maximum: was.maximum,
        },
    )
    .unwrap();
    let mut held = Vec::new();
    let refused = loop {
        match File::open("/dev/null") {
            Ok(file) => held.push(file),
            Err(error) => break error,
        }
    };
    assert_eq!(refused.raw_os_error(), Some(24), "{refused}");
    let failed = open_cached(core, target, false);
    // Descriptors free again: the same open succeeds.
    drop(held);
    setrlimit(Resource::Nofile, was).unwrap();
    let after = open_cached(core, target, false);
    drop(guard);
    assert_eq!(failed, Err(Errno::Io), "{}", log.text());
    assert_eq!(core.descriptor_failures(), 1);
    assert_eq!(core.read(before, 0, 3000).unwrap(), expected);
    assert!(after.is_ok(), "{after:?}");
    let failures = core.detach(RUN).unwrap();
    let digest = failures
        .first()
        .map(|failure| failure.digest.clone())
        .unwrap_or_default();
    assert_eq!(
        failures,
        [Failure {
            path: "/bin/tool".into(),
            digest,
            reason: NO_DESCRIPTOR_REASON.into(),
        }]
    );
    let said = log.text();
    assert!(said.contains(NO_DESCRIPTOR_REASON), "{said}");
    assert!(
        said.contains(&format!("(after {NO_DESCRIPTOR_TRIES} fills)")),
        "{said}"
    );
    assert_eq!(
        rig.upstream.asked("fill").len() - warm_fills,
        NO_DESCRIPTOR_FILLS
    );
}
