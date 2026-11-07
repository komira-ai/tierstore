//! The S3 front over a store whose format object says `active-marks`
//! (`docs/design/put-without-lock.md`, 5.1, 5.3 to 5.5): a `PUT` with no condition replaces a key
//! that exists, `If-None-Match: *` only creates, a put of the bytes the key holds writes nothing,
//! and a hot tier that has no room is answered 503 `SlowDown` or, if the store is set to, lands the
//! object in the cold tier. Each test counts the requests the stores received for the key: the hot
//! bucket, its marks bucket and the cold tier. A request that follows the answer (the release of
//! the lock on the marker) is counted once the lock is let go.
//!
//! No test waits for a clock: the release is waited for by giving the other tasks turns.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use tierstore::server::{AccessKey, Grant, PresignKey, PresignRequest, presign_put};
use tierstore::server::{Prefixes, ScopedKey};
use tierstore::{
    Backend, Error, MarkBirth, MarkerState, MarkerWhy, Markers, MarkersConfig, Mono, Op,
    TokioSleeper, decode_marker,
};

use crate::raw::{call, send};
use crate::support::{BUCKET, FIXED_MILLIS, Harness};

/// The key of the tests.
const K: &str = "builds/k";

/// A server over a store that runs on markers.
async fn marks() -> Harness {
    Harness::start_marks(|_| {}, |_| {}).await
}

/// A server over a store that runs on markers whose proxy lands a put the hot tier refused in
/// the cold tier.
async fn landing() -> Harness {
    Harness::start_marks(|proxy| proxy.emergency_cold_landing = true, |_| {}).await
}

/// The requests the three stores received for a key since the last look.
#[derive(Debug, Default, PartialEq, Eq)]
struct Requests {
    hot: Vec<Op>,
    marks: Vec<Op>,
    cold: Vec<Op>,
}

/// Where the logs of the stores stood at the last look.
struct Cursor(usize, usize, usize);

impl Cursor {
    fn at(h: &Harness) -> Self {
        Self(
            h.hot.call_log().len(),
            h.marks.call_log().len(),
            h.cold.call_log().len(),
        )
    }

    /// The requests for `key` since the last look, which this look ends.
    fn take(&mut self, h: &Harness, key: &str) -> Requests {
        let ops = |log: Vec<(Op, String)>, from: usize| -> Vec<Op> {
            let named = log.into_iter().skip(from);
            named
                .filter(|(_, name)| name == key)
                .map(|(op, _)| op)
                .collect()
        };
        let seen = Requests {
            hot: ops(h.hot.call_log(), self.0),
            marks: ops(h.marks.call_log(), self.1),
            cold: ops(h.cold.call_log(), self.2),
        };
        *self = Self::at(h);
        seen
    }
}

/// Lets the other tasks run until no one holds a lock on the marker of `key`: the release that
/// follows the answer of a put.
async fn released(h: &Harness, key: &str) {
    for _ in 0..100_000 {
        let held = match h.marks_mem.get(key, None).await {
            Ok(body) => decode_marker(&body).unwrap().st.is_held(),
            Err(_) => false,
        };
        if !held {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the lock on {key} was never released");
}

/// Lets the other tasks run until `key` has no marker.
async fn unmarked(h: &Harness, key: &str) {
    for _ in 0..100_000 {
        if h.marks_mem.get(key, None).await.is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the marker of {key} never went");
}

/// The state of the marker of `key` and whether it says `gone`.
async fn shape(h: &Harness, key: &str) -> Option<(MarkerState, bool)> {
    let body = h.marks_mem.get(key, None).await.ok()?;
    let marker = decode_marker(&body).unwrap();
    Some((marker.st, marker.gone))
}

/// The text of an object in a tier.
async fn text(tier: &dyn Backend, key: &str) -> Option<String> {
    let body = tier.get(key, None).await.ok()?;
    Some(String::from_utf8(body.to_vec()).unwrap())
}

/// `x-amz-checksum-sha256` for `body`.
fn checksum(body: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(body))
}

/// The code of the error document of an answer.
async fn code(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap();
    let (_, rest) = text.split_once("<Code>").unwrap();
    rest.split_once("</Code>").unwrap().0.to_owned()
}

/// A plain `PUT`: no condition.
async fn plain(h: &Harness, key: &str, body: &[u8]) -> reqwest::Response {
    call(h, "PUT", key, &[], &[], body).await
}

/// A `PUT` with `If-None-Match: *`.
async fn create(h: &Harness, key: &str, body: &[u8]) -> reqwest::Response {
    call(h, "PUT", key, &[], &[("if-none-match", "*")], body).await
}

/// Asserts what a plain put of a new key asks of the stores: the hot HEAD with the marker GET (the
/// look an overwrite needs), then the object and the marker, and nothing of the cold tier. The
/// object is served and its marker says `DIRTY`.
#[tokio::test]
async fn a_plain_put_of_a_new_key_writes_the_object_and_its_marker_and_never_asks_cold() {
    let h = marks().await;
    let mut seen = Cursor::at(&h);
    let put = plain(&h, K, b"one").await;
    assert_eq!(put.status(), 200);
    assert_eq!(
        seen.take(&h, K),
        Requests {
            hot: vec![Op::Head, Op::Put],
            marks: vec![Op::Get, Op::Put],
            cold: vec![],
        }
    );
    assert_eq!(shape(&h, K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
}

/// Asserts that a plain put of a key that exists is 200 and the key then holds the new bytes (the
/// last writer wins, as in S3), and what the put asks of the stores: the hot HEAD with the marker
/// GET, the compare-and-swap of the marker to a lock, the HEAD under the lock (the body is a
/// stream, whose bytes cannot be sent twice, so the tag is read again), the replace by that tag,
/// and, after the answer, the release of the lock. Nothing is asked of the cold tier.
#[tokio::test]
async fn a_plain_put_of_a_key_that_exists_replaces_it_and_answers_200() {
    let h = marks().await;
    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    let mut seen = Cursor::at(&h);
    let put = plain(&h, K, b"two").await;
    assert_eq!(put.status(), 200);
    released(&h, K).await;
    assert_eq!(
        seen.take(&h, K),
        Requests {
            hot: vec![Op::Head, Op::Head, Op::PutIfMatch],
            marks: vec![Op::Get, Op::PutIfMatch, Op::PutIfMatch],
            cold: vec![],
        }
    );
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("two"));
    let read = call(&h, "GET", K, &[], &[], b"").await;
    assert_eq!(read.text().await.unwrap(), "two");
    assert_eq!(shape(&h, K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts that a put with `If-None-Match: *` of a key that exists is 412 and writes nothing, as
/// before, after the re-check of 5.1: the hot HEAD, the marker GET and the cold HEAD, then the
/// marker once more. The hot tier receives no write and the object is as it was.
#[tokio::test]
async fn a_put_with_if_none_match_of_a_key_that_exists_is_412_and_writes_nothing() {
    let h = marks().await;
    assert_eq!(create(&h, K, b"one").await.status(), 200);
    let mut seen = Cursor::at(&h);
    let refused = create(&h, K, b"two").await;
    assert_eq!(refused.status(), 412);
    assert_eq!(code(refused).await, "PreconditionFailed");
    assert_eq!(
        seen.take(&h, K),
        Requests {
            hot: vec![Op::Head],
            marks: vec![Op::Get, Op::Get],
            cold: vec![Op::Head],
        }
    );
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
}

/// Asserts the answer decided on 2026-11-06 to a create-only put that meets a delete: while the
/// marker says a delete holds the key the put is 409 `ConditionalRequestConflict` and writes
/// nothing, and once the delete has removed the key the same put creates it.
#[tokio::test]
async fn a_put_with_if_none_match_that_meets_a_delete_is_409_and_is_free_after_it() {
    let h = marks().await;
    assert_eq!(create(&h, K, b"one").await.status(), 200);
    released(&h, K).await;
    let markers = Markers::new(
        h.marks.clone(),
        "a-deleter",
        MarkersConfig::default(),
        std::sync::Arc::new(Mono::new()),
        std::sync::Arc::new(TokioSleeper),
    )
    .unwrap();
    // The marker of a delete that has locked the key.
    h.marks_mem.delete(K).await.unwrap();
    markers
        .create(K, MarkBirth::Lock(MarkerWhy::Delete))
        .await
        .unwrap();
    let mut seen = Cursor::at(&h);
    let met = create(&h, K, b"two").await;
    assert_eq!(met.status(), 409);
    assert_eq!(code(met).await, "ConditionalRequestConflict");
    let requests = seen.take(&h, K);
    assert!(
        !requests
            .hot
            .iter()
            .any(|op| matches!(op, Op::Put | Op::PutIfMatch)),
        "{requests:?}"
    );
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));

    // The delete is done: the object and the marker are gone.
    h.hot_mem.delete(K).await.unwrap();
    h.marks_mem.delete(K).await.unwrap();
    assert_eq!(create(&h, K, b"two").await.status(), 200);
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("two"));
}

/// Asserts that a plain put whose `x-amz-checksum-sha256` is the checksum of the bytes the key
/// holds is 200 and writes nothing: the hot HEAD and the marker GET are all the stores receive, and
/// the object, its marker and its tag are as they were. The same put with other bytes and their
/// checksum replaces the key.
#[tokio::test]
async fn a_plain_put_of_the_bytes_the_key_holds_with_their_checksum_writes_nothing() {
    let h = marks().await;
    assert_eq!(plain(&h, K, b"same").await.status(), 200);
    released(&h, K).await;
    let tag = h.hot_mem.head(K).await.unwrap().tag;
    let mut seen = Cursor::at(&h);
    let sum = checksum(b"same");
    let again = call(
        &h,
        "PUT",
        K,
        &[],
        &[("x-amz-checksum-sha256", &sum)],
        b"same",
    )
    .await;
    assert_eq!(again.status(), 200);
    assert_eq!(
        seen.take(&h, K),
        Requests {
            hot: vec![Op::Head],
            marks: vec![Op::Get],
            cold: vec![],
        }
    );
    assert_eq!(h.hot_mem.head(K).await.unwrap().tag, tag);

    let sum = checksum(b"diff");
    let other = call(
        &h,
        "PUT",
        K,
        &[],
        &[("x-amz-checksum-sha256", &sum)],
        b"diff",
    )
    .await;
    assert_eq!(other.status(), 200);
    released(&h, K).await;
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("diff"));
}

/// Asserts that a claim does not stand in for the check of the body: a put of bytes of the same
/// size as those the key holds whose `x-amz-checksum-sha256` names the stored bytes is 400
/// `BadDigest`, not 200, and the key is not changed.
#[tokio::test]
async fn a_body_that_is_not_the_checksum_it_claims_is_refused_when_the_claim_names_the_stored_bytes()
 {
    let h = marks().await;
    assert_eq!(plain(&h, K, b"same").await.status(), 200);
    released(&h, K).await;
    let sum = checksum(b"same");
    let forged = call(
        &h,
        "PUT",
        K,
        &[],
        &[("x-amz-checksum-sha256", &sum)],
        b"evil",
    )
    .await;
    assert_eq!(forged.status(), 400);
    assert_eq!(code(forged).await, "BadDigest");
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("same"));
}

/// Asserts that under the entry protocol nothing changes: a plain put of a key that exists is 412,
/// as it was, the key keeps its bytes, and no marker request is made.
#[tokio::test]
async fn under_the_entry_protocol_a_plain_put_of_a_key_that_exists_is_still_412() {
    let h = Harness::start_fixed(|_| {}).await;
    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    let refused = plain(&h, K, b"two").await;
    assert_eq!(refused.status(), 412);
    assert_eq!(code(refused).await, "PreconditionFailed");
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
    assert!(h.marks.call_log().is_empty());
}

/// Asserts that with the landing off a put the hot tier refuses with `Full` before it took a byte
/// is 503 `SlowDown` with `Retry-After`, for a new key and for a key that exists, that nothing is
/// asked of the cold tier, that no marker is left behind by the new key, and that the key that
/// existed keeps its bytes and its lock is let go.
#[tokio::test]
async fn a_put_the_hot_tier_refuses_with_full_is_503_slowdown_when_the_landing_is_off() {
    let h = marks().await;
    h.hot.fail_next_on(Op::Put, K, Error::Full(K.to_owned()));
    let mut seen = Cursor::at(&h);
    let refused = plain(&h, K, b"one").await;
    assert_eq!(refused.status(), 503);
    assert_eq!(refused.headers()["retry-after"], "1");
    assert_eq!(code(refused).await, "SlowDown");
    assert!(seen.take(&h, K).cold.is_empty());
    unmarked(&h, K).await;
    assert_eq!(text(&*h.hot_mem, K).await, None);

    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    h.hot
        .fail_next_on(Op::PutIfMatch, K, Error::Full(K.to_owned()));
    let mut seen = Cursor::at(&h);
    let refused = plain(&h, K, b"two").await;
    assert_eq!(refused.status(), 503);
    assert_eq!(code(refused).await, "SlowDown");
    released(&h, K).await;
    assert!(seen.take(&h, K).cold.is_empty());
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
    assert_eq!(text(&*h.cold_mem, K).await, None);
}

/// Asserts that with the landing on a put the hot tier refuses with `Full` before it took a byte
/// lands in the cold tier: 200, the cold tier holds the bytes, the hot tier none, the cold tier is
/// asked for its HEAD and then written, and the marker is removed once the landing is settled. The
/// same for a key that exists in the hot tier, whose hot object is deleted before the answer.
#[tokio::test]
async fn a_put_the_hot_tier_refuses_with_full_lands_in_the_cold_tier_when_the_landing_is_on() {
    let h = landing().await;
    h.hot.fail_next_on(Op::Put, K, Error::Full(K.to_owned()));
    let mut seen = Cursor::at(&h);
    let put = plain(&h, K, b"one").await;
    assert_eq!(put.status(), 200);
    unmarked(&h, K).await;
    let requests = seen.take(&h, K);
    assert_eq!(requests.cold, vec![Op::Head, Op::Put]);
    assert!(!requests.hot.contains(&Op::Delete), "{requests:?}");
    assert_eq!(text(&*h.cold_mem, K).await.as_deref(), Some("one"));
    assert_eq!(text(&*h.hot_mem, K).await, None);

    let other = "builds/other";
    assert_eq!(plain(&h, other, b"old").await.status(), 200);
    h.hot
        .fail_next_on(Op::PutIfMatch, other, Error::Full(other.to_owned()));
    let mut seen = Cursor::at(&h);
    let put = plain(&h, other, b"new").await;
    assert_eq!(put.status(), 200);
    unmarked(&h, other).await;
    let requests = seen.take(&h, other);
    assert_eq!(requests.cold, vec![Op::Head, Op::Put]);
    assert_eq!(requests.hot.last(), Some(&Op::Delete));
    assert_eq!(text(&*h.cold_mem, other).await.as_deref(), Some("new"));
    assert_eq!(text(&*h.hot_mem, other).await, None);
}

/// Asserts that a put with `If-None-Match: *` the hot tier refuses with `Full` is 503 whatever the
/// landing is set to: only a plain put lands in the cold tier, and nothing is written there.
#[tokio::test]
async fn a_create_only_put_the_hot_tier_refuses_with_full_is_503_even_with_the_landing_on() {
    let h = landing().await;
    h.hot.fail_next_on(Op::Put, K, Error::Full(K.to_owned()));
    let refused = create(&h, K, b"one").await;
    assert_eq!(refused.status(), 503);
    assert_eq!(code(refused).await, "SlowDown");
    assert_eq!(text(&*h.cold_mem, K).await, None);
    assert_eq!(h.cold.calls_on(Op::Put, K), 0);
}

/// Asserts that a scoped key, whose write right does not replace, is answered 412 for a plain put
/// of a key that exists, and that a URL presigned for a put is good once: the second use is 412.
/// Both hold under markers, where the full key replaces.
#[tokio::test]
async fn a_scoped_key_and_a_presigned_url_still_only_create_under_markers() {
    let scoped = ("scoped-a", "scoped-secret-0123456789");
    let prefixes = Prefixes {
        read: &["w/"],
        write: &["w/"],
        delete: &[],
    };
    let scoped_key = ScopedKey::new(scoped.0, scoped.1, &prefixes).unwrap();
    let grant = Grant {
        prefix: "p/".to_owned(),
        max_expires_secs: 600,
        max_object_bytes: 1 << 20,
    };
    let presign = ("presign-a", "presign-secret-0123456789");
    let presign_key = PresignKey::new(presign.0, presign.1, grant).unwrap();
    let h = Harness::start_marks(
        |_| {},
        |config| {
            config.scoped_keys = vec![scoped_key];
            config.presign_keys = vec![presign_key];
        },
    )
    .await;

    let path = format!("/{BUCKET}/w/k");
    let put = |body: &'static [u8]| {
        let h = &h;
        let path = path.clone();
        async move {
            let headers = h.signed_as(scoped, "PUT", &path, &[], &[], body);
            send(h, "PUT", &path, &[], &headers, body).await
        }
    };
    assert_eq!(put(b"one").await.status(), 200);
    assert_eq!(put(b"two").await.status(), 412);
    assert_eq!(text(&*h.hot_mem, "w/k").await.as_deref(), Some("one"));

    let endpoint = format!("http://{}", h.addr);
    let request = PresignRequest {
        endpoint: &endpoint,
        region: "us-east-1",
        bucket: BUCKET,
        key: "p/k",
        expires_secs: 300,
        now: FIXED_MILLIS,
    };
    let url = presign_put(&AccessKey::new(presign.0, presign.1), &request).unwrap();
    let http = reqwest::Client::new();
    let first = http.put(&url).body("pack").send().await.unwrap();
    assert_eq!(first.status(), 200);
    let second = http.put(&url).body("other").send().await.unwrap();
    assert_eq!(second.status(), 412);
    assert_eq!(text(&*h.hot_mem, "p/k").await.as_deref(), Some("pack"));
}

/// The requests of the completion: those for the upload and its parts, which come before it, left out.
fn completion(mut requests: Requests) -> Requests {
    requests
        .hot
        .retain(|op| !matches!(op, Op::CreateUpload | Op::UploadPart));
    requests
}

/// Begins a multipart upload of `key` with one part, `body`, and completes it with `headers`.
/// Gives the answer to the completion and the id of the upload.
async fn upload(
    h: &Harness,
    key: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> (reqwest::Response, String) {
    let begun = call(h, "POST", key, &[("uploads", "")], &[], b"").await;
    let id = crate::raw::upload_id(&begun.text().await.unwrap()).to_owned();
    let query = [("partNumber", "1"), ("uploadId", id.as_str())];
    let part = call(h, "PUT", key, &query, &[], body).await;
    assert_eq!(part.status(), 200);
    let etag = part.headers()["etag"].to_str().unwrap().to_owned();
    let listed = format!("<Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part>");
    let xml = format!("<CompleteMultipartUpload>{listed}</CompleteMultipartUpload>");
    let query = [("uploadId", id.as_str())];
    let done = call(h, "POST", key, &query, headers, xml.as_bytes()).await;
    (done, id)
}

/// Asserts what the completion of an upload of a new key asks of the stores: the hot HEAD with
/// the marker GET, then the hot tier's completion and the marker, and nothing of the cold tier.
#[tokio::test]
async fn the_completion_of_an_upload_of_a_new_key_writes_the_object_and_its_marker() {
    let h = marks().await;
    let mut seen = Cursor::at(&h);
    let (done, _) = upload(&h, K, b"parts", &[]).await;
    assert_eq!(done.status(), 200);
    let requests = completion(seen.take(&h, K));
    assert_eq!(requests.marks, vec![Op::Get, Op::Put]);
    assert_eq!(requests.hot.first(), Some(&Op::Head));
    assert_eq!(requests.hot.last(), Some(&Op::CompleteUpload));
    assert!(requests.cold.is_empty(), "{requests:?}");
    assert_eq!(shape(&h, K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("parts"));
}

/// Asserts that the completion of an upload of a key that exists replaces it, as S3 does: 200, the
/// key holds the bytes of the parts, and the stores are asked for the HEAD with the marker GET, the
/// compare-and-swap of the marker to a lock, the completion by the tag of the HEAD, and the release
/// of the lock after the answer. Nothing is asked of the cold tier.
#[tokio::test]
async fn the_completion_of_an_upload_of_a_key_that_exists_replaces_it_and_answers_200() {
    let h = marks().await;
    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    released(&h, K).await;
    let mut seen = Cursor::at(&h);
    let (done, _) = upload(&h, K, b"parts", &[]).await;
    assert_eq!(done.status(), 200);
    released(&h, K).await;
    let requests = completion(seen.take(&h, K));
    assert_eq!(requests.hot, vec![Op::Head, Op::CompleteUpload]);
    assert_eq!(
        requests.marks,
        vec![Op::Get, Op::PutIfMatch, Op::PutIfMatch]
    );
    assert!(requests.cold.is_empty(), "{requests:?}");
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("parts"));
    assert_eq!(shape(&h, K).await, Some((MarkerState::Dirty, false)));
}

/// Asserts that the completion of an upload with `If-None-Match: *` of a key that exists is 412 and
/// leaves the key as it was and the upload open, which the same completion without the header
/// then finishes.
#[tokio::test]
async fn the_completion_of_an_upload_with_if_none_match_of_a_key_that_exists_is_412() {
    let h = marks().await;
    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    let (refused, id) = upload(&h, K, b"parts", &[("if-none-match", "*")]).await;
    assert_eq!(refused.status(), 412);
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
    let open = h.hot_mem.list_uploads("", None, 10).await.unwrap();
    assert_eq!(open.len(), 1, "{id}");
}

/// Asserts that under the entry protocol the completion of an upload of a key that exists is
/// still 412, as it was.
#[tokio::test]
async fn under_the_entry_protocol_the_completion_of_an_upload_of_a_key_that_exists_is_412() {
    let h = Harness::start_fixed(|_| {}).await;
    assert_eq!(plain(&h, K, b"one").await.status(), 200);
    let (refused, _) = upload(&h, K, b"parts", &[]).await;
    assert_eq!(refused.status(), 412);
    assert_eq!(text(&*h.hot_mem, K).await.as_deref(), Some("one"));
}
