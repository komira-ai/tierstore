//! DeleteObjects over a proxy under the format `active-marks`: each key is a delete on its marker
//! (`tiered/marks_delete.rs`), the call is answered when every key has finished, a key that is busy
//! is an `Error` of its own, and the keys finish if the client goes away.

use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::StatusCode;

use crate::faulty::Op;
use crate::tiered::marks_delete_tests::{Rig, dirty_shape, foreign_lock, let_the_others_run};
use crate::{Backend, MarkerWhy, Phase, TestClock};

use super::auth::sign;
use super::delete_objects_tests::{document, errors};
use super::testing::{BUCKET, KEY_ID, Reply, SECRET, checksum, texts};
use super::{Server, ServerConfig};

const MD5: &str = "content-md5";

/// A server over the proxy of the rig.
fn server_over(rig: &Rig) -> (Arc<Server>, ServerConfig) {
    let config = ServerConfig::new(BUCKET, KEY_ID, SECRET, Arc::new(TestClock::new()));
    let backend: Arc<dyn Backend> = rig.proxy.clone();
    (Server::new(backend, config.clone()), config)
}

/// Sends DeleteObjects for `keys` (`quiet` as the document spells it) and reads the answer.
async fn send(server: &Server, config: &ServerConfig, keys: &[&str], quiet: Option<&str>) -> Reply {
    let body = document(keys, quiet);
    let sum = checksum(MD5, body.as_bytes());
    let uri = format!("/{BUCKET}?delete");
    let request = sign(
        config,
        "POST",
        &uri,
        &[(MD5, sum.as_str())],
        body.as_bytes(),
    );
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Asserts that the call is not answered while one key's hot DELETE is parked, though the other key
/// has finished, and that once it is let go the call lists both keys as deleted, the objects are
/// gone from the hot tier, both markers say `gone`, and the cold tier was asked nothing.
#[tokio::test]
async fn delete_objects_answers_only_after_every_key_has_finished() {
    let rig = Rig::new().await;
    rig.seed_both("a").await;
    rig.seed_both("b").await;
    let (server, config) = server_over(&rig);
    let gate = rig.hot.gate(Op::Delete, "", Phase::Pre);
    let request = tokio::spawn(async move { send(&server, &config, &["a", "b"], None).await });
    gate.parked().await;
    let_the_others_run().await;
    assert!(!request.is_finished());

    assert!(gate.release());
    let reply = request.await.unwrap();
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(texts(&reply.body, "Key"), ["a", "b"]);
    assert!(errors(&reply.body).is_empty());
    let_the_others_run().await;
    for key in ["a", "b"] {
        assert!(!rig.hot_has(key).await, "{key}");
        assert_eq!(rig.shape(key).await, dirty_shape(true), "{key}");
    }
    assert!(rig.cold.call_log().is_empty());
}

/// Asserts that a key whose lock is still there when `lock_wait` ends is an `Error` of its own with
/// the code `SlowDown` and the fixed text, that the key beside it is `Deleted`, and that the busy
/// key's object and lock are as they were.
#[tokio::test]
async fn delete_objects_lists_a_key_whose_lock_wait_ended_as_a_slowdown_error() {
    let rig = Rig::new().await;
    rig.seed_both("a").await;
    rig.seed_both("b").await;
    let theirs = foreign_lock("b", MarkerWhy::Overwrite);
    rig.seed_marker(&theirs).await;
    let (server, config) = server_over(&rig);
    let reply = send(&server, &config, &["a", "b"], None).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(texts(&reply.body, "Key"), ["a", "b"]);
    let listed = errors(&reply.body);
    let text = "The key is being changed by another request; retry.";
    assert_eq!(
        listed,
        [("b".to_owned(), "SlowDown".to_owned(), text.to_owned())]
    );
    assert!(rig.hot_has("b").await);
    assert_eq!(rig.stored("b").await, Some(theirs));
    assert!(!rig.hot_has("a").await);
}

/// Asserts that in Quiet mode the busy key is listed and the deleted key is not.
#[tokio::test]
async fn delete_objects_quiet_mode_lists_only_the_errors() {
    let rig = Rig::new().await;
    rig.seed_both("a").await;
    rig.seed_both("b").await;
    rig.seed_marker(&foreign_lock("b", MarkerWhy::Overwrite))
        .await;
    let (server, config) = server_over(&rig);
    let reply = send(&server, &config, &["a", "b"], Some("true")).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(texts(&reply.body, "Deleted").is_empty());
    let keys: Vec<String> = errors(&reply.body)
        .into_iter()
        .map(|(key, ..)| key)
        .collect();
    assert_eq!(keys, ["b"]);
    assert!(!rig.hot_has("a").await);
}

/// Asserts that a client that goes away while the keys are being deleted leaves none half done:
/// both objects are deleted, both locks are released to `DIRTY(gone)`.
#[tokio::test]
async fn delete_objects_finishes_its_keys_after_the_client_disconnects() {
    let rig = Rig::new().await;
    rig.seed_both("a").await;
    rig.seed_both("b").await;
    let (server, config) = server_over(&rig);
    let gate = rig.hot.gate(Op::Delete, "", Phase::Pre);
    let request = tokio::spawn(async move { send(&server, &config, &["a", "b"], None).await });
    gate.parked().await;
    request.abort();
    assert!(request.await.err().is_some_and(|gone| gone.is_cancelled()));
    assert!(gate.release());
    let_the_others_run().await;

    for key in ["a", "b"] {
        assert!(!rig.hot_has(key).await, "{key}");
        assert_eq!(rig.shape(key).await, dirty_shape(true), "{key}");
    }
}
