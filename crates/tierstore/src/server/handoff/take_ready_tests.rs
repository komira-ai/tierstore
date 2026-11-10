//! The `ready` step, from the taker's side: the request that asks for it, the connection that is
//! kept for it, and the fall back to a running instance that does not know it.

use tokio::net::UnixListener;

use crate::server::handoff::testing::{handoff, listening, pretending, scratch, socket};
use crate::server::handoff::wire;
use crate::server::handoff::{Front, HandoffError, Offer};

use super::{Want, take, take_with_ready};

/// Asserts that a taker that asks for the step is handed the sockets by a running instance that
/// takes part in it, and holds the connection: the instance has not ended after `done`, ends when
/// `ready` is sent, and the taker's copies of the sockets are the sockets.
#[tokio::test]
async fn a_taker_that_asks_for_ready_says_it_after_it_serves() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::Nfs, &listener)])
        .unwrap()
        .with_ready_wait(std::time::Duration::from_secs(3600));
    let serve = tokio::spawn(offer.serve());
    let addrs = [addr];
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &addrs,
    };
    let taken = take_with_ready(&handoff(path), &want)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.nfs_addrs, [addr]);
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
    assert!(!serve.is_finished(), "the instance waits for ready");
    taken
        .ready
        .expect("the instance takes part")
        .send()
        .await
        .unwrap();
    serve.await.unwrap();
}

/// Asserts that a taker that asks for the step of an instance that does not know it (one that
/// answers `refused` and "the request is not understood" to the second line, as every release
/// before the step does) asks again with `take 1` alone and is handed the sockets as before:
/// there is no `ready` to say, and the instance ended at `done`.
#[tokio::test]
async fn a_taker_asks_again_without_ready_of_an_instance_that_does_not_know_it() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let serve = tokio::spawn(
        Offer::bind(&path, vec![socket(Front::Nfs, &listener)])
            .unwrap()
            .serve(),
    );
    let addrs = [addr];
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &addrs,
    };
    let taken = take_with_ready(&handoff(path.clone()), &want)
        .await
        .unwrap()
        .unwrap();
    assert!(taken.ready.is_none());
    assert_eq!(taken.nfs_addrs, [addr]);
    serve.await.unwrap();
    assert!(!path.exists());
}

/// Asserts that [`take`] never asks for the step: an instance that takes part in it, asked by it,
/// ends at `done` (the S3 and OCI fronts hand over as they did).
#[tokio::test]
async fn a_plain_take_does_not_ask_for_the_step() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let offer = Offer::bind(&path, vec![socket(Front::S3, &listener)])
        .unwrap()
        .with_ready_wait(std::time::Duration::from_secs(3600));
    let serve = tokio::spawn(offer.serve());
    let addrs = [addr];
    let want = Want {
        s3: &addrs,
        oci: &[],
        nfs: &[],
    };
    let taken = take(&handoff(path), &want).await.unwrap().unwrap();
    assert!(taken.ready.is_none());
    serve.await.unwrap();
}

/// Asserts that a refusal for another reason than that the request is not understood is the
/// taker's error, with the instance's reason, and is not asked again: the second line is not
/// what was wrong.
#[tokio::test]
async fn a_refusal_for_another_reason_is_not_asked_again() {
    let (_dir, path) = scratch();
    let fake = pretending(
        &path,
        vec![
            "refused".to_owned(),
            "another taker is taking the sockets".to_owned(),
        ],
        Vec::new(),
        None,
    );
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[],
    };
    let error = take_with_ready(&handoff(path), &want).await.err().unwrap();
    assert_eq!(
        error.to_string(),
        "hand-off: the running instance refused: another taker is taking the sockets"
    );
    fake.await.unwrap();
}

/// Asserts that the first request of a taker that asks for the step has the second line `ready`,
/// and that the second, after a refusal as not understood, has not.
#[tokio::test]
async fn the_requests_of_a_taker_that_asks_for_ready_are_exact() {
    let (_dir, path) = scratch();
    let listener = UnixListener::bind(&path).unwrap();
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &[],
    };
    let handoff = handoff(path);
    let taking = take_with_ready(&handoff, &want);
    let fake = async {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            requests.push(wire::recv(&stream).await.unwrap().lines);
            let refused = [
                "refused".to_owned(),
                "the request is not understood".to_owned(),
            ];
            wire::send(&stream, &refused, &[]).await.unwrap();
        }
        requests
    };
    let (taken, requests) = tokio::join!(taking, fake);
    assert!(matches!(taken, Err(HandoffError::Refused(_))));
    assert_eq!(requests, [vec!["take 1", "ready"], vec!["take 1"]]);
}

/// Asserts that `ready` sent to a running instance that is gone from the connection (it did not
/// wait, and went on serving) is an error that says what was being done, so that the taker does
/// not serve beside it.
#[tokio::test]
async fn ready_to_an_instance_that_is_gone_is_an_error() {
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let fake = pretending(
        &path,
        vec!["offer".to_owned(), format!("nfs {addr}")],
        vec![listener],
        Some(vec!["done".to_owned()]),
    );
    let addrs = [addr];
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &addrs,
    };
    let taken = take_with_ready(&handoff(path), &want)
        .await
        .unwrap()
        .unwrap();
    fake.await.unwrap();
    let error = taken.ready.unwrap().send().await.err().unwrap();
    assert!(
        matches!(
            error,
            HandoffError::Io {
                what: "telling the running instance it can end",
                ..
            }
        ),
        "{error}"
    );
}

/// Asserts that a frame other than `bye` in answer to `ready` is an error of the protocol, so
/// that the taker does not serve on an answer it does not know.
#[tokio::test]
async fn an_answer_other_than_bye_to_ready_is_an_error() {
    use std::os::fd::AsFd as _;
    let (_dir, path) = scratch();
    let (listener, addr) = listening();
    let control = UnixListener::bind(&path).unwrap();
    let fake = tokio::spawn(async move {
        let (stream, _) = control.accept().await.unwrap();
        wire::recv(&stream).await.unwrap();
        let offer = vec!["offer".to_owned(), format!("nfs {addr}")];
        wire::send(&stream, &offer, &[listener.as_fd()])
            .await
            .unwrap();
        wire::recv(&stream).await.unwrap();
        wire::send(&stream, &["done".to_owned()], &[])
            .await
            .unwrap();
        wire::recv(&stream).await.unwrap();
        wire::send(&stream, &["nope".to_owned()], &[])
            .await
            .unwrap();
    });
    let addrs = [addr];
    let want = Want {
        s3: &[],
        oci: &[],
        nfs: &addrs,
    };
    let taken = take_with_ready(&handoff(path), &want)
        .await
        .unwrap()
        .unwrap();
    let error = taken.ready.unwrap().send().await.err().unwrap();
    fake.await.unwrap();
    assert!(
        matches!(
            error,
            HandoffError::Protocol("something other than bye after ready")
        ),
        "{error}"
    );
}
