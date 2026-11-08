//! What the cache counts of its calls to the upstream (`tierstore_cache_upstream_*`) and of the
//! HEADs it is asked (`tierstore_cache_head_*`), and that a read it does not serve is one miss,
//! counted before any call to the upstream. Each count of calls is also compared with the calls
//! the upstream itself saw, so a call counted twice or not at all fails. Time is the rig's clock,
//! moved by hand; no test waits for a real time.

use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::metrics::{Call, PutFailed, read};
use super::testing::{Rig, digest, pattern};
use crate::{Backend as _, ByteRange, Error, Op, PartBody, PutClaim};

/// The calls the cache counted: GETs, HEADs, listings and listings of uploads.
fn counted(rig: &Rig) -> [u64; 4] {
    let counters = rig.cache.counters();
    [Call::Get, Call::Head, Call::List, Call::ListUploads].map(|kind| counters.calls(kind))
}

/// The same calls, as the upstream saw them.
fn seen(rig: &Rig) -> [u64; 4] {
    [Op::Get, Op::Head, Op::List, Op::ListUploads].map(|op| rig.calls(op))
}

/// The bytes the cache counted as brought back by its upstream GETs.
fn bytes_read(rig: &Rig) -> u64 {
    read(&rig.cache.counters().upstream_bytes_read)
}

/// The hits and the misses of the cache's GETs.
fn hits_and_misses(rig: &Rig) -> (u64, u64) {
    let counters = rig.cache.counters();
    (read(&counters.hits), read(&counters.misses))
}

/// The head hits and the head misses.
fn heads(rig: &Rig) -> (u64, u64) {
    let counters = rig.cache.counters();
    (read(&counters.head_hits), read(&counters.head_misses))
}

/// The puts the upstream accepted and their bytes.
fn puts(rig: &Rig) -> (u64, u64) {
    let counters = rig.cache.counters();
    (
        read(&counters.upstream_puts),
        read(&counters.upstream_put_bytes),
    )
}

/// The failed puts, by reason, in the order of [`PutFailed`].
fn failures(rig: &Rig) -> [u64; 5] {
    let reasons = [
        PutFailed::Exists,
        PutFailed::Unavailable,
        PutFailed::Full,
        PutFailed::BadDigest,
        PutFailed::Other,
    ];
    reasons.map(|why| rig.cache.counters().put_failures(why))
}

/// A body of one chunk.
fn whole(bytes: &[u8]) -> PartBody<'static> {
    let chunk = Bytes::copy_from_slice(bytes);
    stream::once(async move { Ok(chunk) }).boxed()
}

/// A body of these chunks.
fn chunks(parts: &[&'static [u8]]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts
        .iter()
        .map(|part| Ok(Bytes::from_static(part)))
        .collect();
    stream::iter(items).boxed()
}

/// A body that gives `bytes` and then fails, as a client that goes away does.
fn cut(bytes: &'static [u8]) -> PartBody<'static> {
    let gone = Error::Unavailable("the client went away".to_owned());
    stream::iter([Ok(Bytes::from_static(bytes)), Err(gone)]).boxed()
}

/// The key named by the content `bytes`.
fn content_key(bytes: &[u8]) -> String {
    format!("cas/l/{}", digest(bytes))
}

/// Asserts that a whole read that fills a 350-byte object in chunks of 100 counts four GETs, the
/// fill's two HEADs and the 350 bytes, as the upstream saw them, and that a hit after it counts
/// no call and no byte. It would catch a fill chunk or a fill HEAD that is not counted.
#[tokio::test]
async fn a_fill_counts_every_chunk_and_both_of_its_heads() {
    let rig = Rig::with("tscache-up-fill", |config| config.fill_chunk = 100);
    let body = pattern(350);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(counted(&rig), [4, 2, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
    assert_eq!(bytes_read(&rig), 350);

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(counted(&rig), [4, 2, 0, 0]);
    assert_eq!(bytes_read(&rig), 350);
    assert_eq!(hits_and_misses(&rig), (1, 1));
}

/// Asserts that a ranged read of an object that is not held counts the HEAD that asks its size
/// as well as the whole fill (four GETs, three HEADs, 350 bytes fetched for 20 served), and that
/// the next read of the range asks nothing. It would catch the size HEAD going uncounted.
#[tokio::test]
async fn a_ranged_miss_counts_the_size_head_and_the_whole_fill() {
    let rig = Rig::with("tscache-up-ranged", |config| config.fill_chunk = 100);
    let body = pattern(350);
    rig.put("cas/b/obj", &body).await;
    let range = Some(ByteRange::new(10, 20));
    assert_eq!(
        rig.cache.get("cas/b/obj", range).await.unwrap(),
        &body[10..30]
    );
    assert_eq!(counted(&rig), [4, 3, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
    assert_eq!(bytes_read(&rig), 350);
    assert_eq!(read(&rig.cache.counters().bytes_served_upstream), 20);

    assert_eq!(
        rig.cache.get("cas/b/obj", range).await.unwrap(),
        &body[10..30]
    );
    assert_eq!(counted(&rig), [4, 3, 0, 0]);
    assert_eq!(hits_and_misses(&rig), (1, 1));
}

/// Asserts that a read of a key the cache does not keep counts one GET for each chunk and the two
/// HEADs that hold a read of more than one chunk to one version, and that a read of one chunk or
/// less is one GET and no HEAD; neither is a hit or a miss and nothing is kept. It would catch a
/// pass-through that is not counted, or counted as a miss.
#[tokio::test]
async fn a_pass_through_counts_each_chunk_and_the_heads_that_pin_it() {
    let rig = Rig::with("tscache-up-pass", |config| config.fill_chunk = 100);
    let body = pattern(250);
    rig.put("other/obj", &body).await;
    assert_eq!(rig.get("other/obj").await.unwrap(), body);
    assert_eq!(counted(&rig), [3, 2, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
    assert_eq!(bytes_read(&rig), 250);

    let range = Some(ByteRange::new(0, 50));
    assert_eq!(
        rig.cache.get("other/obj", range).await.unwrap(),
        &body[..50]
    );
    assert_eq!(counted(&rig), [4, 2, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
    assert_eq!(bytes_read(&rig), 300);
    assert_eq!(hits_and_misses(&rig), (0, 0));
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a ranged read of an object over the whole-object limit is one miss, one HEAD for
/// the size and one GET of the range alone, with only the range fetched and nothing filled.
#[tokio::test]
async fn a_ranged_read_over_the_whole_limit_fetches_the_range_alone() {
    let rig = Rig::with("tscache-up-limit", |config| {
        config.fill_chunk = 100;
        config.whole_limit = 100;
    });
    let body = pattern(350);
    rig.put("cas/b/big", &body).await;
    let range = Some(ByteRange::new(10, 20));
    assert_eq!(
        rig.cache.get("cas/b/big", range).await.unwrap(),
        &body[10..30]
    );
    assert_eq!(counted(&rig), [1, 1, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
    assert_eq!(bytes_read(&rig), 20);
    assert_eq!(hits_and_misses(&rig), (0, 1));
    assert_eq!(read(&rig.cache.counters().fills), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a ranged read of a kept key whose size HEAD fails (the key is not at the upstream,
/// or the upstream is down) still counts its miss, since the miss is counted before any call to
/// the upstream; that a whole read of a missing key counts one too; and that a range past the end
/// of a held object is neither a hit nor a miss. It fails on a miss counted only after the HEAD.
#[tokio::test]
async fn a_read_the_cache_does_not_serve_is_one_miss_whatever_the_upstream_answers() {
    let rig = Rig::new("tscache-up-miss-first");
    let range = Some(ByteRange::new(0, 10));
    let missing = rig.cache.get("cas/b/none", range).await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    assert_eq!(hits_and_misses(&rig), (0, 1));
    assert_eq!(counted(&rig), [0, 1, 0, 0]);

    rig.put("cas/b/obj", &pattern(100)).await;
    let down = Error::Unavailable("down".to_owned());
    rig.upstream.fail_next_with(Op::Head, down.clone());
    assert_eq!(rig.cache.get("cas/b/obj", range).await, Err(down));
    assert_eq!(hits_and_misses(&rig), (0, 2));
    assert_eq!(counted(&rig), [0, 2, 0, 0]);

    assert!(matches!(
        rig.get("cas/b/none").await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(hits_and_misses(&rig), (0, 3));

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), pattern(100));
    assert_eq!(hits_and_misses(&rig), (0, 4));
    let past = rig
        .cache
        .get("cas/b/obj", Some(ByteRange::new(500, 10)))
        .await;
    assert!(past.is_err(), "{past:?}");
    assert_eq!(hits_and_misses(&rig), (0, 4));
    assert_eq!(counted(&rig), seen(&rig));
}

/// Asserts that a HEAD the index answers is a head hit and makes no call, and that a HEAD of a
/// key not held, of a key not kept and of a missing key is a head miss with one upstream HEAD
/// each; that a reserved key is neither; and that the fill's own HEADs are not counted as heads
/// of the cache. It would catch a head hit that asks the upstream, or a miss that is not counted.
#[tokio::test]
async fn a_head_from_the_index_is_a_hit_and_any_other_is_a_miss() {
    let rig = Rig::new("tscache-up-heads");
    rig.put("cas/b/obj", b"0123456789").await;
    rig.put("other/obj", b"abc").await;
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap().size, 10);
    assert_eq!(heads(&rig), (0, 1));
    assert_eq!(counted(&rig)[1], 1);

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), "0123456789");
    assert_eq!(counted(&rig)[1], 3);
    assert_eq!(heads(&rig), (0, 1));
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap().size, 10);
    assert_eq!(heads(&rig), (1, 1));
    assert_eq!(counted(&rig)[1], 3);

    assert_eq!(rig.cache.head("other/obj").await.unwrap().size, 3);
    let missing = rig.cache.head("cas/b/none").await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    assert_eq!(heads(&rig), (1, 3));
    let reserved = rig.cache.head("_journal/x").await;
    assert!(matches!(reserved, Err(Error::Reserved(_))), "{reserved:?}");
    assert_eq!(heads(&rig), (1, 3));
    assert_eq!(counted(&rig), [1, 5, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
}

/// Asserts that a HEAD of an old hit that the check finds the same is a head hit, and that one it
/// finds changed or gone is a head miss, answered by the check's own HEAD: each counts that one
/// upstream HEAD and no other.
#[tokio::test]
async fn a_head_of_an_old_hit_is_a_hit_only_when_the_check_keeps_it() {
    let rig = Rig::with("tscache-up-heads-old", |config| {
        config.revalidate_after = Some(Duration::from_millis(1000));
    });
    for key in ["cas/b/same", "cas/b/changed", "cas/b/gone"] {
        rig.put(key, b"first").await;
        assert_eq!(rig.get(key).await.unwrap(), "first");
    }
    let before = counted(&rig);
    rig.clock.advance(1001);
    rig.replace("cas/b/changed", b"second!").await;
    rig.tamper.delete("cas/b/gone").await.unwrap();

    assert_eq!(rig.cache.head("cas/b/same").await.unwrap().size, 5);
    assert_eq!(heads(&rig), (1, 0));
    assert_eq!(rig.cache.head("cas/b/changed").await.unwrap().size, 7);
    assert_eq!(heads(&rig), (1, 1));
    let gone = rig.cache.head("cas/b/gone").await;
    assert!(matches!(gone, Err(Error::NotFound(_))), "{gone:?}");
    assert_eq!(heads(&rig), (1, 2));
    assert_eq!(counted(&rig), [before[0], before[1] + 3, 0, 0]);
    assert_eq!(counted(&rig), seen(&rig));
}

/// Asserts that a listing is one `list` call, that each page a paged listing asks the upstream
/// for is one more (the pages that make up for reserved keys included), that each page of
/// uploads is one `list_uploads`, and that a reserved prefix and a listing of parts count none.
/// It would catch a page that is not counted, and a call that is counted under the wrong kind.
#[tokio::test]
async fn every_listing_call_is_counted_and_nothing_else_is() {
    let rig = Rig::new("tscache-up-lists");
    for key in ["_journal/a", "_journal/b", "a/1", "b/1"] {
        rig.put(key, b"x").await;
    }
    assert_eq!(rig.cache.list("a/").await.unwrap().len(), 1);
    assert_eq!(counted(&rig)[2], 1);
    let page = rig.cache.list_page("", None, 2).await.unwrap();
    let keys: Vec<&str> = page.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(keys, ["a/1", "b/1"]);
    assert_eq!(counted(&rig)[2], 3);
    assert!(rig.cache.list("_journal/").await.unwrap().is_empty());
    assert_eq!(counted(&rig)[2], 3);

    let id = rig.tamper.create_upload("cas/b/up").await.unwrap();
    assert_eq!(rig.cache.list_uploads("", None, 10).await.unwrap().len(), 1);
    assert!(
        rig.cache
            .list_uploads("_journal/", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let parts = rig.cache.list_parts("cas/b/up", &id, None, 10).await;
    assert!(parts.unwrap().is_empty());
    assert_eq!(rig.calls(Op::ListParts), 1);
    assert_eq!(counted(&rig), [0, 0, 3, 1]);
    assert_eq!(counted(&rig), seen(&rig));
}

/// Asserts that a put under a kept prefix and one under a prefix that is not kept are two puts
/// with their bytes and one HEAD before each; that a key the HEAD finds is answered "exists" with
/// no put sent and counted in `write_exists` alone; that each failure of the upstream's put, of
/// the HEAD before it and of the body counts once under its reason with no put and no byte; and
/// that a plain put is a put with no HEAD before it. It would catch a failure counted under the
/// wrong reason, a refused put counted as taken, or bytes of a failed put.
#[tokio::test]
async fn each_put_counts_once_as_taken_or_under_its_reason() {
    let rig = Rig::writing("tscache-up-puts");
    let put = |key: &'static str, body: PartBody<'static>, len: u64| {
        let cache = rig.cache.clone();
        async move { cache.put_stream(key, Some(len), body).await }
    };
    assert_eq!(put("cas/b/a", whole(b"abcdef"), 6).await, Ok(()));
    assert_eq!(put("other/b", whole(b"ghijkl"), 6).await, Ok(()));
    assert_eq!(puts(&rig), (2, 12));
    assert_eq!(read(&rig.cache.counters().writes_through), 2);
    assert_eq!(rig.calls(Op::Put), 2);

    let exists = put("cas/b/a", whole(b"abcdef"), 6).await;
    assert!(matches!(exists, Err(Error::AlreadyExists(_))), "{exists:?}");
    assert_eq!(read(&rig.cache.counters().write_exists), 1);
    assert_eq!(rig.calls(Op::Put), 2);
    assert_eq!(failures(&rig), [0; 5]);

    let full = Error::Full("no room".to_owned());
    rig.upstream.fail_next_with(Op::Put, full.clone());
    assert_eq!(put("cas/b/full", whole(b"x"), 1).await, Err(full));
    let down = Error::Unavailable("down".to_owned());
    rig.upstream.fail_next_with(Op::Put, down.clone());
    assert_eq!(put("other/down", whole(b"x"), 1).await, Err(down.clone()));
    let other = Error::Other("broken".to_owned());
    rig.upstream.fail_next_with(Op::Put, other.clone());
    assert_eq!(put("cas/b/other", whole(b"x"), 1).await, Err(other));
    assert_eq!(failures(&rig), [0, 1, 1, 0, 1]);

    // The HEAD before the put fails: no put is sent, and the put counts as failed.
    let puts_sent = rig.calls(Op::Put);
    rig.upstream.fail_next_with(Op::Head, down.clone());
    assert_eq!(put("cas/b/headless", whole(b"x"), 1).await, Err(down));
    assert_eq!(rig.calls(Op::Put), puts_sent);
    assert_eq!(failures(&rig), [0, 2, 1, 0, 1]);

    // A body that does not hash to its name, and one that is cut: under a kept and an unkept
    // prefix, each a failure, with no put and no byte.
    let key = content_key(b"good bytes");
    let wrong = rig.cache.put_stream(&key, Some(10), whole(b"evil bytes"));
    assert_eq!(wrong.await, Err(Error::BadDigest(key.clone())));
    assert!(put("cas/b/cut", cut(b"part"), 9).await.is_err());
    assert!(put("other/cut", cut(b"part"), 9).await.is_err());
    assert_eq!(failures(&rig), [0, 4, 1, 1, 1]);
    assert_eq!(puts(&rig), (2, 12));

    // A plain put sends no HEAD before it, and is a put with its bytes.
    let heads_before = rig.calls(Op::Head);
    let claim = PutClaim::default();
    let plain = rig
        .cache
        .put_stream_replacing("cas/b/a", Some(4), whole(b"new!"), &claim);
    assert!(plain.await.is_ok());
    assert_eq!(puts(&rig), (3, 16));
    assert_eq!(rig.calls(Op::Head), heads_before);
    assert_eq!(counted(&rig), seen(&rig));
}

/// Asserts, with `head_before_put` off, that a key the upstream holds is refused by its put: a
/// failure of reason `exists` and a `write_exists`, with no HEAD; and that a key named by its
/// content that the upstream took without reading the body to its end is a put with the bytes it
/// was given and a failure of reason `bad_digest`, as the client is told.
#[tokio::test]
async fn a_refusal_the_head_did_not_see_and_a_digest_found_bad_after_the_put_count_once() {
    let rig = Rig::with("tscache-up-put-races", |config| {
        config.write_through = true;
        config.head_before_put = false;
    });
    rig.put("cas/b/there", b"held").await;
    let refused = rig.cache.put_stream("cas/b/there", Some(4), whole(b"held"));
    assert!(matches!(refused.await, Err(Error::AlreadyExists(_))));
    assert_eq!(failures(&rig), [1, 0, 0, 0, 0]);
    assert_eq!(read(&rig.cache.counters().write_exists), 1);
    assert_eq!(puts(&rig), (0, 0));
    assert_eq!(counted(&rig)[1], 0);

    rig.tamper.stop_after.store(1, Ordering::SeqCst);
    let key = content_key(b"shortened!");
    let taken = rig
        .cache
        .put_stream(&key, Some(10), chunks(&[b"short", b"ened!"]));
    assert_eq!(taken.await, Err(Error::BadDigest(key.clone())));
    assert_eq!(puts(&rig), (1, 5));
    assert_eq!(failures(&rig), [1, 0, 0, 1, 0]);
}

/// Asserts that the parts of an upload count their bytes when the upstream takes them and the
/// completion counts one put and no bytes; that a completion the upstream refuses is a failure
/// under its reason and no put; and that a part that fails counts neither bytes nor a failure.
#[tokio::test]
async fn a_multipart_upload_is_one_put_with_the_bytes_of_its_parts() {
    let rig = Rig::writing("tscache-up-multipart");
    let id = rig.cache.create_upload("cas/b/mp").await.unwrap();
    let first = rig.cache.upload_part("cas/b/mp", &id, 1, whole(b"123456"));
    let first = first.await.unwrap();
    let second = rig.cache.upload_part("cas/b/mp", &id, 2, whole(b"78901"));
    let second = second.await.unwrap();
    assert_eq!(puts(&rig), (0, 11));
    let parts = [(1, first), (2, second)];
    assert!(
        rig.cache
            .complete_upload("cas/b/mp", &id, &parts)
            .await
            .is_ok()
    );
    assert_eq!(puts(&rig), (1, 11));
    assert_eq!(read(&rig.cache.counters().writes_through), 0);

    let id = rig.cache.create_upload("cas/b/mp2").await.unwrap();
    let down = Error::Unavailable("down".to_owned());
    rig.upstream.fail_next_with(Op::UploadPart, down.clone());
    let failed = rig.cache.upload_part("cas/b/mp2", &id, 1, whole(b"abc"));
    assert_eq!(failed.await, Err(down));
    assert_eq!(puts(&rig), (1, 11));
    let tag = rig.cache.upload_part("cas/b/mp2", &id, 1, whole(b"z"));
    let parts = [(1, tag.await.unwrap())];
    let exists = Error::AlreadyExists("cas/b/mp2".to_owned());
    rig.upstream
        .fail_next_with(Op::CompleteUpload, exists.clone());
    let refused = rig.cache.complete_upload("cas/b/mp2", &id, &parts).await;
    assert_eq!(refused, Err(exists));
    assert_eq!(puts(&rig), (1, 12));
    assert_eq!(failures(&rig), [1, 0, 0, 0, 0]);
    assert_eq!(read(&rig.cache.counters().write_exists), 0);
}
