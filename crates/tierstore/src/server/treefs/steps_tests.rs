//! The timing of a start's steps, on a clock the test moves, and the lines it says.

use std::sync::Arc;
use std::time::Duration;

use super::Steps;
use crate::Mono;
use crate::server::treefs::testing::{Logged, logged};

/// Asserts that each step is timed from the end of the one before (the first from the beginning),
/// that a step with no time passed takes zero, and that the total runs from the beginning; and
/// that each step is said on stderr, at info, as `tierstore fs: <step> took <n> ms`, with exactly
/// those numbers.
#[test]
fn each_step_takes_the_time_since_the_step_before_and_the_total_since_the_beginning() {
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::INFO);
    let clock = Arc::new(Mono::new());
    clock.advance(Duration::from_millis(7));
    let mut steps = Steps::new(clock.clone());
    clock.advance(Duration::from_millis(120_000));
    assert_eq!(steps.done("waiting"), 120_000);
    assert_eq!(steps.done("nothing"), 0);
    clock.advance(Duration::from_millis(15));
    assert_eq!(steps.done("mounting"), 15);
    assert_eq!(steps.total(), 120_015);
    assert_eq!(
        log.text(),
        "tierstore fs: waiting took 120000 ms\n\
         tierstore fs: nothing took 0 ms\n\
         tierstore fs: mounting took 15 ms\n"
    );
}

/// Asserts that the steps are said at info: a log of warnings only has none of them.
#[test]
fn the_steps_are_said_at_info() {
    let log = Logged::default();
    let _logging = logged(&log, tracing::Level::WARN);
    let mut steps = Steps::new(Arc::new(Mono::new()));
    let _ = steps.done("quiet");
    assert_eq!(log.text(), "");
}
