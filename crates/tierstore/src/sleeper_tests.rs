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

/// A task that was ready runs while another waits on the sleeper, so a loop that has only the
/// sleeper to wait on does not keep the rest of the runtime from running.
#[tokio::test]
async fn recording_sleeper_gives_other_tasks_a_turn() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let other = tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });

    RecordingSleeper::new().sleep(Duration::from_secs(1)).await;
    assert!(ran.load(Ordering::SeqCst));
    other.await.unwrap();
}

/// The sleeper of a process waits for the runtime's clock and no longer: a sleep of ten seconds
/// is not over a second before its end, and is over at its end. The runtime's clock is paused and
/// moved by the test, so no real time is waited for.
#[tokio::test(start_paused = true)]
async fn tokio_sleeper_waits_for_the_runtime_clock() {
    let sleeper: Arc<dyn Sleeper> = Arc::new(TokioSleeper);
    let task = tokio::spawn(async move { sleeper.sleep(Duration::from_secs(10)).await });
    tokio::task::yield_now().await;
    assert!(!task.is_finished());

    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished());

    tokio::time::advance(Duration::from_secs(1)).await;
    task.await.unwrap();
}
