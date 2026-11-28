//! The process: whether it adopts the mount or makes it, what it tells the service manager,
//! and what ends it. Over a real mount where one can be made; otherwise over a stand-in for the
//! kernel's end of `/dev/fuse` (`fakedev.rs`).

use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::sync::Arc;

use super::{End, Platform, ended, hang, serve_with};
use crate::Mono;
use crate::server::handoff::testing::within;
use crate::server::local_cache::testing as cache_rig;
use crate::server::local_cache::{Mode, Tick};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fakedev;
use crate::server::treefs::fills::Calls;
use crate::server::treefs::init_record::{self, Record};
use crate::server::treefs::run::{Kernel, Prepared, Running};
use crate::server::treefs::sd::{FUSE_NAME, Manager, Stored};
use crate::server::treefs::start::StartError;
use crate::server::treefs::testing::{
    FakeManager, Logged, Rig, SIGNALS, logged, sample, until, with_fuse,
};

/// The kernel's files, as a test makes them: a mount table and a directory of connections.
fn kernel_of(rig: &Rig, mountinfo: &str) -> Kernel {
    let table = rig.dir.path().join("mountinfo");
    std::fs::write(&table, mountinfo).unwrap();
    let connections = rig.dir.path().join("connections");
    std::fs::create_dir_all(&connections).unwrap();
    Kernel {
        mountinfo: table,
        connections,
        // The stand-in for the device is a socket.
        device: |_| Ok(()),
    }
}

/// A platform with the manager at `manager` and the descriptor `stored` under the name `fuse`.
fn platform(manager: &FakeManager, stored: Option<i32>, kernel: Kernel) -> Platform {
    Platform {
        manager: Manager::new(Some(manager.socket().as_os_str())),
        stored: stored
            .into_iter()
            .map(|fd| Stored {
                name: FUSE_NAME.to_owned(),
                fd,
            })
            .collect(),
        kernel,
        clock: Arc::new(Mono::new()),
    }
}

/// Waits for the manager to be told the unit is ready.
fn until_ready(manager: &FakeManager) {
    until(|| manager.texts().iter().any(|text| text == "READY=1"));
}

fn sigterm() {
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
}

fn mounted(mountpoint: &Path) -> bool {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    info.lines()
        .any(|line| line.contains(mountpoint.to_str().unwrap()))
}

/// Detaches a mount that a test left for the manager to keep, without waiting for a server.
fn detach(mountpoint: &Path) {
    let status = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(mountpoint)
        .status()
        .unwrap();
    assert!(status.success());
}

/// The record a process that mounted left, for a stand-in connection.
fn leave_record(rig: &Rig, resend: bool) {
    let record = Record::first(fakedev::outcome(resend), "mount-1".to_owned());
    init_record::write(&rig.runtime, &record).unwrap();
}

/// Asserts a start that makes the mount: the descriptor is put in the service manager's store and
/// the manager has it before it is told ready; and a SIGTERM stops the
/// readers and leaves the mount, for the store to keep, without unmounting it.
#[test]
fn a_fresh_start_stores_the_descriptor_says_ready_and_a_signal_leaves_the_mount() {
    with_fuse(fresh_start);
}

fn fresh_start() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-fresh", sample());
    let settings = rig.settings();
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    let platform = platform(&manager, None, Kernel::real());
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until_ready(&manager);
    assert_eq!(
        manager.texts(),
        ["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"]
    );
    assert_eq!(manager.take_fds("FDSTORE=1\nFDNAME=fuse").len(), 1);
    assert!(mounted(&rig.mountpoint));
    assert_eq!(
        init_record::read(&rig.runtime).unwrap().generation,
        1,
        "the first process of the mount"
    );
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(mounted(&rig.mountpoint), "the store keeps the mount");
    detach(&rig.mountpoint);
}

/// Asserts that a start that finds no descriptor and no manager unmounts at a signal, as a process
/// run by hand does.
#[test]
fn a_start_with_no_service_manager_unmounts_at_a_signal() {
    with_fuse(no_manager);
}

fn no_manager() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-alone", sample());
    let settings = rig.settings();
    let platform = Platform {
        manager: Manager::new(None),
        stored: Vec::new(),
        kernel: Kernel::real(),
        clock: Arc::new(Mono::new()),
    };
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until(|| mounted(&rig.mountpoint));
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(!mounted(&rig.mountpoint));
}

/// Asserts a restart: the descriptor the manager kept is served from the recorded outcome with no
/// mount and no handshake, a request queued meanwhile is answered, where the kernel can send
/// requests again it is asked to, the descriptor is not stored again, and a signal leaves the
/// connection for the store.
#[test]
fn a_restart_serves_the_descriptor_the_manager_kept_and_asks_for_the_requests_again() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-restart", sample());
    leave_record(&rig, true);
    let settings = rig.settings();
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    let (device, kernel) = fakedev::pair();
    kernel.getattr(5, 1);
    let platform = platform(&manager, Some(device.as_raw_fd()), kernel_of(&rig, ""));
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until_ready(&manager);
    let first = kernel.reply();
    let second = kernel.reply();
    let (note, answer) = if first.unique == 0 {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!((note.error, answer.unique, answer.error), (7, 5, 0));
    assert_eq!(manager.texts(), ["READY=1"]);
    assert_eq!(init_record::read(&rig.runtime).unwrap().generation, 2);
    sigterm();
    thread.join().unwrap().unwrap();
    drop(device);
}

/// Asserts that a start tells the service manager it is ready while the node's cache is read-only
/// (another process holds the lock of its directory, as after a boot, when the node's cache reads
/// its whole directory before it takes the write role), so that the mount's pins, which wait for
/// the write role, have not been answered at `READY=1`. Once the cache takes the lock, the process
/// serves on and ends at the signal. Each step of the start is said on stderr as it ends, in order,
/// and the launch's own steps and the time to ready on the platform's clock, which the test does
/// not move: 0 ms. Catches a start that waits for the write role of the cache before it says
/// ready.
#[test]
fn a_start_says_ready_before_the_cache_takes_the_pins() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-ready-first", sample());
    leave_record(&rig, true);
    let mut settings = rig.settings();
    // Long enough that the pins wait for the test, not for the core's own time.
    settings.fetch_timeout = std::time::Duration::from_secs(600);
    let work = rig.work.handle();
    let _entered = work.enter();
    let node = cache_rig::Rig::new("tfs-launch-ready-cache");
    let holder = node.locked();
    let waiting = node.locked();
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    let calls = Calls {
        cache: waiting.clone(),
        trees: None,
        runtime: rig.work.handle(),
    };
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    let (device, _kernel) = fakedev::pair();
    let platform = platform(&manager, Some(device.as_raw_fd()), kernel_of(&rig, ""));
    let mine = settings.clone();
    let log = Logged::default();
    let logging = log.clone();
    let thread = std::thread::spawn(move || {
        let _logging = logged(&logging, tracing::Level::INFO);
        serve_with((&mine, &calls), platform)
    });
    until_ready(&manager);
    assert_eq!(waiting.mode(), Mode::ReadOnly, "ready with no write role");
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    assert_eq!(waiting.mode(), Mode::ReadWrite);
    sigterm();
    thread.join().unwrap().unwrap();
    drop(device);
    let text = log.text();
    let timed: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(" took ") || line.contains(": ready after "))
        .map(|line| line.split(" took ").next().unwrap_or_default())
        .collect();
    assert_eq!(
        timed,
        [
            "tierstore fs: making the directories",
            "tierstore fs: making the 0 attachments of the file again",
            "tierstore fs: setting up the core",
            "tierstore fs: opening the put directory",
            "tierstore fs: serving the connection",
            "tierstore fs: serving the descriptor the service manager kept",
            "tierstore fs: ready after 0 ms",
        ]
    );
    let line = "tierstore fs: serving the descriptor the service manager kept took 0 ms\n";
    assert!(text.contains(line), "{line:?} in {text}");
}

/// Asserts the bound of a restart on a kernel that cannot send requests again: the connection is
/// watched for `fs.recover_wait`; one that never shows nothing waiting is aborted, taken out of the
/// store, and the process ends in failure so that the unit starts again; one that shows nothing
/// waiting, and one the kernel does not show, are left alone.
#[test]
fn a_restart_without_resend_aborts_a_connection_that_stays_hung() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-hung", sample());
    leave_record(&rig, false);
    let mut settings = rig.settings();
    settings.recover_wait = std::time::Duration::from_secs(1);
    let info = format!(
        "36 24 0:77 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    std::fs::create_dir_all(kernel.connections.join("77")).unwrap();
    std::fs::write(kernel.connections.join("77/waiting"), "2\n").unwrap();
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    let (device, _kernel) = fakedev::pair();
    let aborting = platform(&manager, Some(device.as_raw_fd()), kernel.clone());
    let error = serve_with((&settings, &rig.calls()), aborting).unwrap_err();
    assert!(matches!(error, StartError::Aborted));
    assert_eq!(error.status(), 1);
    assert_eq!(
        std::fs::read_to_string(kernel.connections.join("77/abort")).unwrap(),
        "1"
    );
    assert!(
        manager
            .texts()
            .contains(&"FDSTOREREMOVE=1\nFDNAME=fuse".to_owned())
    );

    // Nothing waits: the watch ends and the process serves until the signal.
    std::fs::write(kernel.connections.join("77/waiting"), "0\n").unwrap();
    std::fs::remove_file(kernel.connections.join("77/abort")).unwrap();
    let watching = FakeManager::start(&rig.dir.path().join("watching.sock"));
    let platform = platform(&watching, Some(device.as_raw_fd()), kernel);
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until_ready(&watching);
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(!rig.dir.path().join("connections/77/abort").exists());
}

/// Asserts that a restart on a kernel that can send requests again, which refuses to, is watched as
/// one that cannot: the requests the process before read stay unanswered, and the connection is
/// aborted when they do not clear.
#[test]
fn a_restart_whose_resend_is_refused_is_watched_and_aborted_when_the_requests_stay() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-refused", sample());
    leave_record(&rig, true);
    let mut settings = rig.settings();
    settings.recover_wait = std::time::Duration::from_secs(1);
    let info = format!(
        "36 24 0:79 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    std::fs::create_dir_all(kernel.connections.join("79")).unwrap();
    std::fs::write(kernel.connections.join("79/waiting"), "3\n").unwrap();
    let (device, gone) = fakedev::pair();
    // The kernel's end is gone, so the notification cannot be written.
    drop(gone);
    let platform = Platform {
        manager: Manager::new(None),
        stored: vec![Stored {
            name: FUSE_NAME.to_owned(),
            fd: device.as_raw_fd(),
        }],
        kernel: kernel.clone(),
        clock: Arc::new(Mono::new()),
    };
    let error = serve_with((&settings, &rig.calls()), platform).unwrap_err();
    assert!(matches!(error, StartError::Aborted));
    assert_eq!(
        std::fs::read_to_string(kernel.connections.join("79/abort")).unwrap(),
        "1"
    );
}

/// Asserts that a connection the kernel does not show is served without a watch.
#[test]
fn a_restart_on_a_connection_the_kernel_does_not_show_is_served_unwatched() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-unshown", sample());
    leave_record(&rig, false);
    let settings = rig.settings();
    let info = format!(
        "36 24 0:78 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    let (device, _kernel) = fakedev::pair();
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    let platform = platform(&manager, Some(device.as_raw_fd()), kernel);
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until_ready(&manager);
    sigterm();
    thread.join().unwrap().unwrap();
}

/// Asserts that a descriptor whose state cannot be read is not served: it is taken out of the
/// store, its connection is aborted and detached where there is a mount, and the mount is made afresh.
#[test]
fn a_descriptor_whose_state_cannot_be_read_is_dropped_and_the_mount_made_afresh() {
    with_fuse(|| dropped_and_remade(false));
}

/// Asserts the same for a descriptor number that is not open in the process.
#[test]
fn a_descriptor_that_cannot_be_taken_is_dropped_and_the_mount_made_afresh() {
    with_fuse(|| dropped_and_remade(true));
}

fn dropped_and_remade(bogus: bool) {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-dropped", sample());
    let settings = rig.settings();
    let info = format!(
        "36 24 0:79 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    std::fs::create_dir_all(kernel.connections.join("79")).unwrap();
    let manager = FakeManager::start(&rig.dir.path().join("notify.sock"));
    // No record was left, so the descriptor cannot be served; or it is not a descriptor.
    let (device, _kernel) = fakedev::pair();
    let number = if bogus { i32::MAX } else { device.as_raw_fd() };
    let platform = platform(&manager, Some(number), kernel.clone());
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until_ready(&manager);
    assert_eq!(
        manager.texts(),
        [
            "FDSTOREREMOVE=1\nFDNAME=fuse",
            "FDSTORE=1\nFDNAME=fuse",
            "BARRIER=1",
            "READY=1"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(kernel.connections.join("79/abort")).unwrap(),
        "1"
    );
    assert!(mounted(&rig.mountpoint));
    sigterm();
    thread.join().unwrap().unwrap();
    detach(&rig.mountpoint);
}

/// Asserts that a process whose mount a successor took ends with success and leaves its state
/// whole, for the end of the process to release: the core is not taken apart (a weak reference to
/// it still reaches it after the end), and a part of it still held elsewhere (by the session of a
/// control client) is no reason to fail, as it is for the end at a signal, which takes the state
/// apart. Catches an end that frees the whole state one allocation
/// at a time, which took minutes on a node whose process held tens of gigabytes, and one that
/// ends with an error because a client was connected.
#[tokio::test]
async fn after_a_hand_over_the_state_is_left_whole_to_the_end_of_the_process() {
    let rig = Rig::new("tfs-launch-left", sample());
    leave_record(&rig, false);
    let settings = rig.settings();
    let (device, _kernel) = fakedev::pair();
    let prepared = Prepared::load(&settings).unwrap();
    let running = Arc::new(
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap(),
    );
    let state = Arc::downgrade(&running);
    let manager = Manager::new(None);

    // A signal, unlike a hand-over, takes the state apart, and needs it whole.
    let session = Arc::clone(&running);
    let error = ended(End::Signal, Arc::clone(&running), &manager).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("a hand-over is still being answered"),
        "{error}"
    );
    drop(session);

    ended(End::Handed, running, &manager).unwrap();
    let left = state.upgrade();
    assert!(left.is_some(), "the state was taken apart");
    ended(End::Handed, state.upgrade().unwrap(), &manager).unwrap();
    drop(left);
}

/// Asserts that the wait for a hang is over when the watch reports one, and goes on for ever when it
/// reports none or there is no watch.
#[tokio::test]
async fn the_wait_for_a_hang_ends_only_when_one_is_reported() {
    let (hung, report) = tokio::sync::oneshot::channel();
    hung.send(()).unwrap();
    within("the report", hang(Some(report))).await;
    let (silent, report) = tokio::sync::oneshot::channel::<()>();
    drop(silent);
    tokio::select! {
        biased;
        () = hang(Some(report)) => unreachable!("a watch that ended without a report"),
        () = std::future::ready(()) => {}
    }
    tokio::select! {
        biased;
        () = hang(None) => unreachable!("no watch"),
        () = std::future::ready(()) => {}
    }
}

/// Asserts that the process, started with no service manager, refuses a start it cannot make and says
/// what.
#[test]
fn a_start_that_cannot_be_made_is_the_error_of_serve() {
    let rig = Rig::new("tfs-launch-refused", sample());
    let settings = FsSettings {
        cache_path: rig.dir.path().join("none"),
        ..rig.settings()
    };
    let platform = Platform {
        manager: Manager::new(None),
        stored: Vec::new(),
        kernel: Kernel::real(),
        clock: Arc::new(Mono::new()),
    };
    let error = serve_with((&settings, &rig.calls()), platform).unwrap_err();
    assert!(error.to_string().contains("fs.cache_path"), "{error}");
}

/// Asserts that a service manager that cannot be reached is said and does not stop the process: the
/// mount is made, the descriptor is not stored, the unit is not said ready, and a signal leaves the
/// mount.
#[test]
fn a_service_manager_that_cannot_be_reached_does_not_stop_the_process() {
    with_fuse(unreachable_manager);
}

fn unreachable_manager() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-nomanager", sample());
    let settings = rig.settings();
    let platform = Platform {
        manager: Manager::new(Some(rig.dir.path().join("nobody-home").as_os_str())),
        stored: vec![Stored {
            name: FUSE_NAME.to_owned(),
            fd: i32::MAX,
        }],
        kernel: kernel_of(&rig, ""),
        clock: Arc::new(Mono::new()),
    };
    let mine = settings.clone();
    let calls = rig.calls();
    let thread = std::thread::spawn(move || serve_with((&mine, &calls), platform));
    until(|| mounted(&rig.mountpoint));
    sigterm();
    thread.join().unwrap().unwrap();
    detach(&rig.mountpoint);
}
