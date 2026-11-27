//! The hand-over of the mount over a real FUSE mount while a job reads: what a process that reads
//! files through the mount sees when the running `tierstore fs` hands the mount to a new one, and
//! when a take-over fails. The two instances are two `serve_with` calls on threads of the test,
//! each with a stand-in for its service manager (`FakeManager`), over the real mount of one
//! rig's settings; the reader is a thread that reads every file of the sample tree through the
//! mount again and again and compares the bytes to what `sample` wrote.
//!
//! A file can also be mapped (`vm-memory`'s `MmapRegion`, a private read-only mapping) before the
//! hand-over and read through the mapping after it.
//!
//! Every wait is on an event: the manager's `READY=1`, the end of a serve thread, the hand-over
//! socket's file, and the number of rounds the reader has finished. A mount needs `/dev/fuse` and
//! `fusermount3` (the mounting user is not root); where either is missing a test says so on
//! stderr and ends (`with_fuse`).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use vm_memory::{FileOffset, MmapRegion, VolatileMemory as _};

use super::config::FsSettings;
use super::handover::{self, HandoverError, Paths};
use super::init_record;
use super::launch::{Platform, serve_with};
use super::run::Kernel;
use super::sd::Manager;
use super::start::StartError;
use super::testing::{FakeManager, Rig, SIGNALS, sample, until, with_fuse};
use crate::Mono;

/// The files of the sample tree and the bytes `sample` writes in them.
fn expected() -> Vec<(&'static str, Vec<u8>)> {
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    vec![
        ("big.bin", big),
        ("bin/cc", cc),
        ("bin/tool", b"tool bytes".to_vec()),
        ("empty.h", Vec::new()),
        ("include/a.h", b"int a;".to_vec()),
    ]
}

/// All the bytes of `path`, read through the mount; the pages the kernel kept of it are then
/// dropped, so that the next read of the file asks the process that serves the mount again.
fn read_through(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes)?;
    forget_pages(&file);
    Ok(bytes)
}

/// Drops the pages of `file` the kernel holds, so that a read of it asks the process.
fn forget_pages(file: &std::fs::File) {
    let _ = rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed);
}

/// A reader: a thread that reads every file of the sample tree through the mount, round after
/// round, until it is told to stop or a file is not what it should be.
struct Reader {
    stop: Arc<AtomicBool>,
    rounds: Arc<AtomicUsize>,
    thread: JoinHandle<Result<(), String>>,
}

impl Reader {
    /// Starts reading the files below `at`.
    fn start(at: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicUsize::new(0));
        let (halt, counted) = (Arc::clone(&stop), Arc::clone(&rounds));
        let thread = std::thread::spawn(move || {
            let files = expected();
            while !halt.load(Ordering::Acquire) {
                for (name, want) in &files {
                    match read_through(&at.join(name)) {
                        Ok(got) if &got == want => {}
                        Ok(got) => {
                            return Err(format!(
                                "{name}: {} bytes read, other than the {} written",
                                got.len(),
                                want.len()
                            ));
                        }
                        Err(why) => return Err(format!("{name}: {why}")),
                    }
                }
                let _ = counted.fetch_add(1, Ordering::AcqRel);
            }
            Ok(())
        });
        Self {
            stop,
            rounds,
            thread,
        }
    }

    /// Waits until the reader has read every file `more` times over, from now: the rounds that
    /// end after this call. A reader that failed ends the wait.
    fn after(&self, more: usize) {
        let target = self.rounds.load(Ordering::Acquire) + more;
        until(|| self.rounds.load(Ordering::Acquire) >= target || self.thread.is_finished());
    }

    /// Stops the reader; why it failed, if it did.
    fn finish(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("the reader does not panic")
    }
}

/// The mount at a path, lazily detached when the test ends, however it ends.
struct Mount(PathBuf);

impl Mount {
    fn present(&self) -> bool {
        let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        info.lines()
            .any(|line| line.contains(self.0.to_str().unwrap_or_default()))
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.present() {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(&self.0)
                .status();
        }
    }
}

/// A platform whose service manager is at `manager`, if one is, over the real kernel.
fn platform(manager: Option<&Path>) -> Platform {
    Platform {
        manager: Manager::new(manager.map(Path::as_os_str)),
        stored: Vec::new(),
        kernel: Kernel::real(),
        clock: Arc::new(Mono::new()),
    }
}

/// An instance serving on a thread of its own.
fn serving(settings: &FsSettings, manager: &FakeManager) -> JoinHandle<Result<(), StartError>> {
    let (settings, platform) = (settings.clone(), platform(Some(manager.socket())));
    std::thread::spawn(move || serve_with(&settings, platform))
}

/// Waits until the manager is told the unit is ready; a serve thread that ended first fails the
/// test with what it ended with.
fn until_ready(manager: &FakeManager, thread: &JoinHandle<Result<(), StartError>>) {
    until(|| manager.texts().iter().any(|text| text == "READY=1") || thread.is_finished());
    assert!(
        !thread.is_finished(),
        "the instance ended before it was ready"
    );
}

fn sigterm() {
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
}

/// Where the files of the sample tree are below the mount point of the rig: `trees/<digest>`,
/// which a lookup of the digest attaches.
fn tree_at(rig: &Rig) -> PathBuf {
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.mountpoint.join("trees").join(root)
}

/// A rig of the sample tree whose entries belong to the user of the test.
fn rig(name: &str) -> Rig {
    let me = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    Rig::owned(name, sample(), me)
}

/// Asserts the hand-over of the mount to a new instance while a reader reads through it: the
/// reader gets every byte of every file in every round, before, during and after the hand-over;
/// the old instance ends with success; the new one tells its manager it is ready, holds the next
/// generation of the connection and answers the reads after; and a file the reader opened and
/// half read before the hand-over is read whole after it. Catches a hand-over that loses a request
/// the old instance read, answers it wrongly, or leaves an open handle of the old process unread.
#[test]
fn a_reader_gets_every_byte_while_the_mount_is_handed_to_a_new_instance() {
    with_fuse(hand_over_under_read);
}

fn hand_over_under_read() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = rig("tfs-handover-read");
    let mount = Mount(rig.mountpoint.clone());
    let settings = rig.settings();
    let old_manager = FakeManager::start(&rig.dir.path().join("old.sock"));
    let old = serving(&settings, &old_manager);
    until_ready(&old_manager, &old);
    assert!(mount.present());
    let at = tree_at(&rig);

    // A file opened, and half read, by the process the mount is taken from.
    let (name, whole) = expected().remove(0);
    let mut held = std::fs::File::open(at.join(name)).unwrap();
    let mut first = vec![0u8; whole.len() / 2];
    held.read_exact(&mut first).unwrap();
    assert_eq!(first, whole[..first.len()]);
    forget_pages(&held);

    let reader = Reader::start(at);
    reader.after(2);
    let new_manager = FakeManager::start(&rig.dir.path().join("new.sock"));
    let new = serving(&settings, &new_manager);
    until_ready(&new_manager, &new);
    // The old instance ends by itself, with success, once the new one serves.
    old.join().unwrap().unwrap();
    // The reads from here on are answered by the new instance alone.
    reader.after(2);
    assert_eq!(
        new_manager.texts(),
        ["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"]
    );
    // The old instance took its descriptor out of its store at `ready`, before its `bye`.
    until(|| {
        old_manager.texts().last().map(String::as_str) == Some("FDSTOREREMOVE=1\nFDNAME=fuse")
    });
    assert_eq!(
        init_record::read(&rig.runtime).unwrap().generation,
        2,
        "the second process of the connection"
    );
    assert!(
        settings.handoff_socket.exists(),
        "the new instance can be taken from"
    );
    let mut rest = Vec::new();
    let _ = held.read_to_end(&mut rest).unwrap();
    assert_eq!(
        rest,
        whole[first.len()..],
        "the rest of the file opened before"
    );
    reader.after(1);
    assert_eq!(reader.finish(), Ok(()));
    sigterm();
    new.join().unwrap().unwrap();
    assert!(mount.present(), "the store keeps the mount");
}

/// Asserts that a file mapped through the mount before the hand-over, as a compiler maps its
/// inputs, is read whole through its mapping after it: every page, none of which the kernel held
/// at the hand-over, is answered by the new instance with the file's bytes, and the process that
/// holds the mapping takes no fault (a page the new instance answered with an error would end it
/// with SIGBUS). Catches a hand-over that leaves the pages of a mapped file unreadable.
#[test]
fn a_file_mapped_before_the_hand_over_is_read_through_its_mapping_after_it() {
    with_fuse(mapped_across_the_hand_over);
}

fn mapped_across_the_hand_over() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = rig("tfs-handover-mapped");
    let mount = Mount(rig.mountpoint.clone());
    let settings = rig.settings();
    let old_manager = FakeManager::start(&rig.dir.path().join("old.sock"));
    let old = serving(&settings, &old_manager);
    until_ready(&old_manager, &old);
    let at = tree_at(&rig);

    // Each file that has bytes, mapped by way of the process the mount is taken from, and no page
    // of it read.
    let mapped: Vec<_> = expected()
        .into_iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .map(|(name, bytes)| {
            let file = std::fs::File::open(at.join(name)).unwrap();
            forget_pages(&file);
            let map = MmapRegion::<()>::build(
                Some(FileOffset::new(file, 0)),
                bytes.len(),
                libc::PROT_READ,
                libc::MAP_PRIVATE,
            )
            .unwrap();
            (name, map, bytes)
        })
        .collect();

    let new_manager = FakeManager::start(&rig.dir.path().join("new.sock"));
    let new = serving(&settings, &new_manager);
    until_ready(&new_manager, &new);
    old.join().unwrap().unwrap();
    for (name, map, bytes) in &mapped {
        let mut read = vec![0u8; bytes.len()];
        let copied = map.get_slice(0, read.len()).unwrap().copy_to(&mut read[..]);
        assert_eq!(copied, bytes.len(), "{name}: every page");
        assert_eq!(&read, bytes, "{name} through its mapping");
    }
    drop(mapped);
    sigterm();
    new.join().unwrap().unwrap();
    assert!(mount.present(), "the store keeps the mount");
}

/// Asserts that take-overs that fail before the new instance says it has the descriptor leave
/// the old instance serving a reader: one that names another control socket is answered `nak`
/// after the offer and its start returns the error that says what the old instance serves, with
/// nothing put in its manager's store; one whose manager cannot store the descriptor returns the
/// error that says so; after each the reader has read every file once more, every byte equal to
/// what was written, the old instance has not ended, and its hand-over socket is there. Catches a
/// refused take-over that stops the old instance reading, or leaves it with no hand-over socket.
#[test]
fn a_take_over_that_is_refused_leaves_the_old_instance_serving_the_reader() {
    with_fuse(refused_take_over);
}

fn refused_take_over() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = rig("tfs-handover-refused");
    let mount = Mount(rig.mountpoint.clone());
    let settings = rig.settings();
    let old_manager = FakeManager::start(&rig.dir.path().join("old.sock"));
    let old = serving(&settings, &old_manager);
    until_ready(&old_manager, &old);
    let reader = Reader::start(tree_at(&rig));
    reader.after(2);

    // Another control socket than the one the old instance serves.
    let other = FsSettings {
        control_socket: rig.dir.path().join("other-control.sock"),
        ..settings.clone()
    };
    let refused_manager = FakeManager::start(&rig.dir.path().join("refused.sock"));
    let error = serve_with(&other, platform(Some(refused_manager.socket()))).unwrap_err();
    match &error {
        StartError::Handover(HandoverError::Mismatch { .. }) => {}
        other => panic!("{other}"),
    }
    assert!(
        error.to_string().contains("the running instance serves"),
        "{error}"
    );
    assert_eq!(refused_manager.texts(), Vec::<String>::new());
    reader.after(2);
    assert!(!old.is_finished());
    assert!(settings.handoff_socket.exists());

    // A manager that cannot be reached cannot store the descriptor.
    let nobody = rig.dir.path().join("nobody-home");
    let error = serve_with(&settings, platform(Some(&nobody))).unwrap_err();
    match &error {
        StartError::Handover(HandoverError::Unprepared(_)) => {}
        other => panic!("{other}"),
    }
    assert!(error.to_string().contains("cannot be stored"), "{error}");
    reader.after(2);
    assert!(!old.is_finished());
    assert!(settings.handoff_socket.exists());
    assert_eq!(
        init_record::read(&rig.runtime).unwrap().generation,
        1,
        "the old instance is still the first process of the connection"
    );

    assert_eq!(reader.finish(), Ok(()));
    sigterm();
    old.join().unwrap().unwrap();
    assert!(mount.present(), "the store keeps the mount");
}

/// Asserts that a take-over that fails after the old instance stopped reading, because the taker
/// is gone before it says `ready`, leaves the old instance serving the reader: the old instance
/// made its hand-over socket again, the reader reads every file in full once more, every byte
/// equal to what was written, and a file opened before the take-over is read to its end. Catches a
/// resume that leaves a request the old instance did not answer, or a reader that stays stopped.
#[test]
fn a_taker_gone_before_ready_leaves_the_old_instance_serving_the_reader() {
    with_fuse(taker_gone);
}

fn taker_gone() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = rig("tfs-handover-gone");
    let mount = Mount(rig.mountpoint.clone());
    let settings = rig.settings();
    let old_manager = FakeManager::start(&rig.dir.path().join("old.sock"));
    let old = serving(&settings, &old_manager);
    until_ready(&old_manager, &old);
    let at = tree_at(&rig);
    let (name, whole) = expected().remove(0);
    let mut held = std::fs::File::open(at.join(name)).unwrap();
    let mut first = vec![0u8; whole.len() / 2];
    held.read_exact(&mut first).unwrap();
    forget_pages(&held);
    let reader = Reader::start(at);
    reader.after(2);

    // The taker takes the descriptor and the offer, is told `done`, and goes: the socket file
    // of the old instance is removed before `done` and made again when it serves again.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let paths = Paths {
        socket: &settings.handoff_socket,
        mountpoint: &settings.mountpoint,
        control: &settings.control_socket,
        wait: settings.handoff_wait,
        device: handover::check_device,
    };
    let taken = runtime
        .block_on(handover::take(&paths, Box::new(|_| Ok(()))))
        .unwrap();
    assert!(taken.is_some(), "the old instance handed the mount over");
    assert!(
        !settings.handoff_socket.exists(),
        "told done, it has no socket"
    );
    drop(taken);
    until(|| settings.handoff_socket.exists());
    reader.after(2);
    assert!(!old.is_finished());
    let mut rest = Vec::new();
    let _ = held.read_to_end(&mut rest).unwrap();
    assert_eq!(
        rest,
        whole[first.len()..],
        "the rest of the file opened before"
    );
    assert_eq!(
        init_record::read(&rig.runtime).unwrap().generation,
        1,
        "no one else served the connection"
    );
    assert_eq!(reader.finish(), Ok(()));
    sigterm();
    old.join().unwrap().unwrap();
    assert!(mount.present(), "the store keeps the mount");
}
