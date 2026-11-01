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
