//! The end of the serving of the fronts: what runs after both are done.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use std::task::Poll;

use tokio::sync::oneshot;

use super::serve_fronts;

/// Asserts that `after` (the stop of the heartbeat) does not run while the OCI front is still
/// serving after the S3 front has ended, and runs once the OCI front ends. A stop placed after the
/// S3 front alone would set the flag before the OCI front is released. Deterministic: the OCI front
/// ends when the test sends, and the test yields the scheduler a fixed number of times, then looks.
#[tokio::test]
async fn the_heartbeat_stops_only_when_both_fronts_have_ended() {
    let stopped = AtomicBool::new(false);
    let (release, held) = oneshot::channel::<()>();
    let run = serve_fronts(
        async { 1 },
        async {
            held.await.expect("released");
            2
        },
        async { stopped.store(true, Ordering::SeqCst) },
    );
    let mut run = Box::pin(run);
    for _ in 0..16 {
        assert!(poll_once(&mut run).await.is_none());
        tokio::task::yield_now().await;
    }
    assert!(!stopped.load(Ordering::SeqCst), "stopped with OCI serving");
    release.send(()).expect("held");
    assert_eq!(run.await, (1, 2));
    assert!(stopped.load(Ordering::SeqCst), "not stopped at the end");
}

/// Polls `run` once: its output if it is ready, `None` if not.
async fn poll_once<T>(run: &mut Pin<Box<impl Future<Output = T>>>) -> Option<T> {
    std::future::poll_fn(|cx| match run.as_mut().poll(cx) {
        Poll::Ready(done) => Poll::Ready(Some(done)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}
