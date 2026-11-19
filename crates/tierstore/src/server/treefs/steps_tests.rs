//! The timing of a start's steps, on a clock the test moves.

use std::sync::Arc;
use std::time::Duration;

use super::Steps;
use crate::Mono;

/// Asserts that each step is timed from the end of the one before (the first from the beginning),
/// that a step with no time passed takes zero, and that the total runs from the beginning.
#[test]
fn each_step_takes_the_time_since_the_step_before_and_the_total_since_the_beginning() {
    let clock = Arc::new(Mono::new());
    clock.advance(Duration::from_millis(7));
    let mut steps = Steps::new(clock.clone());
    clock.advance(Duration::from_millis(120_000));
    assert_eq!(steps.done("waiting"), 120_000);
    assert_eq!(steps.done("nothing"), 0);
    clock.advance(Duration::from_millis(15));
    assert_eq!(steps.done("mounting"), 15);
    assert_eq!(steps.total(), 120_015);
}
