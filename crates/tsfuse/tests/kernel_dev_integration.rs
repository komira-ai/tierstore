//! Kernel checks of `/dev/fuse` itself (`docs/design/tsfuse-kernel-findings.md`, K1 to K3):
//! the reply goes to the clone that read the request, every clone polls one input queue, and
//! the kernel's verdict on a malformed reply. Each check reads requests on a chosen clone and
//! writes replies to a chosen clone through the raw harness of `tests/kernel`.
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
    Conn, FILE_SIZE, FOPEN_DIRECT_IO, Open, Server, Setup, VALID, attr, caller, content, dup,
    errno, errno_of, is_read_of, op, pread, read_req, reply, serve_until, set_nonblocking,
};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::time::Timespec;
use tsfuse::abi::{EntryOut, OutHeader};

/// K1. A LOOKUP read on clone `a` is answered on clone `b` and on the original descriptor:
/// both writes are refused with `ENOENT` and change nothing: the reply on `a` is then
/// accepted and the caller gets the entry; a second reply on `a` is refused with `ENOENT`
/// (the kernel completes a request once).
#[test]
fn a_reply_on_another_clone_is_refused_enoent_and_the_request_waits_for_its_own_clone() {
    let Some(conn) = Conn::mount("k1-affinity", &Setup::default()) else {
        return;
    };
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let path = conn.path("f");
    let stat = caller(move || std::fs::metadata(path).map(|m| m.len()));

    let lookup = serve_until(&a, Open::Flags(0), |r| r.h.opcode == op::LOOKUP);
    assert_eq!(lookup.name(), "f");
    let entry = EntryOut::new(2, 1, VALID, VALID, attr(2));
    for other in [&b, &conn.dev] {
        let refused = reply(other, lookup.h.unique, 0, &[entry.as_bytes()]).unwrap_err();
        assert_eq!(errno_of(&refused), Some(errno::ENOENT));
    }
    reply(&a, lookup.h.unique, 0, &[entry.as_bytes()]).unwrap();
    let again = reply(&a, lookup.h.unique, 0, &[entry.as_bytes()]).unwrap_err();
    assert_eq!(errno_of(&again), Some(errno::ENOENT));
    let _server = Server::spawn(a, Open::Flags(0));
    assert_eq!(stat.join().unwrap().unwrap(), FILE_SIZE);
}

/// K2. One request makes every clone poll readable (the clones share one input queue, so one
/// request wakes every poller of every clone, as each `AsyncFd` is); one clone reads it and every other clone's read is
/// `EAGAIN`.
#[test]
fn one_request_makes_every_clone_poll_readable_and_only_one_clone_reads_it() {
    let Some(conn) = Conn::mount("k2-herd", &Setup::default()) else {
        return;
    };
    let clones: Vec<_> = (0..4).map(|_| conn.clone_reader()).collect();
    for c in &clones {
        set_nonblocking(c);
    }
    let path = conn.path("f");
    let stat = caller(move || std::fs::metadata(path).map(|m| m.len()));

    // Wait (no timeout) until the caller's first request is queued.
    let mut first = [PollFd::new(&clones[0], PollFlags::IN)];
    assert_eq!(poll(&mut first, None).unwrap(), 1);
    // Every clone is readable for that one request.
    let mut all: Vec<PollFd<'_>> = clones
        .iter()
        .map(|c| PollFd::new(c, PollFlags::IN))
        .collect();
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(poll(&mut all, Some(&zero)).unwrap(), clones.len());
    assert!(all.iter().all(|p| p.revents().contains(PollFlags::IN)));

    // One clone takes it; the caller sends nothing more until it is answered, so every other
    // clone finds the queue empty.
    let req = read_req(&clones[0]).unwrap();
    for c in &clones[1..] {
        let empty = read_req(c).unwrap_err();
        assert_eq!(errno_of(&empty), Some(errno::EAGAIN));
    }

    kernel::answer(&clones[0], &req, Open::Flags(0)).unwrap();
    let _server = Server::spawn(dup(&conn.dev), Open::Flags(0));
    assert_eq!(stat.join().unwrap().unwrap(), FILE_SIZE);
}

/// K1 with two parked reads: a READ of `f` parked on clone `a` and one of `g` on clone `b`
/// are completed in the opposite order, each on its own clone, and each caller gets its own
/// bytes; `g`'s reply on `a` is refused with `ENOENT`.
#[test]
fn parked_reads_on_two_clones_complete_out_of_order_each_on_its_own_clone() {
    let Some(conn) = Conn::mount("k1-parked", &Setup::default()) else {
        return;
    };
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let (pf, pg) = (conn.path("f"), conn.path("g"));
    let f = caller(move || pread(&pf, 8192, 4096));
    let rf = serve_until(&a, open, |r| is_read_of(r, "f"));
    let g = caller(move || pread(&pg, 4096, 4096));
    let rg = serve_until(&b, open, |r| is_read_of(r, "g"));
    assert_eq!((rf.read_in().offset, rg.read_in().offset), (8192, 4096));

    let g_bytes = content(3, 4096, 4096);
    let wrong = reply(&a, rg.h.unique, 0, &[&g_bytes]).unwrap_err();
    assert_eq!(errno_of(&wrong), Some(errno::ENOENT));
    reply(&b, rg.h.unique, 0, &[&g_bytes]).unwrap();
    reply(&a, rf.h.unique, 0, &[&content(2, 8192, 4096)]).unwrap();

    let _server = Server::spawn(a, open);
    assert_eq!(g.join().unwrap().unwrap(), g_bytes);
    assert_eq!(f.join().unwrap().unwrap(), content(2, 8192, 4096));
}

/// The header of a reply to `unique` whose length field says `len`.
fn header(len: usize, error: i32, unique: u64) -> Vec<u8> {
    OutHeader {
        len: u32::try_from(len).unwrap(),
        error,
        unique,
    }
    .as_bytes()
    .to_vec()
}

/// K3. A reply whose length field is not the number of bytes written, and an error outside
/// -511..=0, are refused with `EINVAL` before the request is looked up: the request stays
/// answerable and its caller gets the correct reply that follows.
#[test]
fn a_reply_with_a_wrong_length_or_an_errno_out_of_range_is_refused_einval_and_the_request_stays_answerable()
 {
    let Some(conn) = Conn::mount("k3-einval", &Setup::default()) else {
        return;
    };
    let open = Open::Flags(FOPEN_DIRECT_IO);
    let path = conn.path("f");
    let f = caller(move || pread(&path, 0, 4096));
    let rf = serve_until(&conn.dev, open, |r| is_read_of(r, "f"));
    let data = content(2, 0, 4096);
    let u = rf.h.unique;

    let malformed = [
        // One byte more than written, and one byte fewer.
        [header(16 + 4096 + 1, 0, u), data.clone()].concat(),
        [header(16 + 4096 - 1, 0, u), data.clone()].concat(),
        // A positive error, and one below -511.
        header(16, 5, u),
        header(16, -512, u),
    ];
    for bytes in malformed {
        let err = std::io::Error::from(rustix::io::write(&conn.dev, &bytes).unwrap_err());
        assert_eq!(errno_of(&err), Some(errno::EINVAL));
    }
    reply(&conn.dev, u, 0, &[&data]).unwrap();
    let _server = Server::spawn(dup(&conn.dev), open);
    assert_eq!(f.join().unwrap().unwrap(), data);
}

/// K3. An error reply that carries a payload is refused with `EINVAL`, and unlike the cases
/// above it ends the request: the caller gets `EIO`, not the error, and a later reply is
/// refused with `ENOENT`.
#[test]
fn an_error_reply_with_a_payload_is_refused_einval_and_fails_the_caller_with_eio() {
    let Some(conn) = Conn::mount("k3-eio", &Setup::default()) else {
        return;
    };
    let path = conn.path("f");
    let stat = caller(move || std::fs::metadata(path).map(|m| m.len()));
    let lookup = serve_until(&conn.dev, Open::Flags(0), |r| r.h.opcode == op::LOOKUP);

    let refused = reply(&conn.dev, lookup.h.unique, -errno::ENOENT, &[&[0u8; 8]]).unwrap_err();
    assert_eq!(errno_of(&refused), Some(errno::EINVAL));
    let _server = Server::spawn(dup(&conn.dev), Open::Flags(0));
    let caller_saw = stat.join().unwrap().unwrap_err();
    assert_eq!(errno_of(&caller_saw), Some(errno::EIO));
    let late = reply(&conn.dev, lookup.h.unique, -errno::ENOENT, &[]).unwrap_err();
    assert_eq!(errno_of(&late), Some(errno::ENOENT));
}
