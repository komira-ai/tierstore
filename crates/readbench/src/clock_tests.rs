use super::{Clock, MachineClock, ManualClock};

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

/// The machine's clock can be made, shared and read; what it reads is not asserted.
#[test]
fn the_machine_clock_can_be_read() {
    let clock = MachineClock::start();
    let _ = clock.now_us();
    let _ = clock.wall_ms();
    let shared = MachineClock::shared();
    let _ = shared.now_us();
    let _ = shared.wall_ms();
    assert!(!format!("{shared:?}").is_empty());
}
