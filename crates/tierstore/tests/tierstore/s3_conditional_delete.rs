//! A conditional delete of the S3 backend over the loopback S3 service, in each of its two ways:
//! a DELETE with `If-Match` where the tier's `conditional_delete` setting is on, and a read of the
//! tag before a plain DELETE where it is off (the default). The backend does not ask the store
//! which it enforces.

use objstore_s3::s3::fake::FakeS3;
use tierstore::{Backend, Error};

use crate::conformance;
use crate::s3::backend;
use crate::support::body;

/// The methods of the requests the service took after the first `from`.
fn methods(s3: &FakeS3, from: usize) -> Vec<String> {
    let lines = s3.requests();
    let sent = lines[from..].iter();
    sent.map(|line| line.split(' ').next().unwrap_or("").to_owned())
        .collect()
}

/// Asserts that a backend that has been made and not used has sent no request: nothing asks the
/// store anything, whether the setting is on or off.
#[tokio::test]
async fn a_backend_sends_no_request_until_it_is_used() {
    let (s3, backend) = backend().await;
    let _on = backend.with_conditional_delete(true);
    assert!(s3.requests().is_empty(), "{:?}", s3.requests());
}

/// Asserts that with the setting off (the default of an S3 backend) a conditional delete is correct
/// on any store: it deletes only the object with the tag (a wrong tag, an earlier tag and an
/// absent key are refused as the trait says), and a delete on the right tag is a HEAD and then a
/// DELETE, a wrong tag only a HEAD.
#[tokio::test]
async fn with_the_setting_off_a_conditional_delete_reads_the_tag_first() {
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

/// Asserts that with the setting on a conditional delete is one DELETE and no HEAD, and it
/// deletes only the object with the tag: the store's 412 is a failed precondition and its 404 is
/// `NotFound`.
#[tokio::test]
async fn with_the_setting_on_a_conditional_delete_is_one_delete_with_the_header() {
    let (s3, backend) = backend().await;
    let backend = backend.with_conditional_delete(true);
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

/// Asserts that the setting is trusted and not checked: over a store that takes `If-Match` on a
/// DELETE and ignores it, the backend with the setting on sends the one DELETE and no HEAD (it
/// does not look for the store ignoring the header), and with the setting off it is correct on
/// that store, because it reads the tag first.
#[tokio::test]
async fn the_setting_is_not_checked_against_the_store() {
    let (s3, backend) = backend().await;
    s3.ignore_delete_condition();
    conformance::a_conditional_delete_removes_only_the_object_with_that_tag(&backend).await;

    let backend = backend.with_conditional_delete(true);
    backend.put("j", body("v")).await.unwrap();
    let tag = backend.head("j").await.unwrap().tag;
    let before = s3.requests().len();
    backend.delete_if_match("j", &tag).await.unwrap();
    assert_eq!(methods(&s3, before), ["DELETE"]);
}

/// Asserts that a conditional delete of a key the client cannot name is an error before any
/// request, with the setting off and with it on.
#[tokio::test]
async fn a_key_the_client_cannot_name_sends_no_request() {
    let (s3, backend) = backend().await;
    let refused = backend.delete_if_match("a b", "tag").await.unwrap_err();
    assert!(matches!(refused, Error::Other(_)), "{refused}");
    let backend = backend.with_conditional_delete(true);
    let refused = backend.delete_if_match("a b", "tag").await.unwrap_err();
    assert!(matches!(refused, Error::Other(_)), "{refused}");
    assert_eq!(s3.requests(), Vec::<String>::new());
}
