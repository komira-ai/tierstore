//! Kernel checks of INTERRUPT on `/dev/fuse` (`docs/design/tsfuse-kernel-findings.md`, K7 and
//! K8): a caller killed while the server holds its direct READ (parked) makes the kernel queue an
//! INTERRUPT, which any clone reads, and ends once the READ is answered; the
//! reply to an INTERRUPT is matched on the processing queue of the clone that holds the
//! interrupted request, not of the clone that read the INTERRUPT. A READ of a buffered read under
//! `FUSE_ASYNC_READ` is a readahead request with no waiter: killing its reader sends no INTERRUPT.
//!
//! The caller is a child process, `cat` of a file of the mount, so that it can be killed; where
//! `cat` cannot be started the check says so on stderr and does not run.
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

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, Command, Stdio};

use kernel::{
    Conn, FOPEN_DIRECT_IO, INT_REQ_BIT, Open, Req, Server, Setup, errno, errno_of, flag,
    is_read_of, op, read_req, reply, serve_until, set_nonblocking, skip,
};
use tsfuse::DevFd;

/// `cat` of the mount's file `f`, its output discarded; `None` (said on stderr) if `cat` cannot
/// be started.
fn cat(conn: &Conn) -> Option<Child> {
    let started = Command::new("cat")
        .arg(conn.path("f"))
        .stdout(Stdio::null())
        .spawn();
    match started {
        Ok(child) => Some(child),
        Err(e) => {
            skip(&format!("cat cannot be started: {e}"));
            None
        }
    }
}

/// Starts `cat`, serves it on `a` until its READ, which is returned parked, and SIGKILLs it.
/// `None` if `cat` cannot be started.
fn killed_with_parked_read(conn: &Conn, a: &DevFd) -> Option<(Child, Req)> {
    let mut child = cat(conn)?;
    let read = serve_until(a, Open::Flags(FOPEN_DIRECT_IO), |r| is_read_of(r, "f"));
    assert_eq!(read.h.pid, child.id(), "the READ is the child's");
    child.kill().unwrap();
    Some((child, read))
}

/// Answers the parked READ with `EINTR` on its own clone and checks that the child then ends,
/// killed by SIGKILL.
fn finish(mut child: Child, read: &Req, a: DevFd) {
    reply(&a, read.h.unique, -errno::EINTR, &[]).unwrap();
    let _server = Server::spawn(a, Open::Flags(FOPEN_DIRECT_IO));
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "{status:?}");
}

/// K7 and K8. A SIGKILLed caller whose READ the server holds: the kernel queues an INTERRUPT
/// naming the READ (its own unique is the READ's with bit 0 set), a clone other than the
/// READ's reads it. Answering the READ `EINTR` on its own clone lets the child end, killed by
/// signal 9. (That it cannot end before, the kernel's uninterruptible wait, is from the source.)
#[test]
fn a_killed_caller_of_a_parked_read_queues_an_interrupt_and_ends_once_the_read_is_answered() {
    let Some(conn) = Conn::mount("k8-kill", &Setup::default()) else {
        return;
    };
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let Some((child, read)) = killed_with_parked_read(&conn, &a) else {
        return;
    };

    let interrupt = read_req(&b).unwrap();
    assert_eq!(interrupt.h.opcode, op::INTERRUPT);
    assert_eq!(interrupt.interrupted(), read.h.unique);
    assert_eq!(interrupt.h.unique, read.h.unique | INT_REQ_BIT);
    finish(child, &read, a);
}

/// K7. The `EAGAIN` reply to an INTERRUPT is matched by the interrupted request's unique on the
/// processing queue of the clone it is written to: written to clone `b`, which read the
/// INTERRUPT but not the READ, it is refused with `ENOENT` and the INTERRUPT is not queued
/// again; written to clone `a`, which holds the READ, it is accepted and the kernel queues the
/// same INTERRUPT again.
#[test]
fn an_interrupt_answered_eagain_is_queued_again_only_when_written_to_the_clone_holding_the_request()
{
    let Some(conn) = Conn::mount("k7-eagain", &Setup::default()) else {
        return;
    };
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let Some((child, read)) = killed_with_parked_read(&conn, &a) else {
        return;
    };
    let interrupt = read_req(&b).unwrap();
    assert_eq!(interrupt.h.opcode, op::INTERRUPT);
    // From here `b` does not block: the kernel queues an INTERRUPT again inside the write of
    // the EAGAIN reply that it accepts, so a read right after that write finds it or nothing.
    set_nonblocking(&b);

    let on_b = reply(&b, interrupt.h.unique, -errno::EAGAIN, &[]).unwrap_err();
    assert_eq!(errno_of(&on_b), Some(errno::ENOENT));
    let nothing = read_req(&b).unwrap_err();
    assert_eq!(errno_of(&nothing), Some(errno::EAGAIN), "not queued again");
    reply(&a, interrupt.h.unique, -errno::EAGAIN, &[]).unwrap();
    let again = read_req(&b).unwrap();
    assert_eq!(again.h.opcode, op::INTERRUPT);
    assert_eq!(again.h.unique, interrupt.h.unique);
    assert_eq!(again.interrupted(), read.h.unique);
    finish(child, &read, a);
}

/// K8, the tree mount's case. With `FUSE_ASYNC_READ` granted (the tree mount asks for it) the
/// READ of a buffered read is a readahead request, which has no waiter: when the reader is
/// SIGKILLed the kernel sends no INTERRUPT for it, the reader exits once its exit-time FLUSH is
/// answered, and the READ stays parked: the server still holds it after the reader is gone and
/// its reply is still accepted.
#[test]
fn a_killed_reader_of_a_parked_readahead_read_exits_and_leaves_the_read_parked_uninterrupted() {
    let setup = Setup {
        want: Setup::default().want | flag::ASYNC_READ,
        ..Setup::default()
    };
    let Some(conn) = Conn::mount("k8-async-read", &setup) else {
        return;
    };
    if conn.granted & flag::ASYNC_READ == 0 {
        skip("the kernel does not offer FUSE_ASYNC_READ");
        return;
    }
    let a = conn.clone_reader();
    let b = conn.clone_reader();
    let Some(mut child) = cat(&conn) else {
        return;
    };
    let read = serve_until(&a, Open::Flags(0), |r| is_read_of(r, "f"));
    child.kill().unwrap();

    // Every request up to the reader's exit-time FLUSH: an INTERRUPT for the READ would have
    // been queued when the reader woke, before it reached its exit, and INTERRUPTs are read
    // before any other request.
    loop {
        let req = read_req(&b).unwrap();
        if req.h.opcode == op::INTERRUPT {
            assert_ne!(
                req.interrupted(),
                read.h.unique,
                "an INTERRUPT for the READ"
            );
            continue;
        }
        kernel::answer(&b, &req, Open::Flags(0)).unwrap();
        if req.h.opcode == op::FLUSH {
            break;
        }
    }
    let _server = Server::spawn(b, Open::Flags(0));
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "{status:?}");

    let r = read.read_in();
    let data = kernel::content(2, r.offset, u64::from(r.size));
    reply(&a, read.h.unique, 0, &[&data]).unwrap();
}
