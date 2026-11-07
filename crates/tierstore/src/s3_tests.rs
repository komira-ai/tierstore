//! The unit tests of the S3 backend: the mapping of store errors, and the tags and ranges it passes on.

use std::sync::Arc;

use super::*;

fn service(status: u16) -> ObjectStoreError {
    ObjectStoreError::Service {
        status,
        code: "Code".to_owned(),
        message: "message".to_owned(),
    }
}

/// Asserts that the number of chunks given is how many wait for the request that sends them:
/// the feeder takes that many chunks of the body and one more (in hand, blocked) while the
/// request has taken none, for one, four and nine.
#[tokio::test]
async fn the_chunks_waiting_given_are_how_many_chunks_wait_for_the_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for (waiting, held) in [(1_usize, 1_usize), (4, 4), (9, 9)] {
        let pulled = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&pulled);
        let chunks = stream::iter((0..30).map(|_| Ok(Bytes::from_static(b"x"))));
        let body: PartBody<'_> = chunks
            .inspect(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .boxed();
        let request = |source| async move {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            drop(source);
            Ok::<usize, ObjectStoreError>(pulled.load(Ordering::SeqCst))
        };
        let seen = flowing(body, waiting, request).await;
        assert_eq!(seen.unwrap().unwrap(), held + 1, "{waiting}");
    }
}

/// Asserts that a backend made with `with_chunks_waiting(0)` holds one chunk, not none: the
/// channel of the bodies is never made with a depth of zero, which it refuses.
#[test]
fn a_depth_of_zero_chunks_waiting_is_one() {
    let config = S3Config {
        endpoint: "http://127.0.0.1:9".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tierstore-test".to_owned(),
        credentials: Credentials::new("k", "s"),
        capabilities: Capabilities::default(),
        connect_timeout: std::time::Duration::from_secs(1),
        request_timeout: std::time::Duration::from_secs(1),
        min_body_rate: None,
    };
    let backend = S3Backend::new(config).unwrap();
    assert_eq!(backend.chunks_waiting, CHUNKS_WAITING);
    assert_eq!(backend.with_chunks_waiting(0).chunks_waiting, 1);
}

/// Asserts that each answer of the client is the error of this crate the trait documents
/// for it: not found, already exists and a failed precondition name the key, a request
/// that did not complete and the statuses 408, 429 and every 5xx but 501 and 507 are
/// unavailable, 507 is full, and any other answer (501 among them) is an other error that
/// says what it was.
#[test]
fn answers_of_the_client_are_the_errors_the_trait_documents() {
    let key = ObjectKey::new("k").unwrap();
    let name = "k".to_owned();
    let found = map_error(ObjectStoreError::NotFound(key.clone()), "k");
    assert_eq!(found, Error::NotFound(name.clone()));
    let exists = map_error(ObjectStoreError::AlreadyExists(key), "k");
    assert_eq!(exists, Error::AlreadyExists(name.clone()));
    let failed = map_error(service(412), "k");
    assert_eq!(failed, Error::PreconditionFailed(name));

    let reset = Arc::new(std::io::Error::other("reset"));
    let down = map_error(ObjectStoreError::Transport(reset), "k");
    assert!(down.is_unavailable(), "{down}");
    for status in [408, 429, 500, 502, 503, 504, 505, 506, 508, 599] {
        let down = map_error(service(status), "k");
        assert!(down.is_unavailable(), "{status}");
    }
    let full = map_error(service(507), "k");
    assert!(matches!(full, Error::Full(_)), "{full}");
    for status in [400, 403, 409, 501] {
        let other = map_error(service(status), "k");
        assert!(matches!(other, Error::Other(_)), "{status}: {other}");
    }
    // Two writers racing a create-only put: the store does not say who won, so the answer
    // is retryable; a 409 of any other code is still an other error.
    let racing = ObjectStoreError::Service {
        status: 409,
        code: "ConditionalRequestConflict".to_owned(),
        message: "conflict".to_owned(),
    };
    let conflict = map_error(racing, "k");
    assert!(conflict.is_unavailable(), "{conflict}");
    let odd = map_error(ObjectStoreError::Protocol("odd".to_owned()), "k");
    assert!(odd.to_string().contains("odd"), "{odd}");
}
