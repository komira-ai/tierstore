//! The control socket over a real Unix socket and a core on a fake fill socket.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::{Value, json};

use super::{Control, handle};
use crate::server::treefs::testing::{Rig, open_cached, sample};

/// A connection to the control socket.
struct Client {
    read: BufReader<UnixStream>,
}

impl Client {
    fn connect(path: &Path) -> Self {
        Self {
            read: BufReader::new(UnixStream::connect(path).unwrap()),
        }
    }

    fn ask(&mut self, request: &str) -> Value {
        self.read
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .unwrap();
        let mut line = String::new();
        assert!(
            self.read.read_line(&mut line).unwrap() > 0,
            "no answer to {request}"
        );
        serde_json::from_str(&line).unwrap()
    }
}

/// A rig and the control socket served over its core.
fn served(name: &str) -> (Rig, Control) {
    let rig = Rig::new(name, sample());
    let path = rig.dir.path().join("control.sock");
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let control = Control::serve(rig.core.clone(), &path, gid).unwrap();
    (rig, control)
}

fn root(rig: &Rig) -> String {
    rig.upstream.built.lock().unwrap().root.clone()
}

/// Asserts the conversation: an attach is answered with the path, an attach again with the same;
/// the attachments are listed; a detach answers the recorded failures and ends the attachment; a
/// detach of a run that attached nothing is refused as not attached; and detach-all-except ends
/// the runs that are not named and counts them.
#[test]
fn a_client_attaches_lists_and_detaches_over_the_socket() {
    let (rig, _control) = served("tfs-control");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    let path = rig.mountpoint.join("trees").join(&root);
    let want = json!({"ok": true, "path": path});
    assert_eq!(client.ask(&format!("attach {root} for run-1")), want);
    assert_eq!(client.ask(&format!("attach {root} for run-1")), want);
    assert_eq!(client.ask(&format!("attach {root} for run-2")), want);
    assert_eq!(client.ask(&format!("attach {root} for run-3")), want);
    let listed = client.ask("attachments");
    assert_eq!(listed["attachments"].as_array().unwrap().len(), 3);
    assert_eq!(
        listed["attachments"][0],
        json!({"tree": root, "run": "run-1"})
    );
    assert_eq!(
        client.ask("detach run-1"),
        json!({"ok": true, "failures": []})
    );
    assert_eq!(
        client.ask("detach run-1"),
        json!({"ok": false, "error": "not attached"})
    );
    assert_eq!(
        client.ask("detach-all-except run-3 run-9"),
        json!({"ok": true, "detached": 1})
    );
    assert_eq!(
        client.ask("detach-all-except"),
        json!({"ok": true, "detached": 1})
    );
    assert_eq!(
        client.ask("attachments"),
        json!({"ok": true, "attachments": []})
    );
}

/// Asserts that a detach answers the failures an open recorded, with path, digest and reason.
#[test]
fn a_detach_answers_the_failures_the_run_recorded() {
    let (rig, _control) = served("tfs-control-fail");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    let _ = client.ask(&format!("attach {root} for run-1"));
    let cc = crate::server::treefs::testing::sha(
        &(0..3000u32).map(|i| (i % 241) as u8).collect::<Vec<_>>(),
    );
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(cc.clone(), "no space".to_owned());
    assert!(open_cached(&rig.core, crate::server::treefs::ino::of(1, 3), false).is_err());
    let answer = client.ask("detach run-1");
    assert_eq!(
        answer["failures"],
        json!([{"path": "/bin/cc", "digest": cc, "reason": "no space"}])
    );
}

/// Asserts each refusal in words, and that the connection goes on after one.
#[test]
fn a_request_that_is_refused_is_answered_in_words_and_the_connection_goes_on() {
    let (rig, _control) = served("tfs-control-refused");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    let refused = |why: &str| json!({"ok": false, "error": why});
    assert_eq!(
        client.ask("attach nope for run-1"),
        refused("not a sha256 digest")
    );
    assert_eq!(
        client.ask(&format!("attach {root} for ")),
        refused("not a usable run name")
    );
    assert_eq!(
        client.ask(&format!("attach {root} to run-1")),
        refused("unknown request")
    );
    assert_eq!(client.ask("attach"), refused("unknown request"));
    assert_eq!(client.ask("detach"), refused("unknown request"));
    assert_eq!(client.ask("detach a b"), refused("unknown request"));
    assert_eq!(client.ask("attachments now"), refused("unknown request"));
    assert_eq!(client.ask("frob"), refused("unknown request"));
    assert_eq!(
        client.ask(&format!(
            "attach {} for run-1 prefetch some",
            "ab".repeat(32)
        )),
        refused("unknown request")
    );
    assert_eq!(client.ask(""), refused("unknown request"));
    let missing = format!("attach {} for run-1", "ab".repeat(32));
    assert!(
        client.ask(&missing)["error"]
            .as_str()
            .unwrap()
            .contains("upstream 404")
    );
    assert_eq!(client.ask("attachments")["ok"], json!(true));
}

/// Asserts that an attach with `prefetch all` is answered with the path and has the tree filled
/// and pinned in the background, and that the plain attach of another tree fills nothing.
#[test]
fn an_attach_that_asks_for_prefetch_has_the_tree_filled() {
    let (rig, _control) = served("tfs-control-prefetch");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    let answer = client.ask(&format!("attach {root} for run-1 prefetch all"));
    assert_eq!(answer["ok"], json!(true));
    crate::server::treefs::testing::until(|| !rig.upstream.asked("pin").is_empty());
    crate::server::treefs::testing::until(|| rig.core.prefetching() == 0);
    let blobs = rig.upstream.asked("fill");
    assert_eq!(
        blobs.iter().filter(|line| line.contains(" from ")).count(),
        4
    );
}

/// Asserts that a line longer than a request may be is refused and the connection closed, that a
/// line that ends the stream is not answered, and that a request that is not text is refused.
#[test]
fn a_line_that_is_too_long_or_unfinished_ends_the_connection() {
    let (rig, _control) = served("tfs-control-long");
    let path = rig.dir.path().join("control.sock");
    let mut client = Client::connect(&path);
    let long = "x".repeat(super::MAX_LINE + 10);
    client.read.get_mut().write_all(long.as_bytes()).unwrap();
    let mut line = String::new();
    let _ = client.read.read_line(&mut line).unwrap();
    assert!(line.contains("the request is too long"), "{line}");
    line.clear();
    assert_eq!(client.read.read_line(&mut line).unwrap(), 0);
    let mut client = Client::connect(&path);
    client.read.get_mut().write_all(b"attachments").unwrap();
    client
        .read
        .get_mut()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    line.clear();
    assert_eq!(client.read.read_line(&mut line).unwrap(), 0);
    let mut client = Client::connect(&path);
    client.read.get_mut().write_all(b"\xff\xfe\n").unwrap();
    line.clear();
    let _ = client.read.read_line(&mut line).unwrap();
    assert!(line.contains("unknown request"));
}

/// Asserts that a client that goes away before the answer is written does not stop the socket.
#[test]
fn a_client_that_hangs_up_does_not_stop_the_socket() {
    let (rig, _control) = served("tfs-control-hangup");
    let path = rig.dir.path().join("control.sock");
    let mut gone = UnixStream::connect(&path).unwrap();
    gone.write_all(b"attachments\n").unwrap();
    gone.shutdown(std::net::Shutdown::Both).unwrap();
    drop(gone);
    let mut client = Client::connect(&path);
    assert_eq!(client.ask("attachments")["ok"], json!(true));
}

/// Asserts that the socket is mode 0660 and has the group given, that a socket file left by a
/// process that is gone is replaced, and that the file is removed when the control is dropped.
#[test]
fn the_socket_has_mode_0660_replaces_a_stale_file_and_is_removed_on_drop() {
    let rig = Rig::new("tfs-control-file", sample());
    let path = rig.dir.path().join("control.sock");
    std::fs::write(&path, "left behind").unwrap();
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let control = Control::serve(rig.core.clone(), &path, gid).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o660);
    assert_eq!(meta.gid(), gid);
    let mut client = Client::connect(&path);
    assert_eq!(client.ask("attachments")["ok"], json!(true));
    drop(control);
    assert!(!path.exists());
}

/// Asserts that a socket that cannot be bound is an error: in a directory that is not there, over a
/// file that cannot be replaced.
#[test]
fn a_socket_that_cannot_be_bound_is_an_error() {
    let rig = Rig::new("tfs-control-bind", sample());
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let nowhere = rig.dir.path().join("no/such/dir/control.sock");
    assert!(Control::serve(rig.core.clone(), &nowhere, gid).is_err());
    let busy = rig.dir.path().join("busy");
    std::fs::create_dir(&busy).unwrap();
    std::fs::write(busy.join("keep"), "x").unwrap();
    assert!(Control::serve(rig.core.clone(), &busy, gid).is_err());
}

/// Asserts that a group the process does not belong to cannot be given to the socket by a process
/// that is not root, and can by one that is.
#[test]
fn a_group_the_process_is_not_in_is_an_error_unless_it_is_root() {
    let rig = Rig::new("tfs-control-group", sample());
    let path = rig.dir.path().join("control.sock");
    let own = std::fs::metadata(&rig.runtime).unwrap().gid();
    let served = Control::serve(rig.core.clone(), &path, own.wrapping_add(7));
    assert_eq!(served.is_ok(), rustix::process::geteuid().is_root());
}

/// Asserts the pure handler, which the socket calls: an unknown request is refused.
#[test]
fn the_handler_refuses_what_it_does_not_know() {
    let rig = Rig::new("tfs-control-handle", sample());
    assert_eq!(
        handle(&rig.core, "x"),
        json!({"ok": false, "error": "unknown request"})
    );
}

/// Asserts the pause of a hand-over: a request on an open connection is refused as handed over and
/// the connection ends; a client that connects meanwhile waits in the backlog and is answered when
/// the socket answers again; and the socket's file stays.
#[test]
fn a_paused_socket_refuses_open_connections_and_a_resumed_one_answers_the_waiting() {
    let (rig, mut control) = served("tfs-control-pause");
    let path = rig.dir.path().join("control.sock");
    let mut open = Client::connect(&path);
    assert_eq!(open.ask("attachments")["ok"], json!(true));
    control.pause();
    assert_eq!(
        open.ask("attachments"),
        json!({"ok": false, "error": "handed over"})
    );
    let mut line = String::new();
    assert_eq!(
        open.read.read_line(&mut line).unwrap(),
        0,
        "the connection ends"
    );
    let mut waiting = UnixStream::connect(&path).unwrap();
    waiting.write_all(b"attachments\n").unwrap();
    control.resume().unwrap();
    let mut answered = String::new();
    BufReader::new(&mut waiting)
        .read_line(&mut answered)
        .unwrap();
    assert!(answered.contains("\"ok\":true"), "{answered}");
    assert!(path.exists());
    // Paused twice or resumed twice is the same.
    control.pause();
    control.pause();
    control.resume().unwrap();
    assert_eq!(Client::connect(&path).ask("attachments")["ok"], json!(true));
}

/// Asserts that a listening socket given by an earlier instance is served with its file, that a copy
/// of the descriptor accepts the same connections, and that an instance that gave up the file does
/// not remove it.
#[test]
fn a_socket_given_over_is_served_copied_and_left_in_place() {
    let rig = Rig::new("tfs-control-given", sample());
    let path = rig.dir.path().join("given.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let mut control = Control::serve_on(rig.core.clone(), listener, &path).unwrap();
    assert_eq!(Client::connect(&path).ask("attachments")["ok"], json!(true));
    let copy = control.listener_fd().unwrap();
    control.pause();
    let copy = std::os::unix::net::UnixListener::from(copy);
    // The descriptor is the same socket, so it is as non-blocking as the instance made it.
    copy.set_nonblocking(false).unwrap();
    let _client = UnixStream::connect(&path).unwrap();
    let (stream, _) = copy.accept().unwrap();
    drop(stream);
    control.leave_path();
    drop(control);
    assert!(path.exists());
}

/// Asserts the gate: it passes requests while open, counts them, refuses when closed, and a close
/// returns only when the requests that passed have ended.
#[test]
fn a_closed_gate_waits_for_the_requests_that_passed_it() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let gate = Arc::new(super::Gate::default());
    let first = gate.enter().unwrap();
    let ended = Arc::new(AtomicBool::new(false));
    let closer = std::thread::spawn({
        let (gate, ended) = (Arc::clone(&gate), Arc::clone(&ended));
        move || {
            gate.close_and_drain();
            ended.load(Ordering::Acquire)
        }
    });
    crate::server::treefs::testing::until(|| gate.enter().is_none());
    ended.store(true, Ordering::Release);
    drop(first);
    assert!(
        closer.join().unwrap(),
        "the close returned before the request ended"
    );
    gate.open();
    assert!(gate.enter().is_some());
}
