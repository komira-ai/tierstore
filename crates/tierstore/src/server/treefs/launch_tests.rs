//! The process: whether it takes the mount over or makes it, what it tells the service manager,
//! and what ends it. Over a real mount where one can be made; otherwise over a stand-in for the
//! kernel's end of `/dev/fuse` (`fakedev.rs`).

use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Platform, hang, serve, serve_with};
use crate::server::handoff::testing::within;
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::fakedev;
use crate::server::treefs::init_record::{self, Record};
use crate::server::treefs::run::{Kernel, StartError};
use crate::server::treefs::sd::{FUSE_NAME, Manager, Stored};
use crate::server::treefs::testing::{FakeManager, Rig, SIGNALS, sample, until, with_fuse};

/// The kernel's files, as a test makes them: a mount table and a directory of connections.
fn kernel_of(rig: &Rig, mountinfo: &str) -> Kernel {
    let table = rig.dir.path().join("mountinfo");
    std::fs::write(&table, mountinfo).unwrap();
    let connections = rig.dir.path().join("connections");
    std::fs::create_dir_all(&connections).unwrap();
    Kernel {
        mountinfo: table,
        connections,
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
/// the manager has it before it is told ready; the hand-over socket is bound; and a SIGTERM stops the
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
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
    until_ready(&manager);
    assert_eq!(
        manager.texts(),
        ["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"]
    );
    assert_eq!(manager.take_fds("FDSTORE=1\nFDNAME=fuse").len(), 1);
    assert!(mounted(&rig.mountpoint));
    assert!(settings.handoff_socket.exists());
    assert_eq!(
        init_record::read(&rig.runtime).unwrap().generation,
        1,
        "the first process of the mount"
    );
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(mounted(&rig.mountpoint), "the store keeps the mount");
    assert!(!settings.handoff_socket.exists());
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
    };
    let mine = settings.clone();
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
    until(|| settings.handoff_socket.exists());
    assert!(mounted(&rig.mountpoint));
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
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
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
    let platform = platform(&manager, Some(device.as_raw_fd()), kernel.clone());
    let error = serve_with(&settings, platform).unwrap_err();
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
    let platform = super::Platform {
        manager: Manager::new(None),
        stored: vec![Stored {
            name: FUSE_NAME.to_owned(),
            fd: device.as_raw_fd(),
        }],
        kernel,
    };
    let mine = settings.clone();
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
    until(|| mine_exists(&settings));
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(!rig.dir.path().join("connections/77/abort").exists());
}

fn mine_exists(settings: &FsSettings) -> bool {
    settings.handoff_socket.exists()
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
    let platform = Platform {
        manager: Manager::new(None),
        stored: vec![Stored {
            name: FUSE_NAME.to_owned(),
            fd: device.as_raw_fd(),
        }],
        kernel,
    };
    let mine = settings.clone();
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
    until(|| settings.handoff_socket.exists());
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
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
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

/// Asserts a start that takes the mount from a running instance: the descriptor goes into the
/// manager's store before the running instance is told to stop, the mount is served from the
/// recorded outcome, the manager is told ready, and the running instance ends.
#[tokio::test]
async fn a_start_takes_the_mount_from_the_running_instance() {
    let old = fakedev::instance(
        "tfs-launch-take",
        std::time::Duration::from_secs(3600),
        None,
    );
    old.assert_serving(1);
    let manager = FakeManager::start(&old.rig.dir.path().join("notify.sock"));
    let platform = platform(&manager, None, Kernel::real());
    let settings = old.settings.clone();
    let ended = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&ended);
    let thread = std::thread::spawn(move || {
        let _one_at_a_time = SIGNALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let served = serve_with(&settings, platform);
        done.store(true, Ordering::Release);
        served
    });
    within("the running instance ends", old.serving)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        manager.texts()[..2],
        ["FDSTORE=1\nFDNAME=fuse", "BARRIER=1"]
    );
    until_ready(&manager);
    // The successor answers the kernel.
    old.kernel.getattr(9, 1);
    let reply = old.kernel.reply();
    assert_eq!((reply.unique, reply.error), (9, 0));
    assert_eq!(init_record::read(&old.rig.runtime).unwrap().generation, 3);
    sigterm();
    thread.join().unwrap().unwrap();
    assert!(ended.load(Ordering::Acquire));
    drop(old.running);
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

/// Asserts that the process, started by hand, refuses a start it cannot make and says what.
#[test]
fn a_start_that_cannot_be_made_is_the_error_of_serve() {
    let rig = Rig::new("tfs-launch-refused", sample());
    let settings = FsSettings {
        cache_path: rig.dir.path().join("none"),
        ..rig.settings()
    };
    let error = serve(&settings).unwrap_err();
    assert!(error.to_string().contains("fs.cache_path"), "{error}");
}

/// Asserts the hand-over between two processes of this release over one connection: the second
/// takes the mount from the first, which ends with success once the second serves; the second puts
/// the descriptor in its own manager's store and says ready to it; and the kernel's requests are
/// answered by the second.
#[test]
fn a_hand_over_between_two_processes_ends_the_first_with_success() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-launch-two", sample());
    leave_record(&rig, false);
    let settings = rig.settings();
    let (device, kernel) = fakedev::pair();
    let first_manager = FakeManager::start(&rig.dir.path().join("first.sock"));
    let first = platform(
        &first_manager,
        Some(device.as_raw_fd()),
        kernel_of(&rig, ""),
    );
    let mine = settings.clone();
    let first_thread = std::thread::spawn(move || serve_with(&mine, first));
    until_ready(&first_manager);
    kernel.getattr(1, 1);
    assert_eq!(kernel.reply().unique, 1);

    let second_manager = FakeManager::start(&rig.dir.path().join("second.sock"));
    let second = platform(&second_manager, None, Kernel::real());
    let mine = settings.clone();
    let second_thread = std::thread::spawn(move || serve_with(&mine, second));
    until_ready(&second_manager);
    first_thread.join().unwrap().unwrap();
    assert_eq!(
        second_manager.texts(),
        ["FDSTORE=1\nFDNAME=fuse", "BARRIER=1", "READY=1"]
    );
    kernel.getattr(2, 1);
    assert_eq!(kernel.reply().unique, 2);
    assert_eq!(init_record::read(&rig.runtime).unwrap().generation, 3);
    sigterm();
    second_thread.join().unwrap().unwrap();
    drop(device);
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
    };
    let mine = settings.clone();
    let thread = std::thread::spawn(move || serve_with(&mine, platform));
    until(|| settings.handoff_socket.exists());
    assert!(mounted(&rig.mountpoint));
    sigterm();
    thread.join().unwrap().unwrap();
    detach(&rig.mountpoint);
}

/// Asserts that a start that is offered the mount and cannot store the descriptor takes nothing:
/// it fails by name, and the running instance goes on serving.
#[tokio::test]
async fn a_start_that_cannot_store_the_descriptor_leaves_the_running_instance_serving() {
    let old = fakedev::instance(
        "tfs-launch-nostore",
        std::time::Duration::from_secs(3600),
        None,
    );
    let platform = Platform {
        manager: Manager::new(Some(old.rig.dir.path().join("nobody-home").as_os_str())),
        stored: Vec::new(),
        kernel: Kernel::real(),
    };
    let settings = old.settings.clone();
    let refused = tokio::task::spawn_blocking(move || serve_with(&settings, platform))
        .await
        .unwrap();
    match refused {
        Err(StartError::Handover(why)) => {
            assert!(why.to_string().contains("cannot be stored"), "{why}");
        }
        other => panic!("{:?}", other.err()),
    }
    old.assert_serving(1);
    assert!(old.settings.handoff_socket.exists());
}
