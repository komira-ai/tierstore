//! The deadline of a holder: past when made, moved on by renewals, run down by the monotonic clock.

use std::sync::Arc;
use std::time::Duration;

use crate::deadline::millis;
use crate::{Deadline, Mono};

const MARGIN: Duration = Duration::from_secs(10);

fn deadline() -> (Arc<Mono>, Deadline) {
    let mono = Arc::new(Mono::new());
    (mono.clone(), Deadline::new(mono, MARGIN))
}

/// Asserts that a deadline nobody has renewed is past: not live, nothing left, and no call that
/// destroys may start, so a process that has not been told it may act does not act.
#[test]
fn a_deadline_that_was_never_renewed_is_past_and_allows_no_start() {
    let (_, deadline) = deadline();
    assert!(!deadline.is_live());
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert!(!deadline.allows_start());
    assert_eq!(deadline.margin(), MARGIN);
}

/// Asserts the three thresholds as the clock runs: a start is allowed while at least the margin
/// is left (the boundary included), the deadline is live while anything is left, and nothing is
/// left at the deadline itself.
#[test]
fn the_clock_runs_the_deadline_down_through_the_margin_to_nothing() {
    let (mono, deadline) = deadline();
    deadline.extend_to(30_000);
    assert_eq!(deadline.remaining(), Duration::from_secs(30));
    assert!(deadline.is_live() && deadline.allows_start());

    mono.advance(Duration::from_secs(20));
    assert_eq!(deadline.remaining(), MARGIN);
    assert!(deadline.allows_start(), "exactly the margin is enough");

    mono.advance(Duration::from_millis(1));
    assert!(deadline.is_live());
    assert!(
        !deadline.allows_start(),
        "a millisecond under the margin is not"
    );

    mono.advance(Duration::from_millis(9_999));
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert!(!deadline.is_live());

    mono.advance(Duration::from_secs(5));
    assert_eq!(
        deadline.remaining(),
        Duration::ZERO,
        "past stays at nothing"
    );
}

/// Asserts that a renewal to an earlier time than the deadline has does not shorten it, as a beat
/// that was acknowledged late would, and that a later one moves it on.
#[test]
fn a_renewal_moves_the_deadline_on_and_never_back() {
    let (_, deadline) = deadline();
    deadline.extend_to(50_000);
    deadline.extend_to(20_000);
    assert_eq!(deadline.remaining(), Duration::from_secs(50));
    deadline.extend_to(70_000);
    assert_eq!(deadline.remaining(), Duration::from_secs(70));
}

/// Asserts the count of milliseconds: whole ones, and the largest count for a duration that does
/// not fit instead of a wrapped one.
#[test]
fn millis_counts_whole_milliseconds_and_stops_at_the_largest() {
    assert_eq!(millis(Duration::from_micros(2_500)), 2);
    assert_eq!(millis(Duration::from_secs(60)), 60_000);
    assert_eq!(millis(Duration::MAX), u64::MAX);
}

/// Asserts that a margin of zero does not let a call start on a deadline with nothing left: an
/// unrenewed deadline and one the clock has run past both refuse, and a live one allows.
#[test]
fn a_zero_margin_still_refuses_a_deadline_that_is_unrenewed_or_past() {
    let mono = Arc::new(Mono::new());
    let deadline = Deadline::new(mono.clone(), Duration::ZERO);
    assert!(!deadline.allows_start(), "never renewed");

    deadline.extend_to(5_000);
    assert!(deadline.allows_start(), "live");

    mono.advance(Duration::from_secs(5));
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert!(!deadline.allows_start(), "run out");
}
