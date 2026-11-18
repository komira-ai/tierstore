//! A trace finished with no `tracing` subscriber (a test binary of its own, so none is
//! installed): the event is skipped and the trace is still returned whole.
//!
//! Catches: a finish that needs a subscriber to return its trace.

use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::Class;
use tierstore_metrics::trace::{DemandTrace, Stage, TraceId};

/// Asserts that with no subscriber a finished trace still has its stages and total.
#[test]
fn a_trace_finishes_with_no_subscriber() {
    let clock = ManualClock::new();
    let mut t = DemandTrace::begin(TraceId(5), Class::Background, &clock);
    clock.advance(3);
    assert_eq!(t.mark(Stage::FirstByte, &clock), Ok(()));
    clock.advance(4);
    let trace = t.finish(&clock).unwrap();
    assert_eq!(
        trace.stages(),
        [
            (Stage::Handler, 0),
            (Stage::FirstByte, 3),
            (Stage::Reply, 7)
        ]
    );
    assert_eq!(trace.class(), Class::Background);
}
