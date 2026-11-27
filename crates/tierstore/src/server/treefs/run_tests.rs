//! The process: a start over a real mount and a real control socket, a stop, the checks a start
//! makes, the notification of the service manager, and the signals.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

use super::{
    ECRYPTFS, Kernel, OVERLAYFS, Prepared, Running, check_cache, refuse_stacked, stacked_name,
};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::start::StartError;
use crate::server::treefs::testing::{PACK, Rig, sample, sha, with_fuse};

/// Settings over the scratch directories of a rig, with the sample tree to be served.
fn settings(rig: &Rig) -> FsSettings {
    rig.settings()
}

/// Fills the blob `content` of the sample tree into the rig's cache, and pins it for `tree` of
/// `mount`; the bytes the cache then holds pinned.
fn pinned_for(rig: &Rig, (mount, tree): (&str, &str), content: &[u8]) -> u64 {
    let digest = sha(content);
    let offset = rig.upstream.built.lock().unwrap().offsets[&digest];
    let asks = rig.asks();
    asks.fill_blob(
        (digest.as_str(), content.len() as u64),
        (PACK, offset),
        None,
    )
    .unwrap();
    assert_eq!(
        asks.pin(mount, tree, &[digest.as_str()]).unwrap(),
        "ok 1 0 0"
    );
    rig.node.pinned_bytes()
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
    // The backing table an earlier release kept for a mount that is gone belongs to a connection
    // that is gone; a fresh mount has no backing id and writes no table.
    std::fs::write(rig.runtime.join("backing"), "stale\n").unwrap();
    let running = Running::start((&settings, &rig.calls()), &Kernel::real()).unwrap();
    assert!(mounted(&rig.mountpoint));
    assert!(!rig.runtime.join("backing").exists());
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
    let again = Running::start((&settings, &rig.calls()), &Kernel::real()).unwrap();
    let left: Vec<String> = again
        .core()
        .attachments()
        .into_iter()
        .map(|(_, run)| run)
        .collect();
    assert_eq!(left, ["run-1"]);
    again.core().settled().unwrap();
    // The cache is told the mount's attached trees: what the mount pinned for the tree it holds
    // stays pinned, and what it pinned for one it does not hold is taken away.
    let mount = crate::server::treefs::init_record::read(&rig.runtime)
        .unwrap()
        .mount_id;
    let kept = pinned_for(&rig, (&mount, &root), b"int a;");
    let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    assert!(pinned_for(&rig, (&mount, "not-attached"), &cc) > kept);
    again.core().tell_live().unwrap();
    assert_eq!(rig.node.pinned_bytes(), kept);
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
        let error = Running::start((&settings, &rig.calls()), &Kernel::real())
            .err()
            .unwrap();
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
    let error = Running::start((&settings(&rig), &rig.calls()), &Kernel::real())
        .err()
        .unwrap();
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

/// Asserts that a start that makes the mount records the handshake: generation 1, the id the pins
/// are made under, and the outcome the kernel and the frontend agreed on.
#[test]
fn a_start_records_the_handshake_for_the_process_that_takes_the_connection_over() {
    with_fuse(records_the_handshake);
}

fn records_the_handshake() {
    let rig = Rig::new("tfs-run-record", sample());
    let settings = settings(&rig);
    let running = Running::start((&settings, &rig.calls()), &Kernel::real()).unwrap();
    let record = crate::server::treefs::init_record::read(&rig.runtime).unwrap();
    assert_eq!(record.generation, 1);
    assert_eq!(record.mount_id.len(), 32);
    assert_eq!(record.outcome.kernel_abi.0, 7);
    assert_eq!(record.outcome, running.outcome());
    // Where the kernel offers resend the process asks for it, so that a takeover can use it.
    let offered = record.outcome.capabilities;
    let resend = tsfuse::abi::InitFlags::HAS_RESEND;
    assert_eq!(
        record.outcome.flags.contains(resend),
        offered.contains(resend)
    );
    assert!(record.outcome.max_write > 0);
    running.stop().unwrap();
}

/// The kernel's files as a test makes them, with the table of mounts `info`.
fn kernel_of(rig: &Rig, info: &str) -> Kernel {
    let table = rig.dir.path().join("mountinfo");
    std::fs::write(&table, info).unwrap();
    let connections = rig.dir.path().join("connections");
    std::fs::create_dir_all(&connections).unwrap();
    Kernel {
        mountinfo: table,
        connections,
        device: |_| Ok(()),
    }
}

/// Asserts that a fresh start finds a mount no process serves in the kernel's table of mounts, and
/// never looks at the mount point: the mount point here is not there at all (a look at it would
/// say `ENOENT`, which the look of the release before took for a mount point with no dead mount,
/// and left the mount), and the mount is aborted and detached all the same, with nothing made at
/// or above the mount point. A mount of another file system at the mount point, and a mount of
/// this one elsewhere, are left.
#[test]
fn a_fresh_start_finds_a_dead_mount_in_mountinfo_without_touching_it() {
    use super::discard_with;
    use std::cell::RefCell;
    let rig = Rig::new("tfs-run-unserved", sample());
    let mountpoint = rig.dir.path().join("absent/mnt");
    let info = format!(
        "35 24 0:44 / {0} rw - fuse.sshfs x rw\n36 24 0:45 / {0} rw - fuse.tierstore-fs x rw\n",
        mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    std::fs::create_dir_all(kernel.connections.join("45")).unwrap();
    let detached = RefCell::new(Vec::new());
    let detach = |path: &std::path::Path| {
        detached.borrow_mut().push(path.to_owned());
        Ok(())
    };
    discard_with(&mountpoint, &kernel, &detach);
    assert_eq!(
        std::fs::read_to_string(kernel.connections.join("45/abort")).unwrap(),
        "1"
    );
    assert_eq!(*detached.borrow(), std::slice::from_ref(&mountpoint));
    assert!(!rig.dir.path().join("absent").exists(), "nothing is made");

    // A mount of another file system at the mount point is left; so is a mount of this one at
    // another path.
    let sshfs = format!(
        "35 24 0:44 / {} rw - fuse.sshfs x rw\n",
        mountpoint.display()
    );
    let other = kernel_of(&rig, &sshfs);
    std::fs::create_dir_all(other.connections.join("44")).unwrap();
    discard_with(&mountpoint, &other, &detach);
    assert!(!other.connections.join("44/abort").exists());
    let elsewhere = kernel_of(&rig, &info);
    discard_with(std::path::Path::new("/elsewhere"), &elsewhere, &detach);
    assert_eq!(detached.borrow().len(), 1);
}

/// Asserts that neither a connection that cannot be aborted nor a mount that cannot be detached
/// stops the other step or the start (each is said on stderr, which the test lets the lines be
/// written to); and the files of the machine that are read.
#[test]
fn a_mount_that_cannot_be_aborted_or_detached_is_said_and_the_start_goes_on() {
    use super::discard_with;
    use std::cell::Cell;
    let rig = Rig::new("tfs-run-discard", sample());
    let info = format!(
        "36 24 0:45 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    // No directory of the connection: it cannot be aborted.
    let kernel = kernel_of(&rig, &info);
    let log = crate::server::treefs::testing::Logged::default();
    let guard = crate::server::treefs::testing::logged(&log, tracing::Level::WARN);
    let tried = Cell::new(0);
    let failing = |_: &std::path::Path| {
        tried.set(tried.get() + 1);
        Err(std::io::Error::other("busy"))
    };
    discard_with(&rig.mountpoint, &kernel, &failing);
    discard_with(&rig.mountpoint, &kernel, &failing);
    drop(guard);
    assert_eq!(
        tried.get(),
        2,
        "the detach is tried though the abort failed"
    );
    assert!(!kernel.connections.join("45").exists());
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
        connections: kernel.connections.clone(),
        device: |_| Ok(()),
    };
    assert_eq!(none.connection_of(&rig.mountpoint), None);
    assert_eq!(none.tierstore_connection_of(&rig.mountpoint), None);
}

/// Asserts that the real detach is tried on a path that is not a mount and its refusal is only
/// said.
#[test]
fn a_mount_that_cannot_be_detached_is_said_and_the_start_goes_on() {
    use super::discard_unserved;
    let rig = Rig::new("tfs-run-detach", sample());
    let info = format!(
        "36 24 0:46 / {} rw - fuse.tierstore-fs x rw\n",
        rig.mountpoint.display()
    );
    let kernel = kernel_of(&rig, &info);
    discard_unserved(&settings(&rig), &kernel);
    assert!(!mounted(&rig.mountpoint));
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
    let first = Running::start((&settings, &rig.calls()), &Kernel::real()).unwrap();
    let device = first.device().unwrap();
    first.leave();
    assert!(mounted(&rig.mountpoint));
    let prepared = Prepared::load(&settings).unwrap();
    let second =
        Running::adopt((&settings, &rig.calls()), prepared, device, (None, false)).unwrap();
    assert!(std::fs::metadata(&rig.mountpoint).unwrap().is_dir());
    assert!(
        std::fs::metadata(rig.mountpoint.join("trees"))
            .unwrap()
            .is_dir()
    );
    second.stop().unwrap();
    detach(&rig.mountpoint);
}
