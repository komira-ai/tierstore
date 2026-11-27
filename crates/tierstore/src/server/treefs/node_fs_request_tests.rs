//! The time of each request the FUSE frontend answers, by the kind of its opcode
//! (`core/requests.rs`): every opcode the frontend serves under its kind, and every other one (a
//! refused change, an opcode it does not serve, a body too short for its opcode) under `other`;
//! a parked request timed to its answer on a thread of the fetches; the put directory's requests
//! not counted; and the stop of the readers telling the node.

use std::time::Duration;

use super::*;
use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::opens::Spans;
use crate::server::local_cache::requests::{KINDS, RequestKind, Requests};

/// How many requests of each kind the core of `wire` counted, in the order of [`KINDS`].
fn counts(wire: &Wire) -> [u64; KINDS.len()] {
    wire.rig
        .core
        .requests_report()
        .spans
        .map(|spans| spans.buckets.iter().sum::<u64>())
}

/// The counts of `kinds`, each the number of times it is named, in the order of [`KINDS`].
fn of(kinds: &[RequestKind]) -> [u64; KINDS.len()] {
    let mut counts = [0; KINDS.len()];
    for kind in kinds {
        counts[kind.index()] += 1;
    }
    counts
}

/// Asserts that each request the frontend serves is counted once under the kind of its opcode,
/// whatever its answer: LOOKUP, GETATTR, OPEN, READ, READDIR and READDIRPLUS (both `readdir`),
/// READLINK (of a link and of a file, which is refused); and that every other one is `other`: a
/// RELEASE, an OPENDIR, a RELEASEDIR, a STATFS, a FLUSH and an FSYNC (`ENOSYS`), a MKDIR
/// (`EROFS`), a GETXATTR (an opcode the library does not implement, `ENOSYS`), and a LOOKUP
/// whose name has no NUL (`EIO`).
#[test]
fn each_request_is_counted_under_the_kind_of_its_opcode() {
    let wire = Wire::new("nodefs-requests-kinds");
    assert_eq!(counts(&wire), [0; KINDS.len()]);
    assert_eq!(wire.ask(op::LOOKUP, node(BIN), &cstr(b"tool")).error, 0);
    assert_eq!(wire.ask(op::GETATTR, node(TOOL), &[0; 16]).error, 0);
    let opened = open_out(&wire.ask(op::OPEN, node(EMPTY), &open_in(0)));
    let read = wire.ask(op::READ, node(EMPTY), &read_in(opened.fh, 0, 16));
    assert_eq!((read.error, read.body.len()), (0, 0));
    assert_eq!(
        wire.ask(op::READDIR, node(BIN), &read_in(0, 0, 4096)).error,
        0
    );
    assert_eq!(
        wire.ask(op::READDIRPLUS, node(BIN), &read_in(0, 0, 4096))
            .error,
        0
    );
    assert_eq!(wire.ask(op::READLINK, node(LIB), &[]).error, 0);
    assert_ne!(wire.error(op::READLINK, node(CC), &[]), 0);
    assert_eq!(
        wire.error(op::RELEASE, node(EMPTY), &release_in(opened.fh)),
        0
    );
    assert_eq!(wire.error(op::OPENDIR, node(BIN), &open_in(0)), 0);
    assert_eq!(wire.error(op::RELEASEDIR, node(BIN), &release_in(0)), 0);
    assert_eq!(wire.error(op::STATFS, node(BIN), &[]), 0);
    let enosys = reads(rustix::io::Errno::NOSYS);
    let (flush, fsync) = (FlushIn::default(), FsyncIn::default());
    assert_eq!(wire.error(op::FLUSH, node(TOOL), flush.as_bytes()), enosys);
    assert_eq!(wire.error(op::FSYNC, node(TOOL), fsync.as_bytes()), enosys);
    assert_eq!(wire.error(op::GETXATTR, node(TOOL), &[0; 8]), enosys);
    let mkdir = [MkdirIn::default().as_bytes(), &cstr(b"d")].concat();
    let erofs = reads(rustix::io::Errno::ROFS);
    assert_eq!(wire.error(op::MKDIR, node(BIN), &mkdir), erofs);
    let eio = reads(rustix::io::Errno::IO);
    assert_eq!(wire.error(op::LOOKUP, node(BIN), b"cc"), eio);
    use RequestKind::{Getattr, Lookup, Open, Other, Read, Readdir, Readlink};
    let want = of(&[
        Lookup, Getattr, Open, Read, Readdir, Readdir, Readlink, Readlink, Other, Other, Other,
        Other, Other, Other, Other, Other, Other,
    ]);
    assert_eq!(counts(&wire), want);
}

/// Asserts that a request parked on the fetches is timed from its arrival on the reader to its
/// answer on a thread of the fetches: an OPEN whose fill the node answers 30 ms later is one
/// `open` of 30 ms, in the bucket up to 0.05 s, and no other kind counts anything.
#[test]
fn a_parked_request_is_timed_to_its_answer_on_the_fetches() {
    let wire = Wire::new("nodefs-requests-parked");
    wire.rig.upstream.gate_verb("fill");
    let device = Arc::new(Recorder::default());
    let unique = wire.next.get() + 1;
    wire.next.set(unique);
    let bytes = request(op::OPEN, unique, node(TOOL), &open_in(0));
    let req = Request::parse(&bytes).unwrap();
    let replier = wire
        .tokens
        .register(unique, device.clone() as Arc<dyn ReplyDevice>);
    assert_eq!(wire.fs.handle(req, replier), Handled::Parked);
    wire.rig.upstream.wait_for_verb("fill", 1);
    // Not answered yet: not counted yet.
    assert_eq!(counts(&wire), [0; KINDS.len()]);
    wire.rig.clock.advance(Duration::from_millis(30));
    wire.rig.upstream.permit_verb("fill", 1);
    until(|| wire.fs.parked() == 0);
    let mut buckets = [0; BUCKETS];
    buckets[5] = 1;
    let mut want = [Spans::default(); KINDS.len()];
    want[RequestKind::Open.index()] = Spans { ms: 30, buckets };
    assert_eq!(wire.rig.core.requests_report().spans, want);
}

/// Asserts that the requests of the put directory are its own and not counted: the LOOKUP of
/// `put` in the root, answered by the put directory, counts nothing, and a LOOKUP of the tree's
/// counts one.
#[test]
fn the_requests_of_the_put_directory_are_not_counted() {
    let wire = Wire::with_put("nodefs-requests-put");
    assert_eq!(wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"put")).error, 0);
    assert_eq!(counts(&wire), [0; KINDS.len()]);
    assert_eq!(wire.ask(op::LOOKUP, node(BIN), &cstr(b"tool")).error, 0);
    assert_eq!(counts(&wire), of(&[RequestKind::Lookup]));
}

/// Asserts that the stop of the readers tells the node of the requests answered, as the core
/// counts them, and that a second stop with no request between tells it nothing more.
#[test]
fn the_stop_of_the_readers_tells_the_node_of_the_requests() {
    let wire = Wire::new("nodefs-requests-drain");
    assert_eq!(wire.ask(op::GETATTR, node(TOOL), &[0; 16]).error, 0);
    let told = || wire.rig.upstream.asked("requests");
    let opens = || wire.rig.upstream.asked("opens");
    wire.fs.drain();
    until(|| told().len() == 1 && opens().len() == 1);
    let sent = Requests::parse(told()[0].strip_prefix("requests ").unwrap()).unwrap();
    assert_eq!(sent, wire.rig.core.requests_report());
    assert_eq!(counts(&wire), of(&[RequestKind::Getattr]));
    wire.fs.drain();
    // The reports go in order: once the second `opens` is there, a second `requests` would be.
    until(|| opens().len() == 2);
    assert_eq!(told().len(), 1);
}
