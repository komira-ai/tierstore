//! Kernel checks of takeover on `/dev/fuse` (`docs/design/tsfuse-kernel-findings.md`, K4 to
//! K6): what a supervisor that keeps the descriptors of a dead server keeps, what closing a
//! clone's last descriptor does to the requests it held, and `FUSE_NOTIFY_RESEND` with requests
//! parked on several clones.
//!
//! A process's death is its descriptors closing: the kernel releases a device (and ends the
//! requests on its processing queue) on the last close of the open file, whichever process held
//! it. So "the server dies and the supervisor keeps a `dup`" is checked here as "the reader's
//! descriptor is closed while a `dup` of it stays open", in one process.
//!
//! An integration target: it mounts through `mount(2)` as root (the farm) or `fusermount3`
//! (CI), and says on stderr why it does not run where neither can.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod kernel;

use kernel::{
    Conn, FOPEN_DIRECT_IO, NOTIFY_RESEND, Open, Server, Setup, UNIQUE_RESEND, caller, content, dup,
    errno, errno_of, flag, is_read_of, pread, read_req, reply, serve_until, skip,
};

/// K5. A READ parked on clone `a`, whose only descriptor is then closed while a `dup` of it
/// stays open (the supervisor's copy): the request is still on that open file's processing
/// queue, the reply written through the `dup` is accepted, and the caller gets its bytes.
#[test]
fn a_dup_kept_by_a_supervisor_keeps_a_parked_read_answerable_after_the_reader_closes() {
    let Some(conn) = Conn::mount("k5-dup", &Setup::default()) else {
        return;
    };
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let a = conn.clone_reader();
    let path = conn.path("f");
    let f = caller(move || pread(&path, 0, 4096));
    let rf = serve_until(&a, open, |r| is_read_of(r, "f"));

    let kept = dup(&a);
    drop(a);
    assert!(
        !f.is_finished(),
        "the open file is not released while a dup is open"
    );
    reply(&kept, rf.h.unique, 0, &[&content(2, 0, 4096)]).unwrap();
    let _server = Server::spawn(kept, open);
    assert_eq!(f.join().unwrap().unwrap(), content(2, 0, 4096));
}

/// K5. A READ parked on clone `b`, whose only descriptor is then closed: the kernel ends the
/// request and the caller gets `ECONNABORTED`. The connection lives on: another clone serves the
/// next caller.
#[test]
fn closing_the_only_descriptor_of_a_clone_ends_its_parked_read_with_econnaborted() {
    let Some(conn) = Conn::mount("k5-release", &Setup::default()) else {
        return;
    };
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let path = conn.path("g");
    let g = caller(move || pread(&path, 0, 4096));
    let _rg = serve_until(&b, open, |r| is_read_of(r, "g"));

    drop(b);
    let _server = Server::spawn(a, open);
    let ended = g.join().unwrap().unwrap_err();
    assert_eq!(errno_of(&ended), Some(errno::ECONNABORTED));
    assert_eq!(
        pread(&conn.path("f"), 0, 4096).unwrap(),
        content(2, 0, 4096)
    );
}

/// K4 and K6. READs parked on two clones; their readers close while the supervisor keeps a
/// `dup` of every descriptor; the new server writes `FUSE_NOTIFY_RESEND` on one of them. Both
/// requests come back on the input queue with `FUSE_UNIQUE_RESEND` (bit 63) set in their unique
/// and the same arguments; a reply under the original unique is refused with `ENOENT`, the reply
/// under the unique as read again is accepted on the descriptor that read it, each caller gets
/// its own bytes (no `EIO`), and a second reply is refused with `ENOENT` (exactly once).
#[test]
fn resend_brings_back_reads_parked_on_every_kept_clone_with_bit_63_set_in_their_unique() {
    let Some(conn) = Conn::mount("k4-resend", &Setup::default()) else {
        return;
    };
    if conn.offered & flag::HAS_RESEND == 0 {
        skip("the kernel does not offer FUSE_HAS_RESEND (Linux 6.9 or later does)");
        return;
    }
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let (pf, pg) = (conn.path("f"), conn.path("g"));
    let f = caller(move || pread(&pf, 4096, 4096));
    let rf = serve_until(&a, open, |r| is_read_of(r, "f"));
    let g = caller(move || pread(&pg, 8192, 4096));
    let rg = serve_until(&b, open, |r| is_read_of(r, "g"));

    // The server dies; the supervisor keeps a dup of each clone (and the mount's descriptor).
    let kept = [dup(&a), dup(&b)];
    drop((a, b));

    // The new server: RESEND, then read the requests again on the mount's descriptor.
    reply(&conn.dev, 0, NOTIFY_RESEND, &[]).unwrap();
    let mut again = [read_req(&conn.dev).unwrap(), read_req(&conn.dev).unwrap()];
    again.sort_by_key(|r| r.h.nodeid);
    for (old, new) in [(&rf, &again[0]), (&rg, &again[1])] {
        assert_eq!(new.h.unique, old.h.unique | UNIQUE_RESEND);
        assert_eq!(new.h.opcode, old.h.opcode);
        assert_eq!(new.body, old.body, "the same arguments");
    }

    let (df, dg) = (content(2, 4096, 4096), content(3, 8192, 4096));
    let original = reply(&conn.dev, rf.h.unique, 0, &[&df]).unwrap_err();
    assert_eq!(errno_of(&original), Some(errno::ENOENT));
    for old_reader in &kept {
        let moved = reply(old_reader, again[0].h.unique, 0, &[&df]).unwrap_err();
        assert_eq!(errno_of(&moved), Some(errno::ENOENT));
    }
    reply(&conn.dev, again[0].h.unique, 0, &[&df]).unwrap();
    reply(&conn.dev, again[1].h.unique, 0, &[&dg]).unwrap();
    let twice = reply(&conn.dev, again[0].h.unique, 0, &[&df]).unwrap_err();
    assert_eq!(errno_of(&twice), Some(errno::ENOENT));

    let _server = Server::spawn(dup(&conn.dev), open);
    assert_eq!(f.join().unwrap().unwrap(), df);
    assert_eq!(g.join().unwrap().unwrap(), dg);
}

/// K6. `FUSE_NOTIFY_RESEND` with nothing in flight is accepted and queues nothing: the next
/// request read is the next caller's, under an ordinary unique.
#[test]
fn resend_with_nothing_in_flight_is_accepted_and_resends_nothing() {
    let Some(conn) = Conn::mount("k6-resend-empty", &Setup::default()) else {
        return;
    };
    if conn.offered & flag::HAS_RESEND == 0 {
        skip("the kernel does not offer FUSE_HAS_RESEND (Linux 6.9 or later does)");
        return;
    }
    reply(&conn.dev, 0, NOTIFY_RESEND, &[]).unwrap();
    let path = conn.path("f");
    let stat = caller(move || std::fs::metadata(path).map(|m| m.len()));
    let next = read_req(&conn.dev).unwrap();
    assert_eq!(next.h.unique & UNIQUE_RESEND, 0);
    kernel::answer(&conn.dev, &next, Open::Flags(0)).unwrap();
    let _server = Server::spawn(dup(&conn.dev), Open::Flags(0));
    assert_eq!(stat.join().unwrap().unwrap(), kernel::FILE_SIZE);
}
