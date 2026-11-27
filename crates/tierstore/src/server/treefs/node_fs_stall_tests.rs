//! A node's cache that takes fills and does not answer them: the opens that wait on those
//! fills are parked on the fetches' threads (`fetches.rs`), never on the readers of the mount, so
//! the mount still answers the reads of open files, the opens of files the cache holds and the
//! other requests of a tree in memory, however many opens wait.

use super::shared_reads::{PACKED, content, placed_pack};
use super::*;
use crate::server::treefs::fills::Report;

/// A request as a reader hands it to the frontend: how the frontend took it, with the reply not
/// waited for.
fn hand(wire: &Wire, opcode: u32, nodeid: u64, body: &[u8], device: &Arc<Recorder>) -> Handled {
    let unique = wire.next.get() + 1;
    wire.next.set(unique);
    let bytes = request(opcode, unique, nodeid, body);
    let req = Request::parse(&bytes).unwrap();
    let device: Arc<dyn ReplyDevice> = device.clone();
    wire.fs.handle(req, wire.tokens.register(unique, device))
}

/// Asserts that with every fill held unanswered by the node and more opens waiting on fills than
/// the fetches have threads (and than the mount has readers), each of those opens is parked at once
/// and none is answered; that meanwhile a read of a file open before, an open of a file the cache
/// holds, and an attribute of the tree are each answered at once, on the reader that asked them;
/// and that the waiting opens are answered once the node answers.
#[test]
fn opens_that_wait_on_fills_never_hold_a_reader_and_cached_files_are_still_served() {
    let wire = Wire::new("nodefs-stall");
    // `bin/cc` is filled, pinned for the tree, and open.
    let cc = ino::of(1, CC);
    let fh = open_out(&wire.ask(op::OPEN, cc, &open_in(0))).fh;
    wire.rig.upstream.gate_verb("fill");
    let waiting = FETCHERS.get() + 4;
    let stalled = Arc::new(Recorder::default());
    for _ in 0..waiting {
        // `bin/tool` is not in the cache: its open asks a fill, which the node holds.
        let handled = hand(&wire, op::OPEN, ino::of(1, TOOL), &open_in(0), &stalled);
        assert_eq!(handled, Handled::Parked);
    }
    // Every thread of the fetches waits on the cache's one fill of the blob, which the others
    // join; the opens after them wait in the queue.
    wire.rig.upstream.wait_for_verb("fill", 1);
    until(|| wire.joins() == FETCHERS.get() as u64 - 1);
    assert_eq!(wire.fs.parked(), waiting);
    // Meanwhile, on the reader that asks: a read of the open file, an open of the cached file,
    // and the attributes of the tree's root.
    let mut cc_bytes: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    cc_bytes.truncate(100);
    let answered = Arc::new(Recorder::default());
    let handled = hand(&wire, op::READ, cc, &read_in(fh, 0, 100), &answered);
    assert_eq!(handled, Handled::Done);
    let handled = hand(&wire, op::OPEN, cc, &open_in(0), &answered);
    assert_eq!(handled, Handled::Done);
    let handled = hand(&wire, op::GETATTR, ino::of(1, 0), &[0; 16], &answered);
    assert_eq!(handled, Handled::Done);
    let replies = std::mem::take(&mut *answered.0.lock().unwrap());
    let errors: Vec<i32> = replies.iter().map(|reply| reply.error).collect();
    assert_eq!(errors, [0, 0, 0]);
    assert_eq!(replies[0].body, cc_bytes);
    // None of the waiting opens was answered, nor any fill asked beyond the threads'.
    assert!(stalled.0.lock().unwrap().is_empty());
    assert_eq!(wire.rig.upstream.received("fill"), 1);
    assert_eq!(wire.joins(), FETCHERS.get() as u64 - 1);
    // The cache answers: every waiting open is answered, a handle each.
    wire.rig.upstream.permit_verb("fill", waiting);
    until(|| wire.fs.parked() == 0);
    let replies = std::mem::take(&mut *stalled.0.lock().unwrap());
    assert_eq!(replies.len(), waiting);
    assert!(replies.iter().all(|reply| reply.error == 0), "{replies:?}");
}

/// Asserts that an open whose blob the cache holds, pinned for its tree, is answered at once on the
/// reader with no fill of the cache, and one the cache does not hold is parked. A report to the
/// cache between the two counts, as the `fetched` report of the attach of [`Wire::new`] is: it is
/// not a request of the open, and the cache fills nothing for it.
#[test]
fn an_open_of_a_cached_and_pinned_blob_asks_no_fill_of_the_cache() {
    let wire = Wire::new("nodefs-now");
    let cc = ino::of(1, CC);
    let first = open_out(&wire.ask(op::OPEN, cc, &open_in(0))).fh;
    let before = wire.rig.upstream.asked("fill");
    let device = Arc::new(Recorder::default());
    let handled = hand(&wire, op::OPEN, cc, &open_in(0), &device);
    assert_eq!(handled, Handled::Done);
    let reply = device.0.lock().unwrap().remove(0);
    assert_eq!(reply.error, 0);
    assert_ne!(open_out(&reply).fh, first);
    wire.rig.asks().report(Report::Started("late".to_owned()));
    assert_eq!(wire.rig.upstream.asked("fill"), before);
    let handled = hand(&wire, op::OPEN, ino::of(1, TOOL), &open_in(0), &device);
    assert_eq!(handled, Handled::Parked);
    until(|| wire.fs.parked() == 0);
    assert_eq!(device.0.lock().unwrap().remove(0).error, 0);
}

/// The GETs the node's cache has sent the upstream so far.
fn upstream_gets(wire: &Wire) -> u64 {
    wire.metric("tierstore_cache_upstream_reads_total{op=\"get\"}")
}

/// Asserts that a file of a placed tree whose span the tree was given is opened again with no
/// fill of the cache while the pack's file is open (the first open fills the span, parked; the
/// second is answered on the reader and the cache reads nothing from the upstream), and that once
/// every handle of the pack is released the next open is still answered on the reader, from the
/// span the cache holds, with the upstream read no more.
#[test]
fn a_placed_file_is_opened_again_without_the_node_while_its_pack_is_open() {
    let wire = placed_pack("nodefs-now-placed");
    let root = entry_out(&wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"jobs"))).nodeid;
    let (name, size, _) = PACKED[1];
    let node = entry_out(&wire.ask(op::LOOKUP, root, &cstr(name.as_bytes()))).nodeid;
    let before = upstream_gets(&wire);
    let first = open_out(&wire.ask(op::OPEN, node, &open_in(0))).fh;
    let filled = upstream_gets(&wire);
    assert!(filled > before, "the first open fills the span");
    let device = Arc::new(Recorder::default());
    assert_eq!(
        hand(&wire, op::OPEN, node, &open_in(0), &device),
        Handled::Done
    );
    let second = open_out(&device.0.lock().unwrap().remove(0)).fh;
    assert_eq!(upstream_gets(&wire), filled);
    let read = wire.ask(op::READ, node, &read_in(second, 0, size as u32));
    assert_eq!(read.body, content(11, size));
    for fh in [first, second] {
        let _ = wire.ask(op::RELEASE, node, &release_in(fh));
    }
    assert_eq!(
        hand(&wire, op::OPEN, node, &open_in(0), &device),
        Handled::Done
    );
    assert_eq!(device.0.lock().unwrap().remove(0).error, 0);
    assert_eq!(upstream_gets(&wire), filled);
}
