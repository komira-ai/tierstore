//! The trace of a demand read (section 12): stages marked on the injected clock, in order, and
//! one `tracing` event when it is answered.
//!
//! Catches: offsets measured from the wrong origin, a stage marked out of order or before the
//! last mark accepted, a refused mark that changes the trace, an event with missing or wrong
//! fields, a trace that emits before its reply or carries anything but its id and class.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::{self, ThreadId};

use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_fetch_api::ids::Class;
use tierstore_metrics::trace::{DemandTrace, Stage, TARGET, TraceError, TraceId};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// The fields of each event the subscriber saw, with its target and level, by the thread that
/// emitted it.
#[derive(Default)]
struct Capture(Mutex<Vec<(ThreadId, BTreeMap<String, String>)>>);

/// The one subscriber of this test binary, the global default, installed before any test emits.
/// It is global, not scoped to a test: `tracing` caches whether each callsite is enabled, so a
/// scoped subscriber can miss an event whose callsite another test's thread registered first.
/// Each test reads only its own thread's events.
static CAPTURE: OnceLock<&'static Capture> = OnceLock::new();

fn capture() -> &'static Capture {
    CAPTURE.get_or_init(|| {
        let capture: &'static Capture = Box::leak(Box::default());
        let installed = tracing::subscriber::set_global_default(SharedCapture(capture));
        assert!(installed.is_ok(), "the test binary installs one subscriber");
        capture
    })
}

/// The events this thread emitted.
fn events() -> Vec<BTreeMap<String, String>> {
    let me = thread::current().id();
    capture()
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(thread, _)| *thread == me)
        .map(|(_, fields)| fields.clone())
        .collect()
}

/// The subscriber, by reference to the capture it fills.
struct SharedCapture(&'static Capture);

struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

impl Subscriber for SharedCapture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(BTreeMap::new());
        let meta = event.metadata();
        fields
            .0
            .insert("@target".to_owned(), meta.target().to_owned());
        fields
            .0
            .insert("@level".to_owned(), meta.level().to_string());
        event.record(&mut fields);
        self.0
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((thread::current().id(), fields.0));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Runs every stage of a read on `clock`, the device queue wait reported by the drive.
fn full_read(clock: &ManualClock) -> DemandTrace {
    let mut t = DemandTrace::begin(TraceId(7), Class::Demand, clock);
    let steps = [
        (Stage::WaitSent, 1),
        (Stage::BudgetAcquired, 2),
        (Stage::Locate, 3),
        (Stage::DeviceChosen, 0),
        (Stage::GetStart, 4),
    ];
    for (stage, ms) in steps {
        clock.advance(ms);
        assert_eq!(t.mark(stage, clock), Ok(()));
    }
    let get_start = clock.now_ms();
    assert_eq!(t.mark_at(Stage::DeviceQueueWait, get_start + 5), Ok(()));
    for (stage, ms) in [
        (Stage::FirstByte, 11),
        (Stage::CellReceived, 7),
        (Stage::HashDone, 8),
        (Stage::Write, 9),
    ] {
        clock.advance(ms);
        assert_eq!(t.mark(stage, clock), Ok(()));
    }
    clock.advance(10);
    t
}

const STAGES: &str = "handler=0 wait_sent=1 budget_acquired=3 locate=6 device_chosen=6 \
    get_start=10 device_queue_wait=15 first_byte=21 cell_received=28 hash_done=36 write=45 \
    reply=55";

/// Asserts each stage's offset from the handler, with the clock not at zero when the read began,
/// and the total.
#[test]
fn every_stage_is_measured_from_the_handler() {
    capture();
    let clock = ManualClock::new();
    clock.advance(100);
    let trace = full_read(&clock).finish(&clock).unwrap();
    let offsets: Vec<u64> = trace.stages().iter().map(|(_, ms)| *ms).collect();
    assert_eq!(offsets, [0, 1, 3, 6, 6, 10, 15, 21, 28, 36, 45, 55]);
    let stages: Vec<Stage> = trace.stages().iter().map(|(s, _)| *s).collect();
    assert_eq!(stages, Stage::ALL);
    assert_eq!(trace.total_ms(), 55);
    assert_eq!(trace.offset_ms(Stage::DeviceQueueWait), Some(15));
    assert_eq!((trace.id(), trace.class()), (TraceId(7), Class::Demand));
}

/// Asserts that a read of present cells, handler then reply, has only those two stages.
#[test]
fn a_read_that_skips_stages_has_only_the_ones_it_passed() {
    capture();
    let clock = ManualClock::new();
    let t = DemandTrace::begin(TraceId(1), Class::Prepare, &clock);
    clock.advance(2);
    let trace = t.finish(&clock).unwrap();
    assert_eq!(trace.stages(), [(Stage::Handler, 0), (Stage::Reply, 2)]);
    assert_eq!(trace.offset_ms(Stage::Locate), None);
    assert_eq!(trace.total_ms(), 2);
}

/// Asserts that a stage marked again, or before the last one marked, or at a time before the
/// last mark, is refused with its error and leaves the trace as it was.
#[test]
fn a_mark_out_of_order_or_back_in_time_is_refused_and_changes_nothing() {
    capture();
    let clock = ManualClock::new();
    let mut t = DemandTrace::begin(TraceId(2), Class::Demand, &clock);
    clock.advance(9);
    t.mark(Stage::Locate, &clock).unwrap();
    let before = t.clone();
    let err = t.mark(Stage::BudgetAcquired, &clock).unwrap_err();
    assert_eq!(
        err,
        TraceError::OutOfOrder {
            stage: Stage::BudgetAcquired,
            last: Stage::Locate
        }
    );
    assert_eq!(
        err.to_string(),
        "demand read trace: BudgetAcquired marked after Locate"
    );
    assert_eq!(
        t.mark(Stage::Locate, &clock),
        Err(TraceError::OutOfOrder {
            stage: Stage::Locate,
            last: Stage::Locate
        })
    );
    let err = t.mark_at(Stage::Write, 5).unwrap_err();
    assert_eq!(
        err,
        TraceError::BeforeLast {
            stage: Stage::Write,
            at_ms: 5,
            last_ms: 9
        }
    );
    assert_eq!(
        err.to_string(),
        "demand read trace: Write at 5 ms, before the last mark at 9 ms"
    );
    assert_eq!(t, before);
    let trace = t.finish(&clock).unwrap();
    assert_eq!(
        trace.stages(),
        [(Stage::Handler, 0), (Stage::Locate, 9), (Stage::Reply, 9)]
    );
}

/// Asserts that a trace whose reply was already marked, or whose last mark is later than the
/// clock at its finish, is not finished and emits nothing.
#[test]
fn a_finish_that_breaks_the_order_fails_and_emits_nothing() {
    capture();
    {
        let clock = ManualClock::new();
        let mut t = DemandTrace::begin(TraceId(3), Class::Demand, &clock);
        t.mark_at(Stage::Reply, 4).unwrap();
        assert_eq!(
            t.finish(&clock),
            Err(TraceError::OutOfOrder {
                stage: Stage::Reply,
                last: Stage::Reply
            })
        );
        let mut t = DemandTrace::begin(TraceId(4), Class::Demand, &clock);
        t.mark_at(Stage::Write, 30).unwrap();
        assert_eq!(
            t.finish(&clock),
            Err(TraceError::BeforeLast {
                stage: Stage::Reply,
                at_ms: 0,
                last_ms: 30
            })
        );
    }
    assert!(events().is_empty());
}

/// Asserts that a finished trace emits exactly one debug event, with the trace target, its id,
/// class, total and the offset of every stage, and no other field.
#[test]
fn a_finished_trace_emits_one_event_with_its_id_class_and_offsets() {
    capture();
    let clock = ManualClock::new();
    clock.advance(100);
    let read = full_read(&clock);
    assert!(events().is_empty());
    read.finish(&clock).unwrap();
    let events = events();
    let expected: BTreeMap<String, String> = [
        ("@target", TARGET),
        ("@level", "DEBUG"),
        ("message", "tierstore: demand read"),
        ("trace", "7"),
        ("class", "demand"),
        ("total_ms", "55"),
        ("stages", STAGES),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(events, [expected]);
}

/// Asserts that the stages are in ascending order with distinct names.
#[test]
fn the_stages_are_ordered_and_named_once() {
    capture();
    assert!(Stage::ALL.windows(2).all(|w| w[0] < w[1]));
    let names: Vec<&str> = Stage::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        names,
        STAGES
            .split(' ')
            .map(|p| p.split('=').next().unwrap())
            .collect::<Vec<_>>()
    );
}
