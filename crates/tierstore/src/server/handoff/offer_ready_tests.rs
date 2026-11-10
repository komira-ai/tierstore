//! The `ready` step, from the running instance's side: a taker that asks for it has handed over
//! nothing until it says `ready`, and one that is gone, wrong or silent leaves the instance serving.

use std::time::Duration;

use crate::server::handoff::testing::{ask, connect, listening, scratch, send, socket};
use crate::server::handoff::{Front, wire};

use super::Offer;

/// A wait that only a hung test reaches.
const WAIT: Duration = Duration::from_secs(3600);

/// The text of a frame's lines, for a comparison.
fn texts(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// An offer of one NFS socket that takes part in the `ready` step, serving, and what it tells its
/// hold.
fn serving(
    path: &std::path::Path,
    wait: Duration,
) -> (
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
) {
    let (nfs, _) = listening();
    let told = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&told);
    let offer = Offer::bind(path, vec![socket(Front::Nfs, &nfs)])
        .unwrap()
        .with_ready_wait(wait)
        .with_hold(std::sync::Arc::new(move |on| seen.lock().unwrap().push(on)));
    (tokio::spawn(offer.serve()), told)
}

/// The inode of the file at `path`.
fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).unwrap().ino()
}

/// Waits until the file at `path` is another one than the inode `was`: the instance has made its
/// control socket again.
async fn remade(path: &std::path::Path, was: u64) {
    use std::os::unix::fs::MetadataExt as _;
    crate::server::handoff::testing::within("the control socket made again", async {
        while std::fs::symlink_metadata(path).map_or(true, |meta| meta.ino() == was) {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Lets every task that can run, run.
async fn settle() {
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
}

/// A taker that asks for the `ready` step, answers `ack`, and reads `done`: the connection.
async fn through_done(path: &std::path::Path) -> tokio::net::UnixStream {
    let taker = connect(path).await;
    let offer = ask(&taker, &["take 1", "ready"]).await;
    assert_eq!(offer.lines[0], "offer");
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), vec!["done"]);
    taker
}

/// Asserts that a taker that asked for the step and says `ready` after `done` has taken the
/// sockets, and not before: the serve is still going after `done` is read, the control socket's
/// file is gone from `done` on, and `ready` ends the serve without the hold being let go.
#[tokio::test]
async fn an_instance_ends_when_the_taker_says_ready_and_not_at_done() {
    let (_dir, path) = scratch();
    let (serve, told) = serving(&path, WAIT);
    let taker = through_done(&path).await;
    settle().await;
    assert!(!serve.is_finished(), "done is not the end of the hand-off");
    assert!(!path.exists());
    let bye = ask(&taker, &["ready"]).await;
    assert_eq!(texts(&bye.lines), vec!["bye"], "the taker serves on bye");
    serve.await.unwrap();
    assert!(!path.exists(), "the successor's path is left to it");
    assert_eq!(*told.lock().unwrap(), [true]);
}

/// Asserts that a taker that answered `ack`, read `done` and is gone before it says `ready` has
/// taken nothing: the instance makes its control socket again (a file the taker may have replaced
/// is replaced), lets the hold go, serves on, and the next taker is offered the sockets and takes
/// them.
#[tokio::test]
async fn a_taker_gone_after_done_before_ready_leaves_the_instance_serving() {
    let (_dir, path) = scratch();
    let (serve, told) = serving(&path, WAIT);
    let was = inode(&path);
    drop(through_done(&path).await);
    remade(&path, was).await;
    assert!(!serve.is_finished());
    assert_eq!(*told.lock().unwrap(), [true, false]);

    let later = through_done(&path).await;
    send(&later, &["ready"]).await;
    serve.await.unwrap();
    assert_eq!(*told.lock().unwrap(), [true, false, true]);
}

/// Asserts that a taker that sends a frame that is not `ready` after `done` has not taken the
/// sockets either: the instance serves on with its control socket made again, and the hold let
/// go.
#[tokio::test]
async fn a_frame_other_than_ready_after_done_leaves_the_instance_serving() {
    let (_dir, path) = scratch();
    let (serve, told) = serving(&path, WAIT);
    let was = inode(&path);
    let taker = through_done(&path).await;
    send(&taker, &["ack"]).await;
    remade(&path, was).await;
    assert!(!serve.is_finished());
    assert_eq!(*told.lock().unwrap(), [true, false]);
    serve.abort();
}

/// Asserts, on the clock of the runtime (paused, so that no real time passes and no test waits),
/// that a taker that is connected and silent after `done` is given up once the wait of the offer
/// has passed: the instance makes its control socket again and lets the hold go, though the
/// taker's connection is open still. Before the wait has passed it has not.
#[tokio::test(start_paused = true)]
async fn a_taker_silent_for_the_wait_after_done_is_given_up() {
    let (_dir, path) = scratch();
    let (serve, told) = serving(&path, Duration::from_secs(10));
    let taker = through_done(&path).await;
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(
        !path.exists(),
        "the wait has not passed: the file is the taker's"
    );
    assert_eq!(*told.lock().unwrap(), [true]);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(path.exists(), "the control socket is made again");
    assert_eq!(*told.lock().unwrap(), [true, false]);
    assert!(!serve.is_finished());
    drop(taker);
    serve.abort();
}

/// Asserts that a taker whose `ready` arrives after the instance gave up (the wait passed with
/// the connection open) is not answered `bye`: the connection is closed on it, which is what
/// tells it not to serve, since the instance serves on.
#[tokio::test(start_paused = true)]
async fn a_ready_that_arrives_after_the_instance_gave_up_is_not_answered_bye() {
    let (_dir, path) = scratch();
    let (serve, told) = serving(&path, Duration::from_secs(10));
    let taker = through_done(&path).await;
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(*told.lock().unwrap(), [true, false]);
    // The connection is closed: the write fails, or the read ends with no `bye`.
    let ready = ["ready".to_owned()];
    let answered = match wire::send(&taker, &ready, &[]).await {
        Ok(()) => wire::recv(&taker).await.map(|_| ()),
        Err(why) => Err(why),
    };
    assert!(answered.is_err(), "no bye: the connection is closed");
    assert!(!serve.is_finished());
    serve.abort();
}

/// Asserts that an instance that was given no wait does not take part in the step: a request that
/// names it is refused as not understood, in the exact words (which is what makes a taker ask
/// again without it), and the instance goes on.
#[tokio::test]
async fn an_instance_with_no_ready_wait_refuses_a_request_that_asks_for_the_step() {
    let (_dir, path) = scratch();
    let (nfs, _) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::Nfs, &nfs)]).unwrap();
    let serve = tokio::spawn(offer.serve());
    let taker = connect(&path).await;
    let refused = ask(&taker, &["take 1", "ready"]).await;
    assert_eq!(
        texts(&refused.lines),
        vec!["refused", "the request is not understood"]
    );
    assert!(refused.fds.is_empty());
    serve.abort();
}

/// Asserts that a request for the step with another second line, or a third, is refused as not
/// understood, and that a request that does not ask for the step is handed over at `done`, with
/// no `ready`, though the instance takes part in the step: a taker of a release that has none is
/// served as it was.
#[tokio::test]
async fn a_taker_that_does_not_ask_for_the_step_is_handed_over_at_done() {
    let (_dir, path) = scratch();
    let (serve, _) = serving(&path, WAIT);
    for request in [
        vec!["take 1", "soon"],
        vec!["take 2", "ready"],
        vec!["take 1", "ready", "now"],
        vec!["ready"],
    ] {
        let client = connect(&path).await;
        let refused = ask(&client, &request).await;
        assert_eq!(
            texts(&refused.lines),
            vec!["refused", "the request is not understood"],
            "{request:?}"
        );
    }
    let taker = connect(&path).await;
    ask(&taker, &["take 1"]).await;
    let done = ask(&taker, &["ack"]).await;
    assert_eq!(texts(&done.lines), vec!["done"]);
    serve.await.unwrap();
    // The conversation of the plain request ended at `done`: nothing is read after it.
    assert!(wire::recv(&taker).await.is_err());
}
