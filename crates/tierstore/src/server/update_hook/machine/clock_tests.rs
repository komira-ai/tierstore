//! The clock.

use std::time::Duration;

use super::super::ports::Clock;
use super::MonotonicClock;

/// Asserts that `pause` waits at least its span on the clock's own count and that the count
/// never goes back. It would catch a pause that returns at once, or a clock that is not
/// monotonic.
#[test]
fn pause_moves_the_monotonic_count_on_by_at_least_its_span() {
    let clock = MonotonicClock::default();
    let before = clock.now_ms();
    clock.pause(Duration::from_millis(30));
    assert!(clock.now_ms() >= before + 30);
}

/// Asserts what a pause waits, without waiting: the span asked for on a plain clock, and the
/// clock's own pause on one made `with_pause`, whatever the span (an hour asked, a millisecond
/// waited). It would catch a conformance run's poll that waits out the host's interval anyway.
#[test]
fn a_clock_with_a_pause_of_its_own_waits_that_pause() {
    let hour = Duration::from_secs(3600);
    assert_eq!(MonotonicClock::new().waited(hour), hour);
    let fast = MonotonicClock::new().with_pause(Duration::from_millis(1));
    assert_eq!(fast.waited(hour), Duration::from_millis(1));
    assert_eq!(fast.waited(Duration::ZERO), Duration::from_millis(1));
}
