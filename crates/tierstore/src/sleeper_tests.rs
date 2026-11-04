//! The sleepers: the one of a process waits on the runtime's timer, and the one of tests writes
//! the wait down and does not wait.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::{RecordingSleeper, Sleeper, TokioSleeper};

/// The waits a loop with back-off asks for are the vector the test expects, in order, as values.
#[tokio::test]
async fn recording_sleeper_records_the_requested_waits_in_order() {
    let sleeper = RecordingSleeper::new();
    assert!(sleeper.asked().is_empty());
    for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
        sleeper.sleep(Duration::from_secs(seconds)).await;
    }
    let expected: Vec<Duration> = [1, 2, 4, 8, 16, 32, 60, 60]
        .into_iter()
        .map(Duration::from_secs)
        .collect();
    assert_eq!(sleeper.asked(), expected);
}

/// The call returns at once: a wait of the longest length is over without the test doing
/// anything, where a real wait would never end.
#[tokio::test]
async fn recording_sleeper_returns_without_waiting() {
    let sleeper = RecordingSleeper::new();
    sleeper.sleep(Duration::MAX).await;
    assert_eq!(sleeper.asked(), [Duration::MAX]);
}

/// The call records its wait and then yields once before it returns: its first poll is pending,
/// so a task that was waiting to run can be run by the test at that point, and the next poll
/// finishes it.
#[tokio::test]
async fn recording_sleeper_yields_once_after_writing_the_wait_down() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let other = tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });

    let sleeper = RecordingSleeper::new();
    let mut sleeping = Box::pin(sleeper.sleep(Duration::from_secs(1)));
    assert!(futures::poll!(&mut sleeping).is_pending());
    assert_eq!(sleeper.asked(), [Duration::from_secs(1)]);

    other.await.unwrap();
    assert!(ran.load(Ordering::SeqCst));
    sleeping.await;
}

/// The sleeper of a process waits for the runtime's clock and no longer: a sleep of ten seconds
/// is not over at nine, and is over at ten. The runtime's clock is paused and moved by the test,
/// and the sleep is polled by the test itself at each point, so no real time passes and no timer
/// is left to advance on its own.
#[tokio::test(start_paused = true)]
async fn tokio_sleeper_waits_for_the_runtime_clock() {
    let sleeper: Arc<dyn Sleeper> = Arc::new(TokioSleeper);
    let mut sleeping = Box::pin(sleeper.sleep(Duration::from_secs(10)));
    assert!(futures::poll!(&mut sleeping).is_pending());

    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(futures::poll!(&mut sleeping).is_pending());

    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(futures::poll!(&mut sleeping).is_ready());
}
