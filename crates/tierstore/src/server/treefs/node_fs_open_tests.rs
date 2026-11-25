//! The time of each OPEN and the count of the OPENs waiting (`core/opens.rs`), as the FUSE frontend
//! makes them: an OPEN answered on the reader is `cached`, one parked that asked the node for its
//! bytes `filled`, one refused `error`, each in the bucket of the time from its arrival to its
//! answer on the core's clock; the opens parked are counted as waiting until each is answered,
//! whatever its answer and whether or not the kernel took it; no open waits on the report of the
//! counts, nor on a node that does not know it.

use std::time::Duration;

use super::*;
use crate::server::local_cache::attaches::BUCKETS;
use crate::server::local_cache::opens::{Opens, Spans};
use crate::server::treefs::locked;

/// The places of two kinds in a report, of the four: ok and cached, ok and filled, error and
/// cached, error and filled (`local_cache/opens.rs`).
const OK_CACHED: usize = 0;
const OK_FILLED: usize = 1;

/// A request as a reader hands it to the frontend, answered on `device`: how the frontend took it.
fn hand(wire: &Wire, opcode: u32, nodeid: u64, device: &Arc<Recorder>) -> Handled {
    let unique = wire.next.get() + 1;
    wire.next.set(unique);
    let bytes = request(opcode, unique, nodeid, &open_in(0));
    let req = Request::parse(&bytes).unwrap();
    let device: Arc<dyn ReplyDevice> = device.clone();
    wire.fs.handle(req, wire.tokens.register(unique, device))
}

/// The opens of the core of `wire` so far.
fn opens(wire: &Wire) -> Opens {
    wire.rig.core.opens_report()
}

/// How many opens of each kind the core of `wire` counted, in the order of the kinds.
fn counts(wire: &Wire) -> [u64; 4] {
    opens(wire)
        .spans
        .map(|spans| spans.buckets.iter().sum::<u64>())
}

/// Totals of one kind: `count` opens in the bucket `bucket`, of `ms` milliseconds in all.
fn spans(ms: u64, bucket: usize, count: u64) -> Spans {
    let mut buckets = [0; BUCKETS];
    buckets[bucket] = count;
    Spans { ms, buckets }
}

/// The digest of the blob of `include/a.h`.
fn header_blob() -> String {
    sha(b"int a;")
}

/// Asserts each kind through the frontend, each in its exact bucket: an empty file and a cached
/// one opened on the reader are `ok` and `cached` in the first bucket; a directory refused at once
/// is `error` and `cached`; a parked open whose fill the node answers after 30 ms is `ok` and
/// `filled` in the bucket up to 0.05 s; one whose fill fails after 7 s is `error` and `filled` in
/// the bucket up to 10 s. The clock moves only while a fill is held, so every span is exact.
#[test]
fn each_open_is_counted_by_outcome_and_source_in_the_bucket_of_its_span() {
    let wire = Wire::new("nodefs-opens-kinds");
    assert_eq!(wire.error(op::OPEN, node(EMPTY), &open_in(0)), 0);
    let isdir = reads(rustix::io::Errno::ISDIR);
    assert_eq!(wire.error(op::OPEN, node(BIN), &open_in(0)), isdir);
    wire.rig.upstream.gate_verb("fill");
    let device = Arc::new(Recorder::default());
    assert_eq!(hand(&wire, op::OPEN, node(TOOL), &device), Handled::Parked);
    wire.rig.upstream.wait_for_verb("fill", 1);
    wire.rig.clock.advance(Duration::from_millis(30));
    wire.rig.upstream.permit_verb("fill", 1);
    until(|| wire.fs.parked() == 0);
    let _ = locked(&wire.rig.upstream.failing).insert(header_blob(), "upstream 404".to_owned());
    assert_eq!(hand(&wire, op::OPEN, node(A_H), &device), Handled::Parked);
    wire.rig.upstream.wait_for_verb("fill", 2);
    wire.rig.clock.advance(Duration::from_millis(7_000));
    wire.rig.upstream.permit_verb("fill", 1);
    until(|| wire.fs.parked() == 0);
    // `bin/tool` is in the cache now, and pinned for the tree: answered on the reader.
    assert_eq!(hand(&wire, op::OPEN, node(TOOL), &device), Handled::Done);
    let errors: Vec<i32> = locked(&device.0).iter().map(|reply| reply.error).collect();
    assert_eq!(errors, [0, reads(rustix::io::Errno::IO), 0]);
    let want = Opens {
        mount: "mount-1".to_owned(),
        process: u64::from(std::process::id()),
        waiting: 0,
        spans: [
            spans(0, 0, 2),
            spans(30, 5, 1),
            spans(0, 0, 1),
            spans(7_000, 12, 1),
        ],
    };
    assert_eq!(opens(&wire), want);
}

/// Asserts that the opens parked behind fills the node holds are counted as waiting from their
/// arrival, more of them than the fetches have threads, and that the count falls to zero once
/// the node answers and every one of them is answered: those that asked the node are `filled`,
/// and those that waited in the queue until the file was in the cache are `cached`.
#[test]
fn opens_waiting_rise_with_parked_opens_and_fall_to_zero_when_they_are_answered() {
    let wire = Wire::new("nodefs-opens-waiting");
    wire.rig.upstream.gate_verb("fill");
    let parked = FETCHERS.get() + 4;
    let device = Arc::new(Recorder::default());
    for n in 1..=parked {
        assert_eq!(hand(&wire, op::OPEN, node(TOOL), &device), Handled::Parked);
        assert_eq!(opens(&wire).waiting, n as u64);
    }
    wire.rig.upstream.wait_for_verb("fill", FETCHERS.get());
    assert_eq!(opens(&wire).waiting, parked as u64);
    assert_eq!(counts(&wire), [0; 4]);
    wire.rig.upstream.permit_verb("fill", parked);
    until(|| wire.fs.parked() == 0);
    assert_eq!(opens(&wire).waiting, 0);
    let filled = FETCHERS.get() as u64;
    assert_eq!(counts(&wire), [4, filled, 0, 0]);
    assert!(locked(&device.0).iter().all(|reply| reply.error == 0));
}

/// Asserts that the opens waiting fall to zero when the fill they wait on fails (each `error` and
/// `filled`), and when the kernel does not take the answer: an OPEN interrupted while it was
/// parked, and one answered on the reader to a device that is gone, are each still counted, as
/// the opens the core made.
#[test]
fn opens_waiting_fall_to_zero_when_the_fill_fails_and_when_the_open_is_interrupted() {
    let wire = Wire::new("nodefs-opens-failed");
    let _ = locked(&wire.rig.upstream.failing).insert(header_blob(), "upstream 404".to_owned());
    wire.rig.upstream.gate_verb("fill");
    let device = Arc::new(Recorder::default());
    for _ in 0..3 {
        assert_eq!(hand(&wire, op::OPEN, node(A_H), &device), Handled::Parked);
    }
    wire.rig.upstream.wait_for_verb("fill", 3);
    assert_eq!(opens(&wire).waiting, 3);
    wire.rig.upstream.permit_verb("fill", 3);
    until(|| wire.fs.parked() == 0);
    assert_eq!(opens(&wire).waiting, 0);
    assert_eq!(counts(&wire), [0, 0, 0, 3]);
    // `bin/tool` asks a fill, which the gate lets through.
    wire.rig.upstream.permit_verb("fill", 1);
    let replies = wire.deliver(Delivery::Interrupted, op::OPEN, node(TOOL), &open_in(0));
    assert_not_taken(Delivery::Interrupted, &replies);
    let replies = wire.deliver(Delivery::Gone, op::OPEN, node(EMPTY), &open_in(0));
    assert_not_taken(Delivery::Gone, &replies);
    assert_eq!(opens(&wire).waiting, 0);
    assert_eq!(counts(&wire), [1, 1, 0, 3]);
    assert_eq!(wire.rig.core.open_files(), 0);
}

/// Asserts that no open waits on the report of the counts: with the thread that sends the reports
/// held in the middle of one by the node, 1000 opens of a cached file from 16 threads at once are
/// each answered on the reader, each counted once, and the next report carries every one of them.
#[test]
fn a_thousand_opens_from_sixteen_threads_are_counted_while_the_report_waits() {
    const THREADS: u64 = 16;
    const OPENS: u64 = 1000;
    let wire = Wire::new("nodefs-opens-lockfree");
    // `bin/cc` is filled and pinned for the tree: its opens are answered on the reader.
    let first = open_out(&wire.ask(op::OPEN, node(CC), &open_in(0))).fh;
    let _ = wire.ask(op::RELEASE, node(CC), &release_in(first));
    let before = counts(&wire);
    wire.rig.upstream.gate_verb("opens");
    wire.rig.core.report_opens();
    wire.rig.upstream.wait_for_verb("opens", 1);
    let (fs, tokens) = (&wire.fs, &wire.tokens);
    let device = Arc::new(Recorder::default());
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let device = Arc::clone(&device);
            let _ = scope.spawn(move || {
                let mine = (0..OPENS).filter(|n| n % THREADS == thread);
                for n in mine {
                    let unique = 1_000_000 + n;
                    let bytes = request(op::OPEN, unique, node(CC), &open_in(0));
                    let req = Request::parse(&bytes).unwrap();
                    let device: Arc<dyn ReplyDevice> = device.clone();
                    assert_eq!(
                        fs.handle(req, tokens.register(unique, device)),
                        Handled::Done
                    );
                }
            });
        }
    });
    assert_eq!(locked(&device.0).len(), OPENS as usize);
    assert!(locked(&device.0).iter().all(|reply| reply.error == 0));
    let mut want = before;
    want[OK_CACHED] += OPENS;
    assert_eq!(counts(&wire), want);
    assert_eq!(opens(&wire).waiting, 0);
    assert_eq!(wire.rig.upstream.received("opens"), 1);
    wire.rig.core.report_opens();
    wire.rig.upstream.permit_verb("opens", 2);
    until(|| wire.rig.upstream.asked("opens").len() == 2);
    let last = wire.rig.upstream.asked("opens").pop().unwrap();
    let carried = Opens::parse(last.strip_prefix("opens ").unwrap()).unwrap();
    assert_eq!(carried, opens(&wire));
    assert_eq!(carried.spans[OK_CACHED].buckets[0], want[OK_CACHED]);
}

/// Asserts that a node of an earlier release, which answers the report `refused unknown request`,
/// slows no open and stops no report: the opens are answered on the reader before and after it,
/// and the next report is sent and carries them.
#[test]
fn a_node_that_refuses_the_report_slows_no_open() {
    let wire = Wire::new("nodefs-opens-older");
    let refused = "refused unknown request".to_owned();
    let _ = locked(&wire.rig.upstream.answers).insert("opens".to_owned(), refused);
    let device = Arc::new(Recorder::default());
    assert_eq!(hand(&wire, op::OPEN, node(EMPTY), &device), Handled::Done);
    wire.rig.core.report_opens();
    until(|| wire.rig.upstream.asked("opens").len() == 1);
    assert_eq!(hand(&wire, op::OPEN, node(EMPTY), &device), Handled::Done);
    wire.rig.core.report_opens();
    until(|| wire.rig.upstream.asked("opens").len() == 2);
    let last = wire.rig.upstream.asked("opens").pop().unwrap();
    let carried = Opens::parse(last.strip_prefix("opens ").unwrap()).unwrap();
    assert_eq!(carried.spans[OK_CACHED].buckets[0], 2);
    assert_eq!(locked(&device.0).len(), 2);
}

/// Asserts that the stop of the readers (a hand-over, a restart) tells the node of the opens once
/// every parked one is answered: its report has none waiting and counts the open that was parked.
#[test]
fn the_stop_of_the_readers_reports_the_opens_once_they_are_answered() {
    let wire = Wire::new("nodefs-opens-drain");
    wire.rig.upstream.gate_verb("fill");
    let device = Arc::new(Recorder::default());
    assert_eq!(hand(&wire, op::OPEN, node(TOOL), &device), Handled::Parked);
    wire.rig.upstream.wait_for_verb("fill", 1);
    wire.rig.upstream.permit_verb("fill", 1);
    wire.fs.drain();
    until(|| !wire.rig.upstream.asked("opens").is_empty());
    let line = wire.rig.upstream.asked("opens").remove(0);
    let told = Opens::parse(line.strip_prefix("opens ").unwrap()).unwrap();
    assert_eq!(told.waiting, 0);
    assert_eq!(told.spans[OK_FILLED].buckets.iter().sum::<u64>(), 1);
}
