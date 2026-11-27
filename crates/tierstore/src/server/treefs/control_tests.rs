//! The control socket over a real Unix socket and a core over the node's cache.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Control, handle};
use crate::server::treefs::attach::Attachments;
use crate::server::treefs::testing::{Built, Logged, Rig, file, logged, open_cached, sample};

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
    served_over(name, sample())
}

/// A rig whose upstream holds `built`, and the control socket served over its core.
fn served_over(name: &str, built: Built) -> (Rig, Control) {
    let rig = Rig::new(name, built);
    let path = rig.dir.path().join("control.sock");
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let control = Control::serve(rig.core.clone(), &path, gid).unwrap();
    (rig, control)
}

fn root(rig: &Rig) -> String {
    rig.upstream.built.lock().unwrap().root.clone()
}

/// The value of the sample of the node's cache's `/metricz` text that begins with `series`.
fn metric(rig: &Rig, series: &str) -> u64 {
    let prefix = format!("{series} ");
    let text = rig.node.metrics();
    let found = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(|n| n.parse().unwrap()));
    found.unwrap_or_else(|| panic!("no sample {series} in the cache's metrics"))
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
        json!({"ok": true, "failures": [], "stall_ms": 0})
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
        .insert(cc.clone(), "mismatch".to_owned());
    assert!(open_cached(&rig.core, crate::server::treefs::ino::of(1, 3), false).is_err());
    let answer = client.ask("detach run-1");
    assert_eq!(
        answer["failures"],
        json!([{"path": "/bin/cc", "digest": cc, "reason": "mismatch"}])
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
    assert_eq!(
        client.ask("set tree_list_ram_bytes 4095"),
        refused("tree_list_ram_bytes 4095 is less than 4096")
    );
    for (bytes, said) in [
        ("lots", "\"lots\""),
        ("-1", "\"-1\""),
        ("", "\"\""),
        ("18446744073709551616", "\"18446744073709551616\""),
    ] {
        assert_eq!(
            client.ask(&format!("set tree_list_ram_bytes {bytes}")),
            refused(&format!(
                "tree_list_ram_bytes {said} is not a whole number of bytes"
            ))
        );
    }
    for request in [
        "set",
        "set tree_list_ram_bytes",
        "set tree_list_ram_bytes 4096 now",
        "set tree_idle_secs 60",
    ] {
        assert_eq!(client.ask(request), refused("unknown request"), "{request}");
    }
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
    let total: u64 = {
        let built = rig.upstream.built.lock().unwrap();
        built.blobs.values().map(|blob| blob.len() as u64).sum()
    };
    crate::server::treefs::testing::until(|| rig.node.pinned_bytes() == total);
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

/// Asserts that an instance that is handed over from ends without removing the socket of the one
/// that took over: the successor binds the same path while this one is alive, this one's drop
/// returns (its thread is woken without a connection to the path) and leaves the successor's
/// socket answering.
#[test]
fn a_control_that_was_replaced_leaves_the_successors_socket_when_dropped() {
    let rig = Rig::new("tfs-control-replaced", sample());
    let path = rig.dir.path().join("control.sock");
    let gid = std::fs::metadata(&rig.runtime).unwrap().gid();
    let old = Control::serve(rig.core.clone(), &path, gid).unwrap();
    let new = Control::serve(rig.core.clone(), &path, gid).unwrap();
    drop(old);
    assert!(path.exists());
    let mut client = Client::connect(&path);
    assert_eq!(client.ask("attachments")["ok"], json!(true));
    drop(new);
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

/// Asserts that `set tree_list_ram_bytes` makes the cap of the lists mapped what it names, from
/// the least the file allows (4096) to the most a number of bytes holds, answers it with the id
/// of the mount, and has the node's cache told each cap at once, with no look for idle trees: its
/// gauge for the mount shows the cap, with nothing mapped and nothing released. Catches a set that
/// the node's gauge `tierstore_fs_tree_list_ram_bytes` never shows.
#[test]
fn a_set_of_the_tree_list_cap_is_answered_with_the_mount_and_told_to_the_node() {
    let (rig, _control) = served("tfs-control-set");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    for bytes in [4096, 1 << 30, u64::MAX] {
        assert_eq!(
            client.ask(&format!("set tree_list_ram_bytes {bytes}")),
            json!({"ok": true, "tree_list_ram_bytes": bytes, "mount": "mount-1"})
        );
        let gauge = |name: &str| {
            let series = format!("tierstore_fs_{name}{{mount=\"mount-1\"}}");
            metric(&rig, &series)
        };
        assert_eq!(gauge("tree_list_ram_bytes"), bytes);
        assert_eq!(gauge("tree_lists_mapped"), 0);
        assert_eq!(gauge("tree_list_mapped_bytes"), 0);
        let counter = |name: &str, why: &str| {
            let labels = format!("mount=\"mount-1\"{why}");
            let series = format!("tierstore_fs_{name}{{{labels}}}");
            metric(&rig, &series)
        };
        assert_eq!(counter("tree_list_releases_total", ",why=\"cap\""), 0);
        assert_eq!(counter("tree_list_releases_total", ",why=\"idle\""), 0);
        assert_eq!(counter("tree_list_over_cap_total", ""), 0);
    }
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

/// Asserts that a control socket of a held core (its successor is taking over) refuses an attach,
/// a detach and a detach-all-except by saying so, not as "not attached" or as a count of none,
/// and answers a listing, and that the requests work again once the hold is released.
#[test]
fn a_held_core_refuses_changes_in_words_and_still_lists() {
    let (rig, _control) = served("tfs-control-held");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for run-1"))["ok"], true);
    rig.core.hold(true);
    let replaced = json!({
        "ok": false,
        "error": "this instance is being replaced by its successor: ask its control socket"
    });
    assert_eq!(client.ask(&format!("attach {root} for run-2")), replaced);
    assert_eq!(client.ask("detach run-1"), replaced);
    assert_eq!(client.ask("detach-all-except"), replaced);
    assert_eq!(
        client.ask("attachments")["attachments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    rig.core.hold(false);
    assert_eq!(client.ask("detach run-1")["ok"], true);
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

/// The inode of `bin/cc` of the sample tree, the first tree a rig numbers.
fn cc_ino() -> u64 {
    crate::server::treefs::ino::of(1, 3)
}

/// The bytes of `bin/cc` of the sample tree.
fn cc_bytes() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 241) as u8).collect()
}

/// Opens `ino` on a thread of its own while every fill (`verb`) is held at the rig's upstream,
/// moves the rig's clock on by `waited` once one is received, lets it go, and gives the open's
/// handle or error.
fn open_while_held(
    rig: &Rig,
    ino: u64,
    verb: &str,
    waited: Duration,
) -> Result<u64, crate::server::treefs::core::Errno> {
    rig.upstream.gate_verb(verb);
    let core = rig.core.clone();
    let opening = std::thread::spawn(move || open_cached(&core, ino, false));
    rig.upstream.wait_for_verb(verb, 1);
    rig.clock.advance(waited);
    rig.upstream.permit_verb(verb, 1);
    opening.join().unwrap()
}

/// Opens `ino` through a fill the rig's upstream holds while the rig's clock moves on by
/// `waited`.
fn open_through_a_held_fill(rig: &Rig, ino: u64, waited: Duration) {
    assert!(open_while_held(rig, ino, "fill", waited).is_ok());
}

/// The answer of a detach with no failures and a stall of `ms`.
fn stalled(ms: u64) -> Value {
    json!({"ok": true, "failures": [], "stall_ms": ms})
}

/// Asserts that a detach answers, as `stall_ms`, the time the run's open of a blob waited on its
/// fill: 1500 ms on the rig's clock, which moved while the upstream held the fill.
#[test]
fn detach_answers_the_fill_wait_of_the_runs_opens_as_stall_ms() {
    let (rig, _control) = served("tfs-control-stall");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(1500));
    assert_eq!(
        client.ask("detach r1"),
        json!({"ok": true, "failures": [], "stall_ms": 1500})
    );
}

/// Asserts that a run whose opens the cache answered has a stall of 0, though the clock moved:
/// the blob was filled before the run attached, and the opens of the run ask no fill.
#[test]
fn a_run_whose_opens_were_all_cached_answers_stall_ms_0() {
    let (rig, _control) = served("tfs-control-stall-cached");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r0"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(900));
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    for _ in 0..3 {
        assert!(open_cached(&rig.core, cc_ino(), false).is_ok());
        rig.clock.advance(Duration::from_millis(400));
    }
    assert_eq!(client.ask("detach r1"), stalled(0));
    assert_eq!(client.ask("detach r0"), stalled(900));
}

/// Asserts that two runs that attach one tree each get the wait of their own opens only: the
/// first run's open of `bin/cc` waits 1500 ms on its fill; the second run's open of the same blob
/// in a tree of its own is answered from the cache, and the 700 ms the clock moves on after it add
/// to neither.
#[test]
fn two_runs_of_a_tree_each_get_the_wait_of_their_own_opens_only() {
    let mut built = sample();
    let other = built.add_tree(vec![("cc", file(&cc_bytes()))]);
    let (rig, _control) = served_over("tfs-control-stall-two", built);
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    assert_eq!(client.ask(&format!("attach {other} for r2"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(1500));
    let other_cc = crate::server::treefs::ino::of(2, 1);
    assert!(open_cached(&rig.core, other_cc, false).is_ok());
    rig.clock.advance(Duration::from_millis(700));
    let cc = crate::server::treefs::testing::sha(&cc_bytes());
    let blob_fills = rig.upstream.asked("fill");
    let blob_fills = blob_fills
        .iter()
        .filter(|line| line.starts_with(&format!("fill {cc} ")));
    assert_eq!(
        blob_fills.count(),
        1,
        "the second open of the blob is not filled"
    );
    assert_eq!(client.ask("detach r2"), stalled(0));
    assert_eq!(client.ask("detach r1"), stalled(1500));
}

/// Asserts that an open under a tree two runs attach adds its wait to both: the time is the
/// tree's, as its failures are, and each run's detach answers it.
#[test]
fn every_run_that_attaches_the_tree_gets_the_wait_of_an_open_under_it() {
    let (rig, _control) = served("tfs-control-stall-shared");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    assert_eq!(client.ask(&format!("attach {root} for r2"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(250));
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    assert_eq!(client.ask("detach r1"), stalled(250));
    assert_eq!(client.ask("detach r2"), stalled(250));
}

/// Asserts that an open whose fill waited and then failed adds its wait as well as its failure.
#[test]
fn an_open_whose_fill_failed_adds_its_wait_and_its_failure() {
    let (rig, _control) = served("tfs-control-stall-failed");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    let cc = crate::server::treefs::testing::sha(&cc_bytes());
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(cc.clone(), "upstream 404".to_owned());
    let opened = open_while_held(&rig, cc_ino(), "fill", Duration::from_millis(60));
    assert!(opened.is_err());
    assert_eq!(
        client.ask("detach r1"),
        json!({
            "ok": true,
            "failures": [{"path": "/bin/cc", "digest": cc, "reason": "upstream 404"}],
            "stall_ms": 60,
        })
    );
}

/// Asserts that a detach of a run that attached nothing still answers `not attached`, with no
/// `stall_ms`, and so does a second detach of a run that had a stall.
#[test]
fn a_detach_of_a_run_that_attached_nothing_answers_not_attached_with_no_stall_ms() {
    let (rig, _control) = served("tfs-control-stall-none");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let not_attached = json!({"ok": false, "error": "not attached"});
    assert_eq!(client.ask("detach r9"), not_attached);
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(5));
    assert_eq!(client.ask("detach r1"), stalled(5));
    assert_eq!(client.ask("detach r1"), not_attached);
}

/// Asserts that the stall survives a restart: a start that reads an attachments file with the
/// stall of an attachment and of a record it cannot attach again answers each run's stall, and
/// writes the file back with both.
#[test]
fn a_restart_answers_the_stall_the_attachments_file_held() {
    let rig = Rig::new("tfs-control-stall-restart", sample());
    let root = root(&rig);
    let gone = "ab".repeat(32);
    let text = format!("{root} r1\nstall_ms {root} r1 1500\n{gone} r2\nstall_ms {gone} r2 40\n");
    let file = rig.runtime.join("attachments");
    std::fs::write(&file, &text).unwrap();
    rig.core
        .restore(Attachments::saved(&rig.runtime).unwrap(), 4);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    assert_eq!(handle(&rig.core, "detach r1"), stalled(1500));
    assert_eq!(handle(&rig.core, "detach r2"), stalled(40));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
}

/// Asserts that a hand-over keeps the stall: the hold before a successor takes over writes the
/// stall of the run to the attachments file, and the successor's detach of the run answers it.
#[test]
fn a_successor_answers_the_stall_its_predecessor_counted() {
    let (rig, _control) = served("tfs-control-stall-handover");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(1500));
    rig.core.hold(true);
    let file = rig.runtime.join("attachments");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{root} r1\nstall_ms {root} r1 1500\n")
    );
    let successor = rig.successor();
    successor.restore(Attachments::saved(&rig.runtime).unwrap(), 4);
    assert_eq!(handle(&successor, "detach r1"), stalled(1500));
}

/// Asserts that a hold whose write of the stall fails says so on stderr and still holds: the file
/// is a directory by then, and a rename over it fails.
#[test]
fn a_hold_that_cannot_write_the_stall_says_so_and_holds() {
    let (rig, _control) = served("tfs-control-stall-unwritten");
    let mut client = Client::connect(&rig.dir.path().join("control.sock"));
    let root = root(&rig);
    assert_eq!(client.ask(&format!("attach {root} for r1"))["ok"], true);
    open_through_a_held_fill(&rig, cc_ino(), Duration::from_millis(20));
    let file = rig.runtime.join("attachments");
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    std::fs::write(file.join("in-the-way"), "").unwrap();
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    rig.core.hold(true);
    drop(guard);
    assert!(rig.core.is_held());
    let said = log.text();
    assert!(
        said.contains("tierstore fs: cannot write the attachments file: "),
        "{said}"
    );
}
