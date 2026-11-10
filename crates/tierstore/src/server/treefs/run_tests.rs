//! The process: a start over a real mount and a real control socket, a stop, the checks a start
//! makes, the notification of the service manager, and the signals.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

use super::{
    ECRYPTFS, OVERLAYFS, Prepared, Running, Signals, StartError, check_cache, new_mount_id,
    refuse_stacked, stacked_name,
};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::testing::{Rig, SIGNALS, sample, with_fuse};

/// Settings over the scratch directories of a rig, with the sample tree to be served.
fn settings(rig: &Rig) -> FsSettings {
    rig.settings()
}

fn mounted(mountpoint: &std::path::Path) -> bool {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    info.lines()
        .any(|line| line.contains(mountpoint.to_str().unwrap()))
}

fn ask(path: &std::path::Path, request: &str) -> Value {
    let stream = UnixStream::connect(path).unwrap();
    (&stream)
        .write_all(format!("{request}\n").as_bytes())
        .unwrap();
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Asserts a whole start: the mount is made, a client attaches over the control socket and a
/// process reads the tree through the mount; a stop unmounts and the attachments file is kept; and a
/// second start over the same runtime directory attaches again what the file holds, tells the cache
/// which trees are attached, and leaves out what it cannot read.
#[test]
fn a_start_mounts_attaches_and_serves_and_a_stop_unmounts() {
    with_fuse(start_and_stop);
}

fn start_and_stop() {
    let rig = Rig::new("tfs-run-start", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let settings = settings(&rig);
    // The backing table of a mount that is gone belongs to a connection that is gone.
    std::fs::write(rig.runtime.join("backing"), "stale\n").unwrap();
    let running = Running::start(&settings).unwrap();
    assert!(mounted(&rig.mountpoint));
    let table = std::fs::read_to_string(rig.runtime.join("backing")).unwrap_or_default();
    assert!(!table.contains("stale"), "{table}");
    let answer = ask(
        &settings.control_socket,
        &format!("attach {root} for run-1"),
    );
    assert_eq!(answer["ok"], json!(true));
    let path = answer["path"].as_str().unwrap().to_owned();
    assert_eq!(
        std::fs::read(format!("{path}/include/a.h")).unwrap(),
        b"int a;"
    );
    assert_eq!(running.core().attachments().len(), 1);
    running.stop().unwrap();
    assert!(!mounted(&rig.mountpoint));
    let kept = std::fs::read_to_string(rig.runtime.join("attachments")).unwrap();
    assert_eq!(kept, format!("{root} run-1\n"));
    // A line of the file whose tree cannot be had is not attached again.
    let gone = "ab".repeat(32);
    std::fs::write(
        rig.runtime.join("attachments"),
        format!("{kept}{gone} run-2\n"),
    )
    .unwrap();
    let again = Running::start(&settings).unwrap();
    let left: Vec<String> = again
        .core()
        .attachments()
        .into_iter()
        .map(|(_, run)| run)
        .collect();
    assert_eq!(left, ["run-1"]);
    assert_eq!(
        rig.upstream
            .asked("pins-live")
            .last()
            .unwrap()
            .split(' ')
            .next_back()
            .unwrap(),
        root
    );
    again.stop().unwrap();
}

/// Asserts that a fresh start detaches a dead mount before it makes the directories: a dead mount
/// answers `mkdir` with `EEXIST` and `stat` with `ENOTCONN`, so `create_dir_all` fails on it. The
/// mount point is not there when the detaching runs, and is there after.
#[test]
fn a_fresh_start_detaches_a_dead_mount_before_it_makes_the_directories() {
    let rig = Rig::new("tfs-run-dead", sample());
    let settings = FsSettings {
        mountpoint: rig.dir.path().join("fresh/mnt"),
        ..settings(&rig)
    };
    let mut ran = false;
    super::begin(&settings, || {
        ran = true;
        assert!(!settings.mountpoint.exists());
    })
    .unwrap();
    assert!(ran);
    assert!(settings.mountpoint.is_dir());
}

/// Asserts that a start that cannot do what it needs is an error that says what: a cache directory
/// that is not there, a runtime directory or a mount point under a file, a control socket in a
/// directory that is not there, a mount point that is not a directory.
#[test]
fn a_start_that_cannot_do_what_it_needs_says_what() {
    let rig = Rig::new("tfs-run-refused", sample());
    let base = settings(&rig);
    let file = rig.dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let cases: Vec<(FsSettings, &str)> = vec![
        (
            FsSettings {
                cache_path: rig.dir.path().join("none"),
                ..base.clone()
            },
            "fs.cache_path",
        ),
        (
            FsSettings {
                runtime_dir: file.join("run"),
                ..base.clone()
            },
            "fs.runtime_dir",
        ),
        (
            FsSettings {
                mountpoint: file.join("mnt"),
                ..base.clone()
            },
            "fs.mountpoint",
        ),
        (
            FsSettings {
                control_socket: rig.dir.path().join("no/dir/c.sock"),
                ..base.clone()
            },
            "fs.control_socket",
        ),
        (
            FsSettings {
                mountpoint: file.clone(),
                ..base.clone()
            },
            "fs.mountpoint",
        ),
    ];
    for (settings, what) in cases {
        let error = Running::start(&settings).err().unwrap();
        assert!(error.to_string().contains(what), "{what}: {error}");
        assert_eq!(error.status(), 1);
    }
    assert!(!mounted(&rig.mountpoint));
}

/// Asserts that a start that cannot remove the backing table of an earlier mount says so and
/// mounts nothing.
#[test]
fn a_backing_table_that_cannot_be_removed_stops_the_start() {
    let rig = Rig::new("tfs-run-stale", sample());
    let stuck = rig.runtime.join("backing");
    std::fs::create_dir_all(stuck.join("inside")).unwrap();
    let error = Running::start(&settings(&rig)).err().unwrap();
    assert!(error.to_string().contains("removing"), "{error}");
    assert!(!mounted(&rig.mountpoint));
}

/// Asserts the file systems the cache is refused on: overlayfs and eCryptfs, by name and with the
/// status 3 of a cache directory that fails its check; and a cache on an ordinary file system
/// passes.
#[test]
fn a_cache_on_a_stacked_file_system_is_refused_by_name() {
    assert_eq!(stacked_name(OVERLAYFS), Some("overlayfs"));
    assert_eq!(stacked_name(ECRYPTFS), Some("eCryptfs"));
    assert_eq!(stacked_name(0xef53), None);
    let error = refuse_stacked(std::path::Path::new("/cache"), OVERLAYFS).unwrap_err();
    assert!(matches!(
        error,
        StartError::Stacked {
            name: "overlayfs",
            ..
        }
    ));
    assert_eq!(error.status(), 3);
    assert!(
        error
            .to_string()
            .starts_with("fs.cache_path /cache: the file system is overlayfs")
    );
    assert!(refuse_stacked(std::path::Path::new("/cache"), ECRYPTFS).is_err());
    assert!(refuse_stacked(std::path::Path::new("/cache"), 0xef53).is_ok());
    let rig = Rig::new("tfs-run-cache", sample());
    check_cache(&rig.cache).unwrap();
}

/// Asserts that a mount id is thirty-two hex digits and two are not alike.
#[test]
fn a_mount_id_is_random_hex() {
    let (a, b) = (new_mount_id().unwrap(), new_mount_id().unwrap());
    assert_eq!(a.len(), 32);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

/// Asserts that SIGTERM and SIGINT, each, end the wait, and that one that came before the wait is
/// not lost.
#[test]
fn sigterm_and_sigint_end_the_wait_even_if_they_came_first() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut signals = Signals::install().unwrap();
        for signal in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            signals.wait().await;
        }
    });
}

/// Asserts that a start that makes the mount records the handshake: generation 1, the id the pins
/// are made under, and the outcome the kernel and the frontend agreed on.
#[test]
fn a_start_records_the_handshake_for_the_process_that_takes_the_connection_over() {
    with_fuse(records_the_handshake);
}

fn records_the_handshake() {
    let rig = Rig::new("tfs-run-record", sample());
    let settings = settings(&rig);
    let running = Running::start(&settings).unwrap();
    let record = crate::server::treefs::init_record::read(&rig.runtime).unwrap();
    assert_eq!(record.generation, 1);
    assert_eq!(record.mount_id.len(), 32);
    assert_eq!(record.outcome.kernel_abi.0, 7);
    assert_eq!(record.outcome, running.outcome());
    // Where the kernel offers resend the process asks for it, so that a takeover can use it.
    let offered = record.outcome.capabilities;
    assert_eq!(
        record
            .outcome
            .flags
            .contains(fuser::InitFlags::FUSE_HAS_RESEND),
        offered.contains(fuser::InitFlags::FUSE_HAS_RESEND)
    );
    assert!(record.outcome.max_write > 0);
    running.stop().unwrap();
}

/// Asserts that the answer of a mount whose connection has ended is told from the others.
#[test]
fn the_answer_of_a_dead_connection_is_told_from_other_errors() {
    use super::not_connected;
    assert!(not_connected(&std::io::Error::from_raw_os_error(107)));
    assert!(!not_connected(&std::io::Error::from_raw_os_error(2)));
    assert!(!not_connected(&std::io::Error::other("no os error")));
}

/// Asserts what is done to a mount that is to be discarded: its connection is aborted if asked, then
/// the mount is detached; a mount point that is not a FUSE mount is left alone; and a connection
/// that cannot be aborted, or a mount that cannot be detached, is said and not fatal.
#[test]
fn a_discarded_mount_is_aborted_if_asked_and_detached() {
    use super::{Kernel, discard_with};
    use std::cell::RefCell;
    let rig = Rig::new("tfs-run-discard", sample());
    let info = format!(
        "36 24 0:45 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let table = rig.dir.path().join("mountinfo");
    std::fs::write(&table, info).unwrap();
    let connections = rig.dir.path().join("connections");
    std::fs::create_dir_all(connections.join("45")).unwrap();
    let kernel = Kernel {
        mountinfo: table,
        connections: connections.clone(),
    };
    let detached = RefCell::new(Vec::new());
    let detach = |path: &std::path::Path| {
        detached.borrow_mut().push(path.to_owned());
        Ok(())
    };
    discard_with(&rig.mountpoint, (&kernel, false), &detach);
    assert!(!connections.join("45/abort").exists());
    assert_eq!(*detached.borrow(), std::slice::from_ref(&rig.mountpoint));
    discard_with(&rig.mountpoint, (&kernel, true), &detach);
    assert_eq!(
        std::fs::read_to_string(connections.join("45/abort")).unwrap(),
        "1"
    );
    assert_eq!(detached.borrow().len(), 2);
    // Not a FUSE mount: nothing is done.
    discard_with(std::path::Path::new("/elsewhere"), (&kernel, true), &detach);
    assert_eq!(detached.borrow().len(), 2);
    // Neither failure stops the other step.
    std::fs::remove_dir_all(connections.join("45")).unwrap();
    let failing = |_: &std::path::Path| Err(std::io::Error::other("busy"));
    discard_with(&rig.mountpoint, (&kernel, true), &failing);
    // The files of the machine are named.
    let real = Kernel::real();
    assert_eq!(real.mountinfo, std::path::Path::new("/proc/self/mountinfo"));
    assert_eq!(
        real.connection_of(std::path::Path::new("/does/not/exist")),
        None
    );
    // A mount table that is not there has no connection.
    let none = Kernel {
        mountinfo: rig.dir.path().join("none"),
        connections,
    };
    assert_eq!(none.connection_of(&rig.mountpoint), None);
}

/// Asserts that the real detach is tried on a path that is not a mount and its refusal is only
/// said.
#[test]
fn a_mount_that_cannot_be_detached_is_said_and_the_start_goes_on() {
    use super::{Kernel, discard_mount};
    let rig = Rig::new("tfs-run-detach", sample());
    let table = rig.dir.path().join("mountinfo");
    let info = format!(
        "36 24 0:46 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    std::fs::write(&table, info).unwrap();
    let kernel = Kernel {
        mountinfo: table,
        connections: rig.dir.path().join("connections"),
    };
    discard_mount(&settings(&rig), true, &kernel);
    assert!(!mounted(&rig.mountpoint));
}

/// Asserts that a mount point that answers `ENOTCONN` has its mount discarded (without aborting a
/// connection that is already dead), and that any other answer leaves it alone.
#[test]
fn only_a_mount_point_that_answers_not_connected_is_discarded() {
    use super::{Kernel, discard_dead_mount};
    let rig = Rig::new("tfs-run-dead", sample());
    let table = rig.dir.path().join("mountinfo");
    let info = format!(
        "36 24 0:47 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    std::fs::write(&table, info).unwrap();
    let connections = rig.dir.path().join("connections");
    std::fs::create_dir_all(connections.join("47")).unwrap();
    let kernel = Kernel {
        mountinfo: table,
        connections: connections.clone(),
    };
    let settings = settings(&rig);
    let dead = Err(std::io::Error::from_raw_os_error(107));
    discard_dead_mount(&settings, dead, &kernel);
    assert!(
        !connections.join("47/abort").exists(),
        "the connection is dead already"
    );
    let alive = std::fs::metadata(&rig.mountpoint);
    discard_dead_mount(&settings, alive, &kernel);
    let other = Err(std::io::Error::from_raw_os_error(2));
    discard_dead_mount(&settings, other, &kernel);
}

/// Detaches a mount that a test left behind, without waiting for a server.
fn detach(mountpoint: &std::path::Path) {
    let status = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(mountpoint)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Asserts that a process that takes over a live mount reaches ready and serves it: the first
/// process mounts and leaves (the readers stop, the mount stays), and a second adopts the
/// descriptor. The root of the mount has no cache time, so a `stat` of the mount point is a request
/// the new process must answer itself; an adoption that made the mount point first waited on its
/// own readers, which were not started, and never finished.
#[test]
fn a_takeover_of_a_live_mount_serves_a_stat_of_the_mount_point() {
    with_fuse(takeover_of_live_mount);
}

fn takeover_of_live_mount() {
    let rig = Rig::new("tfs-run-live", sample());
    let settings = settings(&rig);
    let first = Running::start(&settings).unwrap();
    let device = first.device().unwrap();
    first.leave();
    assert!(mounted(&rig.mountpoint));
    let prepared = Prepared::load(&settings).unwrap();
    let second = Running::adopt(&settings, prepared, device, (None, false)).unwrap();
    assert!(std::fs::metadata(&rig.mountpoint).unwrap().is_dir());
    assert!(
        std::fs::metadata(rig.mountpoint.join("trees"))
            .unwrap()
            .is_dir()
    );
    second.stop().unwrap();
    detach(&rig.mountpoint);
}
