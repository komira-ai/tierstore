//! The plain `PUT` and the completion of an upload that the S3 front sends to the proxy
//! ([`Backend::put_stream_replacing`] and [`Backend::complete_upload_replacing`]): that they create
//! only under the entry format, that a hot tier which refuses a body after taking part of it is
//! not followed by the cold tier, and the races and refusals of the completion that replaces a key.
//!
//! No test waits: the order of the calls is made by gates and by planned errors.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use super::overwrite::PutClaim;
use super::plain_put_tests::{Rig, chunks, settle, until_parked};
use crate::{
    Backend, ByteRange, Error, GateBackend, Listed, Meta, Op, PartBody, Phase, ProxyConfig,
};

/// The format object of a store whose writes are made on entries.
const ENTRIES_FORMAT: &str = r#"{"epoch":2,"phase":"active"}"#;

fn landing(config: &mut ProxyConfig) {
    config.emergency_cold_landing = true;
}

/// A hot tier that answers `Full` to a streamed body: after it took the first piece of it, whatever
/// it is, if the flag says so (a store that ran out of room while it wrote), else before it took a
/// byte (a store that has no room to begin with).
struct Takes(Arc<dyn Backend>, bool);

#[async_trait]
impl Backend for Takes {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.0.put(key, body).await
    }

    async fn put_stream_tagged(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<String, Error> {
        if self.1 {
            let _ = body.next().await;
        }
        Err(Error::Full(key.to_owned()))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.0.put_if_match(key, body, etag).await
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
        _etag: &str,
    ) -> Result<String, Error> {
        let _ = body.next().await;
        Err(Error::Full(key.to_owned()))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.0.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.0.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.0.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.0.list(prefix).await
    }
}

/// A rig whose hot tier is [`Takes`] and whose landing is on.
async fn taking() -> Rig {
    let wrap = |hot: Arc<GateBackend>| -> Arc<dyn Backend> { Arc::new(Takes(hot, true)) };
    Rig::over(super::plain_put_tests::MARKS_FORMAT, false, landing, wrap).await
}

/// A rig whose hot tier is [`Takes`] that refuses before it takes a byte, and whose landing is on.
async fn refusing() -> Rig {
    let wrap = |hot: Arc<GateBackend>| -> Arc<dyn Backend> { Arc::new(Takes(hot, false)) };
    Rig::over(super::plain_put_tests::MARKS_FORMAT, false, landing, wrap).await
}

/// Asserts that under the entry format the plain put of the trait creates and a second one is
/// refused as `AlreadyExists`, and that no marker is asked for: only the format `active-marks`
/// replaces.
#[tokio::test]
async fn under_the_entry_format_a_replacing_put_creates_only() {
    let rig = Rig::with(ENTRIES_FORMAT, |_| {}).await;
    let claim = PutClaim::default();
    let put = |text: &'static str| {
        let rig = &rig;
        let claim = &claim;
        async move {
            let body = chunks(&[text]);
            rig.proxy
                .put_stream_replacing("k", Some(3), body, claim)
                .await
        }
    };
    put("one").await.unwrap();
    assert_eq!(put("two").await, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("one"));
    assert!(rig.marks_log().is_empty());
}

/// Asserts what a plain put says of the object it made, for a cache that keeps a copy: under
/// `active-marks` the answer is the tag of the hot object (the tag a HEAD gives), for a key that
/// is new and for an overwrite; under the entry format it is the tag of the create. A hot tier
/// that answers `Full` before it took a byte, with the landing on, lands the object in the cold
/// tier and the answer has no tag, since that tag moves when the object is copied the other way.
#[tokio::test]
async fn a_replacing_put_answers_with_the_tag_of_the_hot_object_and_none_for_a_landing() {
    let claim = PutClaim::default();
    for format in [super::plain_put_tests::MARKS_FORMAT, ENTRIES_FORMAT] {
        let rig = Rig::with(format, |_| {}).await;
        let answer = rig
            .proxy
            .put_stream_replacing("k", Some(3), chunks(&["one"]), &claim)
            .await;
        assert_eq!(answer.unwrap().unwrap().tag, rig.hot_tag("k").await);
        if format == ENTRIES_FORMAT {
            continue;
        }
        let answer = rig
            .proxy
            .put_stream_replacing("k", Some(3), chunks(&["two"]), &claim)
            .await;
        settle().await;
        assert_eq!(rig.hot_text("k").await.as_deref(), Some("two"));
        assert_eq!(answer.unwrap().unwrap().tag, rig.hot_tag("k").await);
    }

    let rig = refusing().await;
    let answer = rig
        .proxy
        .put_stream_replacing("k", Some(3), chunks(&["new"]), &claim)
        .await;
    settle().await;
    assert_eq!(answer, Ok(None));
    assert_eq!(rig.cold_text("k").await.as_deref(), Some("new"));
}

/// Asserts that under the entry format the completion of the trait creates only, too.
#[tokio::test]
async fn under_the_entry_format_a_replacing_completion_creates_only() {
    let rig = Rig::with(ENTRIES_FORMAT, |_| {}).await;
    for (text, made) in [("one", true), ("two", false)] {
        let id = rig.proxy.create_upload("k").await.unwrap();
        let part = rig.proxy.upload_part("k", &id, 1, chunks(&[text])).await;
        let parts = [(1, part.unwrap())];
        let done = rig.proxy.complete_upload_replacing("k", &id, &parts).await;
        assert_eq!(done.is_ok(), made, "{done:?}");
    }
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("one"));
    assert!(rig.marks_log().is_empty());
}

/// Asserts that a hot tier which takes part of a body before it answers `Full` is not followed
/// by the cold tier, with the landing on: the bytes it took cannot be sent again, so the put is
/// answered `Full` for the client to retry, for a key the hot tier lacks and for one it holds,
/// and nothing is written to the cold tier. The key that existed keeps its bytes and its lock is
/// let go.
#[tokio::test]
async fn a_hot_tier_that_took_part_of_the_body_before_it_said_full_is_not_followed_by_cold() {
    let rig = taking().await;
    let claim = PutClaim::default();
    let put = |key: &'static str| {
        let rig = &rig;
        let claim = &claim;
        async move {
            let body = chunks(&["ab", "cd"]);
            rig.proxy
                .put_stream_replacing(key, Some(4), body, claim)
                .await
        }
    };
    assert_eq!(put("new").await, Err(Error::Full("new".to_owned())));
    assert_eq!(rig.marker("new").await, None);

    rig.seed_hot("old", "kept").await;
    assert_eq!(put("old").await, Err(Error::Full("old".to_owned())));
    settle().await;
    assert_eq!(rig.hot_text("old").await.as_deref(), Some("kept"));
    assert!(rig.marker("old").await.is_none_or(|m| !m.st.is_held()));
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Asserts that a body that ended with an error is not followed by the cold tier either, whatever
/// the hot tier said: there is no body left to send.
#[tokio::test]
async fn a_body_that_failed_is_not_sent_to_the_cold_tier() {
    let rig = taking().await;
    let broken = futures::stream::iter(vec![Err(Error::Other("cut".to_owned()))]).boxed();
    let claim = PutClaim::default();
    let put = rig
        .proxy
        .put_stream_replacing("k", Some(4), broken, &claim)
        .await;
    assert_eq!(put, Err(Error::Full("k".to_owned())));
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Gives an upload of `key` with one part of `text` and the parts to complete it with.
async fn upload(rig: &Rig, key: &str, text: &'static str) -> (String, Vec<(u32, String)>) {
    let id = rig.proxy.create_upload(key).await.unwrap();
    let part = rig.proxy.upload_part(key, &id, 1, chunks(&[text])).await;
    (id, vec![(1, part.unwrap())])
}

/// Asserts that the completion of an upload of a key the hot tier lacks creates the object with its
/// marker, and that one of a key it holds replaces it under the lock, with the marker released
/// `DIRTY`.
#[tokio::test]
async fn a_replacing_completion_creates_a_new_key_and_replaces_one_that_exists() {
    let rig = Rig::new().await;
    let (id, parts) = upload(&rig, "k", "one").await;
    rig.proxy
        .complete_upload_replacing("k", &id, &parts)
        .await
        .unwrap();
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("one"));
    assert!(rig.marker("k").await.is_some());

    let (id, parts) = upload(&rig, "k", "two").await;
    let tag = rig.proxy.complete_upload_replacing("k", &id, &parts).await;
    settle().await;
    assert_eq!(tag.unwrap(), rig.hot_tag("k").await);
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("two"));
    assert!(rig.marker("k").await.is_some_and(|m| !m.st.is_held()));
    assert!(rig.cold_faults.call_log().is_empty());
}

/// Asserts that an upload begun in the cold tier is refused with nothing asked of any tier: its
/// object would land in the cold tier, which no completion does.
#[tokio::test]
async fn a_replacing_completion_of_an_upload_begun_in_cold_is_unavailable() {
    let rig = Rig::new().await;
    let done = rig
        .proxy
        .complete_upload_replacing("k", "a-cold-upload", &[])
        .await;
    assert!(matches!(done, Err(Error::Unavailable(_))), "{done:?}");
    assert!(rig.hot_log().is_empty());
}

/// Asserts that a completion that finds no object at wave 1 and loses the creation to another
/// writer goes on as the replace of what the winner made.
#[tokio::test]
async fn a_replacing_completion_that_lost_the_creation_replaces_what_the_winner_made() {
    let rig = Rig::new().await;
    let (id, parts) = upload(&rig, "k", "mine").await;
    let create = rig.hot.gate(Op::CompleteUpload, "k", Phase::Pre);
    let mut done = Box::pin(rig.proxy.complete_upload_replacing("k", &id, &parts));
    until_parked(&create, &mut done).await;
    rig.seed_hot("k", "theirs").await;
    assert!(create.release());
    done.await.unwrap();
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("mine"));
}

/// Asserts that a completion told the key exists when the second look finds none (it was made and
/// removed) is told to retry and writes nothing.
#[tokio::test]
async fn a_replacing_completion_whose_winner_was_removed_is_unavailable() {
    let rig = Rig::new().await;
    let (id, parts) = upload(&rig, "k", "mine").await;
    let exists = Error::AlreadyExists("k".to_owned());
    rig.hot_faults.fail_next_with(Op::CompleteUpload, exists);
    let done = rig.proxy.complete_upload_replacing("k", &id, &parts).await;
    assert!(matches!(done, Err(Error::Unavailable(_))), "{done:?}");
    assert_eq!(rig.hot_text("k").await, None);
}

/// Asserts that a replace refused for its tag reads the tag again and tries again, and that an
/// object removed after wave 1 is created by the completion under the lock.
#[tokio::test]
async fn a_replacing_completion_reads_the_tag_again_when_it_is_refused() {
    let rig = Rig::new().await;
    rig.seed_hot("k", "old").await;
    let (id, parts) = upload(&rig, "k", "one").await;
    let moved = Error::PreconditionFailed("k".to_owned());
    rig.hot_faults.fail_next_with(Op::CompleteUpload, moved);
    rig.proxy
        .complete_upload_replacing("k", &id, &parts)
        .await
        .unwrap();
    settle().await;
    assert_eq!(rig.hot_text("k").await.as_deref(), Some("one"));

    let (id, parts) = upload(&rig, "gone", "two").await;
    rig.seed_hot("gone", "old").await;
    let lock = rig.marks.gate(Op::Put, "gone", Phase::Post);
    let mut done = Box::pin(rig.proxy.complete_upload_replacing("gone", &id, &parts));
    until_parked(&lock, &mut done).await;
    rig.hot_mem.delete("gone").await.unwrap();
    assert!(lock.release());
    done.await.unwrap();
    settle().await;
    assert_eq!(rig.hot_text("gone").await.as_deref(), Some("two"));
}
