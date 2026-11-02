//! A conditional delete of the S3 backend over the loopback S3 service, in each of its two ways:
//! a DELETE with `If-Match` where the service enforces it, and a read of the tag before a plain
//! DELETE where it does not, and the settling that picks between them.

use std::net::TcpListener;
use std::sync::Mutex;

use objstore_s3::s3::fake::{Failure, FakeS3};
use tierstore::{Backend, Error, S3Backend, SelfTestOptions, settle_conditional_delete};

use crate::conformance;
use crate::s3::{backend, config};
use crate::support::body;

/// The methods of the requests the service took after the first `from`.
fn methods(s3: &FakeS3, from: usize) -> Vec<String> {
    let lines = s3.requests();
    let sent = lines[from..].iter();
    sent.map(|line| line.split(' ').next().unwrap_or("").to_owned())
        .collect()
}

/// The options of a settling: only the name and the run id are read.
fn options() -> SelfTestOptions<'static> {
    SelfTestOptions {
        name: "the service",
        run_id: "run-1",
        multipart: false,
        skip: &[],
        wait: std::time::Duration::ZERO,
        pause: std::time::Duration::from_millis(1),
        max_pause: std::time::Duration::from_millis(1),
    }
}

/// Asserts that before the store has been asked, a conditional delete is correct on any store:
/// it deletes only the object with the tag (a wrong tag, an earlier tag and an absent key are
/// refused as the trait says), and a delete on the right tag is a HEAD and then a DELETE, a wrong
/// tag only a HEAD.
#[tokio::test]
async fn before_the_store_is_asked_a_conditional_delete_reads_the_tag_first() {
    let (s3, backend) = backend().await;
    conformance::a_conditional_delete_removes_only_the_object_with_that_tag(&backend).await;

    backend.put("j", body("v")).await.unwrap();
    let tag = backend.head("j").await.unwrap().tag;
    let before = s3.requests().len();
    let wrong = backend.delete_if_match("j", "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("j".to_owned())));
    assert_eq!(methods(&s3, before), ["HEAD"]);
    let before = s3.requests().len();
    backend.delete_if_match("j", &tag).await.unwrap();
    assert_eq!(methods(&s3, before), ["HEAD", "DELETE"]);
}

/// Asserts that once the settling found the store to enforce `If-Match`, it is named
/// `if-match`, the probe leaves no key, a conditional delete is one DELETE and no HEAD, and it
/// deletes only the object with the tag: the store's 412 is a failed precondition and its 404
/// is `NotFound`.
#[tokio::test]
async fn a_store_that_enforces_if_match_is_given_a_delete_with_the_header() {
    let (s3, backend) = backend().await;
    let way = backend.settle_conditional_delete("probe/conditional").await;
    assert_eq!(way.as_deref(), Some("if-match"));
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());
    conformance::a_conditional_delete_removes_only_the_object_with_that_tag(&backend).await;

    backend.put("j", body("v")).await.unwrap();
    let tag = backend.head("j").await.unwrap().tag;
    let before = s3.requests().len();
    backend.delete_if_match("j", &tag).await.unwrap();
    assert_eq!(methods(&s3, before), ["DELETE"]);
    let before = s3.requests().len();
    let again = backend.delete_if_match("j", &tag).await;
    assert_eq!(again, Err(Error::NotFound("j".to_owned())));
    assert_eq!(methods(&s3, before), ["DELETE"]);
}

/// Asserts that a store that takes `If-Match` on a DELETE and ignores it is settled on
/// `head-then-delete` (without failing), the probe leaves no key, and a wrong tag still deletes
/// nothing, because the tag is read first.
#[tokio::test]
async fn a_store_that_ignores_if_match_is_given_a_read_of_the_tag_before_the_delete() {
    let (s3, backend) = backend().await;
    s3.ignore_delete_condition();
    let way = backend.settle_conditional_delete("probe/conditional").await;
    assert_eq!(way.as_deref(), Some("head-then-delete"));
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());
    conformance::a_conditional_delete_removes_only_the_object_with_that_tag(&backend).await;
}

/// Asserts that a store that cannot be reached is settled on `head-then-delete` and not
/// refused: the start must go on, and the answer is the one that is right on every store.
#[tokio::test]
async fn a_store_that_cannot_be_asked_is_settled_on_the_read_of_the_tag() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let way = backend.settle_conditional_delete("probe/conditional").await;
    let way = way.unwrap();
    assert!(
        way.starts_with("head-then-delete (could not ask: "),
        "{way}"
    );
}

/// Asserts that a request that fails in the middle of the probe (the store answering 500, once)
/// settles the backend on `head-then-delete` with a line that says the store could not be asked
/// and why, not the line of a store that was asked and ignores the header, whichever of the five
/// requests of the questions it is, and that the probe key is gone afterwards. The probe is a
/// write, then HEAD, DELETE, HEAD, DELETE, HEAD.
#[tokio::test]
async fn a_request_that_fails_mid_probe_is_told_from_a_store_that_ignores_if_match() {
    let failures = [
        Failure::Head(1),
        Failure::Delete(1),
        Failure::Head(2),
        Failure::Delete(2),
        Failure::Head(3),
    ];
    for failure in failures {
        let (s3, backend) = backend().await;
        s3.fail(failure);
        let way = backend.settle_conditional_delete("probe/conditional").await;
        let way = way.unwrap();
        assert!(
            way.starts_with("head-then-delete (could not ask: ") && way.contains("500"),
            "{failure:?}: {way}"
        );
        assert!(s3.keys().is_empty(), "{failure:?}: {:?}", s3.keys());
    }
}

/// Asserts that a probe key the client cannot name is settled on `head-then-delete`, as one that
/// could not be asked, without a request, and that a conditional delete of such a key is an error before any request.
#[tokio::test]
async fn a_key_the_client_cannot_name_sends_no_request() {
    let (s3, backend) = backend().await;
    let way = backend.settle_conditional_delete("a b").await;
    let way = way.unwrap();
    assert!(
        way.starts_with("head-then-delete (could not ask: "),
        "{way}"
    );
    let refused = backend.delete_if_match("a b", "tag").await.unwrap_err();
    assert!(matches!(refused, Error::Other(_)), "{refused}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that the line of the settling names the way, with the name of the tier, for a store that
/// enforces `If-Match` and for one that does not, and that the probe key is under the run's probe
/// prefix and is gone afterwards.
#[tokio::test]
async fn the_settling_names_the_way_and_leaves_no_key() {
    for (ignores, line) in [
        (false, "the service: conditional delete: if-match"),
        (true, "the service: conditional delete: head-then-delete"),
    ] {
        let (s3, backend) = backend().await;
        if ignores {
            s3.ignore_delete_condition();
        }
        let notes = Mutex::new(Vec::new());
        let note = |line: String| notes.lock().unwrap().push(line);
        settle_conditional_delete(&backend, &options(), &note).await;
        assert_eq!(*notes.lock().unwrap(), [line]);
        assert!(s3.keys().is_empty(), "{:?}", s3.keys());
        let probe = s3.requests().into_iter().find(|r| r.starts_with("PUT "));
        let probe = probe.unwrap();
        assert!(
            probe.contains("_tiering/selftest/run-1/conditional-delete"),
            "{probe}"
        );
    }
}
