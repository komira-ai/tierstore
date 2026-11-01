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

/// Asserts that a clock made `with_pause` waits its own pause instead of the span asked for: a
/// pause of an hour returns after the millisecond it was given, and the count still moves on. It
/// would catch a conformance run's poll that waits out the host's interval anyway.
#[test]
fn a_clock_with_a_pause_of_its_own_waits_that_pause() {
    let clock = MonotonicClock::new().with_pause(Duration::from_millis(1));
    let before = clock.now_ms();
    clock.pause(Duration::from_secs(3600));
    assert!(clock.now_ms() >= before + 1);
}
