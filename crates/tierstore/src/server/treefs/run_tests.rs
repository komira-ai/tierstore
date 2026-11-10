//! The process: a start over a real mount and a real control socket, a stop, the checks a start
//! makes, the notification of the service manager, and the signals.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::Mutex;

use serde_json::{Value, json};

use super::{
    ECRYPTFS, OVERLAYFS, Running, Signals, StartError, check_cache, new_mount_id, notify_ready,
    refuse_stacked, serve, serve_notifying, stacked_name,
};
use crate::server::treefs::config::FsSettings;
use crate::server::treefs::testing::{Rig, sample, with_fuse};

/// Tests that send signals to the process go one at a time: a signal is for every handler.
static SIGNALS: Mutex<()> = Mutex::new(());

/// Settings over the scratch directories of a rig, with the sample tree to be served.
fn settings(rig: &Rig) -> FsSettings {
    FsSettings {
        mountpoint: rig.mountpoint.clone(),
        runtime_dir: rig.runtime.clone(),
        control_socket: rig.dir.path().join("control.sock"),
        client_group: std::fs::metadata(&rig.runtime).unwrap().gid(),
        owner: (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        ),
        fill_socket: rig.dir.path().join("fill.sock"),
        cache_path: rig.cache.clone(),
        fetch_timeout: std::time::Duration::from_secs(5),
        threads: 4,
        allow_other: false,
    }
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
    let running = Running::start(&settings).unwrap();
    assert!(mounted(&rig.mountpoint));
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
            .last()
            .unwrap(),
        root
    );
    again.stop().unwrap();
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

/// Asserts that the service manager is told `READY=1` at a path socket and at an abstract one,
/// that nothing is sent without a socket, and that a socket that cannot be reached is only said.
#[test]
fn the_service_manager_is_told_the_unit_is_ready() {
    use std::os::linux::net::SocketAddrExt as _;
    let rig = Rig::new("tfs-run-notify", sample());
    let path = rig.dir.path().join("notify.sock");
    let socket = UnixDatagram::bind(&path).unwrap();
    notify_ready(Some(path.as_os_str()));
    let mut buffer = [0u8; 16];
    let got = socket.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..got], b"READY=1");
    let name = format!("tierstore-fs-test-{}", rig.dir.path().display());
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let abstract_socket = UnixDatagram::bind_addr(&address).unwrap();
    notify_ready(Some(std::ffi::OsStr::new(&format!("@{name}"))));
    let got = abstract_socket.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..got], b"READY=1");
    notify_ready(None);
    notify_ready(Some(rig.dir.path().join("nobody-home").as_os_str()));
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

/// Asserts the process: it starts, tells the service manager, serves, and a SIGTERM unmounts it and
/// ends it with success; and that a start that fails ends `serve` with the error.
#[test]
fn serve_runs_until_sigterm_and_unmounts() {
    with_fuse(serve_and_sigterm);
}

fn serve_and_sigterm() {
    let _one_at_a_time = SIGNALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rig = Rig::new("tfs-run-serve", sample());
    let settings = settings(&rig);
    let notify = rig.dir.path().join("notify.sock");
    let ready = UnixDatagram::bind(&notify).unwrap();
    let (mine, path) = (settings.clone(), notify.clone());
    let thread = std::thread::spawn(move || serve_notifying(&mine, Some(path.as_os_str())));
    // The notice comes after the mount is made, and the handlers were in place before it.
    let mut buffer = [0u8; 16];
    let got = ready.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..got], b"READY=1");
    assert!(mounted(&rig.mountpoint));
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
    thread.join().unwrap().unwrap();
    assert!(!mounted(&rig.mountpoint));
    let bad = FsSettings {
        cache_path: rig.dir.path().join("none"),
        ..settings.clone()
    };
    assert!(serve(&bad).is_err());
}
