//! The service manager's side: the descriptors it passes, the messages it is sent, and the
//! duplicate of a descriptor of the process itself.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::UnixDatagram;

use rustix::pipe::{PipeFlags, pipe_with};
use tierstore_testkit::dir::TestDir;

use super::{Environment, FUSE_NAME, Manager, Stored, environment, take};
use crate::server::treefs::testing::FakeManager;

/// The environment of a unit whose descriptor store holds `names`, started as process `pid`.
fn passed(pid: &str, count: &str, names: Option<&str>) -> Environment {
    Environment {
        notify_socket: None,
        listen_pid: Some(pid.to_owned()),
        listen_fds: Some(count.to_owned()),
        listen_fdnames: names.map(str::to_owned),
    }
}

/// Asserts that the descriptors passed to the process are numbered from 3 and named in order, and
/// that a name the manager did not give is `unknown`.
#[test]
fn the_descriptors_passed_to_the_process_are_numbered_from_three_and_named() {
    let env = passed("42", "2", Some("fuse:other"));
    assert_eq!(
        env.stored(42),
        vec![
            Stored {
                name: "fuse".to_owned(),
                fd: 3
            },
            Stored {
                name: "other".to_owned(),
                fd: 4
            },
        ]
    );
    let unnamed = passed("42", "2", Some("fuse"));
    assert_eq!(unnamed.stored(42)[1].name, "unknown");
    assert_eq!(passed("42", "1", None).stored(42)[0].name, "unknown");
}

/// Asserts that descriptors meant for another process, and a count that is not one, pass nothing:
/// a child of the unit's process inherits the variables and must not take the unit's descriptors.
#[test]
fn descriptors_meant_for_another_process_or_with_no_count_are_none() {
    assert!(passed("41", "1", Some("fuse")).stored(42).is_empty());
    assert!(passed("forty-two", "1", Some("fuse")).stored(42).is_empty());
    assert!(passed("42", "many", Some("fuse")).stored(42).is_empty());
    assert!(passed("42", "0", Some("fuse")).stored(42).is_empty());
    assert!(Environment::default().stored(42).is_empty());
    // The process's own id is the one `stored_here` compares.
    let this = rustix::process::getpid().as_raw_nonzero().get().to_string();
    assert_eq!(passed(&this, "1", Some("fuse")).stored_here().len(), 1);
    assert!(passed("1", "1", Some("fuse")).stored_here().is_empty());
}

/// Asserts that the four variables are read by their names, and that a value that is not text is
/// none (the notify socket is a path and may be any bytes).
#[test]
fn the_four_variables_of_the_manager_are_read_by_name() {
    use std::os::unix::ffi::OsStringExt as _;
    let set: std::collections::HashMap<&str, std::ffi::OsString> = [
        ("NOTIFY_SOCKET", std::ffi::OsString::from("/run/notify")),
        ("LISTEN_PID", std::ffi::OsString::from("42")),
        ("LISTEN_FDS", std::ffi::OsString::from_vec(vec![0xff, 0xfe])),
        ("LISTEN_FDNAMES", std::ffi::OsString::from("fuse")),
    ]
    .into_iter()
    .collect();
    let env = Environment::read(|name| set.get(name).cloned());
    assert_eq!(
        env.notify_socket,
        Some(std::ffi::OsString::from("/run/notify"))
    );
    assert_eq!(env.listen_pid.as_deref(), Some("42"));
    assert_eq!(env.listen_fds, None);
    assert_eq!(env.listen_fdnames.as_deref(), Some("fuse"));
    assert_eq!(Environment::read(|_| None), Environment::default());
    // What the process itself was started with is read the same way.
    let _ = environment();
}

/// Asserts that a descriptor of the process is taken as an owned duplicate of the same open file,
/// that the original stays open, and that a number that is not open is an error.
#[test]
fn a_descriptor_of_the_process_is_taken_as_an_owned_duplicate() {
    let (read, write) = pipe_with(PipeFlags::CLOEXEC).unwrap();
    let taken = take(read.as_raw_fd()).unwrap();
    std::fs::File::from(write).write_all(b"hello").unwrap();
    let mut bytes = [0u8; 5];
    std::fs::File::from(taken).read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"hello");
    assert!(read.as_raw_fd() >= 0);
    assert!(take(i32::MAX).is_err());
}

/// Asserts that a process without a manager sends nothing, waits for nothing and has none to tell.
#[test]
fn without_a_manager_nothing_is_sent_and_nothing_is_waited_for() {
    let none = Manager::new(None);
    assert!(!none.present());
    none.ready().unwrap();
    none.barrier().unwrap();
    none.unstore(FUSE_NAME).unwrap();
    let (read, _write) = pipe_with(PipeFlags::CLOEXEC).unwrap();
    none.store(FUSE_NAME, read.as_fd()).unwrap();
}

/// Asserts the messages: ready, a descriptor stored under a name (the descriptor arrives as the
/// same open file), and a name taken out of the store; and that a barrier returns after the manager
/// has handled every message sent before it.
#[test]
fn the_manager_is_told_ready_given_a_descriptor_and_waited_for() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-sd");
    let manager = FakeManager::start(&dir.path().join("notify.sock"));
    let channel = Manager::new(Some(manager.socket().as_os_str()));
    assert!(channel.present());
    let (read, write) = pipe_with(PipeFlags::CLOEXEC).unwrap();
    channel.store(FUSE_NAME, write.as_fd()).unwrap();
    channel.ready().unwrap();
    channel.unstore(FUSE_NAME).unwrap();
    channel.barrier().unwrap();
    assert_eq!(
        manager.texts(),
        [
            "FDSTORE=1\nFDNAME=fuse",
            "READY=1",
            "FDSTOREREMOVE=1\nFDNAME=fuse",
            "BARRIER=1"
        ]
    );
    let stored = manager.take_fds("FDSTORE=1\nFDNAME=fuse");
    assert_eq!(stored.len(), 1);
    std::fs::File::from(stored.into_iter().next().unwrap())
        .write_all(b"via the store")
        .unwrap();
    drop(write);
    let mut text = String::new();
    std::fs::File::from(read).read_to_string(&mut text).unwrap();
    assert_eq!(text, "via the store");
}

/// Asserts that an abstract socket, named with a leading `@`, is reached as a path one is.
#[test]
fn an_abstract_socket_is_reached() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-sd-abstract");
    let name = format!("tierstore-fs-test-{}", dir.path().display());
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let socket = UnixDatagram::bind_addr(&address).unwrap();
    Manager::new(Some(std::ffi::OsStr::new(&format!("@{name}"))))
        .ready()
        .unwrap();
    let mut buffer = [0u8; 16];
    let got = socket.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..got], b"READY=1");
}

/// Asserts that a socket nobody listens on is an error of the send, and that more descriptors than
/// a message may carry are refused by name.
#[test]
fn a_manager_that_cannot_be_reached_and_too_many_descriptors_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "tfs-sd-errors");
    let nobody = Manager::new(Some(dir.path().join("nobody-home").as_os_str()));
    assert!(nobody.ready().is_err());
    assert!(nobody.barrier().is_err());
    let manager = FakeManager::start(&dir.path().join("notify.sock"));
    let channel = Manager::new(Some(manager.socket().as_os_str()));
    let stdin = std::io::stdin();
    let many = vec![stdin.as_fd(); 300];
    let error = channel.send("FDSTORE=1", &many).unwrap_err();
    assert!(error.to_string().contains("too many"), "{error}");
}
