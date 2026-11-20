use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Clock, MachineClock, ManualClock, epoch_ms, micros};

#[test]
fn a_manual_clock_starts_at_zero_and_moves_only_when_told() {
    let clock = ManualClock::new(5_000);
    assert_eq!(clock.now_us(), 0);
    assert_eq!(clock.wall_ms(), 5_000);
    clock.advance_us(2_500);
    assert_eq!(clock.now_us(), 2_500);
    assert_eq!(clock.wall_ms(), 5_002);
    clock.advance_us(500);
    assert_eq!(clock.now_us(), 3_000);
    // The wall clock moves by whole milliseconds of each advance: 500 us add none.
    assert_eq!(clock.wall_ms(), 5_002);
}

#[test]
fn a_default_manual_clock_reads_zero() {
    let clock = ManualClock::default();
    assert_eq!(clock.now_us(), 0);
    assert_eq!(clock.wall_ms(), 0);
}

/// The machine clock's monotonic reading is the elapsed time in whole microseconds.
#[test]
fn an_elapsed_time_is_whole_microseconds_saturating_at_the_largest() {
    assert_eq!(micros(Duration::ZERO), 0);
    assert_eq!(micros(Duration::from_nanos(1_999)), 1);
    assert_eq!(micros(Duration::from_millis(2_500)), 2_500_000);
    assert_eq!(micros(Duration::MAX), u64::MAX);
}

/// The machine clock's wall reading is whole milliseconds since the epoch; a clock set before
/// the epoch reads 0.
#[test]
fn a_wall_reading_is_whole_milliseconds_since_the_epoch_and_zero_before_it() {
    let at = UNIX_EPOCH + Duration::from_micros(1_795_204_351_123_456);
    assert_eq!(epoch_ms(at.duration_since(UNIX_EPOCH)), 1_795_204_351_123);
    assert_eq!(epoch_ms(Ok(Duration::MAX)), u64::MAX);
    let before: SystemTime = UNIX_EPOCH - Duration::from_secs(1);
    assert_eq!(epoch_ms(before.duration_since(UNIX_EPOCH)), 0);
}

/// The machine's clock can be made and shared; the two readings it passes through the
/// functions above are not asserted (they are the machine's time).
#[test]
fn the_machine_clock_can_be_made_and_shared() {
    let clock = MachineClock::start();
    let shared = MachineClock::shared();
    let _ = (
        clock.now_us(),
        clock.wall_ms(),
        shared.now_us(),
        shared.wall_ms(),
    );
    assert!(format!("{clock:?}").starts_with("MachineClock"));
}
