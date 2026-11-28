//! A request of the rig, to the loopback S3 service of `objstore_s3::s3::fake` and to addresses
//! where nothing answers.

use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::port::dead_addr;

use super::*;

/// Asserts that a PUT is taken and a GET of the same key answers its bytes, with the statuses
/// the service gave, and that a GET of a key it does not hold answers its 404.
#[tokio::test]
async fn a_request_answers_its_status_and_body() {
    let s3 = FakeS3::start().await;
    let address = s3.endpoint().trim_start_matches("http://").to_owned();
    let put = call(&address, Method::PUT, "/cas/k", b"bytes".to_vec()).await;
    assert_eq!(put, Ok((200, Vec::new())));
    let get = call(&address, Method::GET, "/cas/k", Vec::new()).await;
    assert_eq!(get, Ok((200, b"bytes".to_vec())));
    let (status, _) = call(&address, Method::GET, "/cas/none", Vec::new())
        .await
        .unwrap();
    assert_eq!(status, 404);
}

/// Asserts that an address where nothing listens, and a path that is no URI, fail with the
/// request named.
#[tokio::test]
async fn a_request_that_cannot_be_made_names_itself() {
    let dead = dead_addr().to_string();
    let refused = call(&dead, Method::GET, "/metricz", Vec::new())
        .await
        .unwrap_err();
    assert!(
        refused.starts_with(&format!("GET http://{dead}/metricz: ")),
        "{refused}"
    );
    let refused = call("127.0.0.1:1", Method::PUT, "/a b", Vec::new())
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("PUT http://127.0.0.1:1/a b: "),
        "{refused}"
    );
}
