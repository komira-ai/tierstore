//! An S3 tier reached through several endpoints that are front doors of one store
//! (`endpoints`, docs/design/multi-endpoint-store.md section 4): the calls of one multipart
//! upload, and the pages of one listing, go to different endpoints in turn and still make one
//! upload and one listing, because neither the client nor the proxy keeps state of them. The
//! endpoints are fronts of one loopback S3 service ([`FakeS3::start_front`]), each with a log of
//! the requests it took.

use std::sync::Arc;

use futures::StreamExt;
use futures::stream;
use objstore_s3::s3::fake::FakeS3;
use tierstore::s3::S3Tuning;
use tierstore::{
    Backend, Error, MemoryBackend, PartBody, Proxy, ProxyConfig, S3Backend, SystemClock,
};

use crate::s3::config;
use crate::support::{body, open_live, text};

/// The key every test uploads or lists under.
const KEY: &str = "k";

/// The fronts of one service: the first and `count - 1` more. Parts of a few bytes are taken.
async fn fronts(count: usize) -> Vec<FakeS3> {
    let first = FakeS3::start().await;
    first.set_min_part_bytes(1);
    let mut all = vec![first.clone()];
    for _ in 1..count {
        all.push(first.start_front().await);
    }
    all
}

/// A backend that sends its first request to `endpoints[0]`, its second to `endpoints[1]`, and so
/// on in turn, as a tier with `endpoints` does.
fn rotating(endpoints: &[String]) -> S3Backend {
    let first = config(&endpoints[0]);
    S3Backend::new_rotating(first, S3Tuning::default(), &endpoints[1..]).unwrap()
}

/// The endpoints of `fronts`, in order.
fn endpoints(fronts: &[FakeS3]) -> Vec<String> {
    fronts.iter().map(|f| f.endpoint().to_owned()).collect()
}

/// Sends `text` as part `number` of the upload `id` of [`KEY`], its length told.
async fn part(backend: &dyn Backend, id: &str, number: u32, text: &'static str) -> String {
    let chunks: PartBody<'static> = stream::once(async move { Ok(body(text)) }).boxed();
    let len = Some(text.len() as u64);
    let sent = backend.upload_part_with_len(KEY, id, number, len, chunks);
    sent.await.unwrap()
}

/// The request lines of a multipart upload `id` of [`KEY`] in bucket `tier-test`, as the
/// service logs them.
fn create_line() -> String {
    "POST /tier-test/k?uploads=".to_owned()
}

fn part_line(id: &str, number: u32) -> String {
    format!("PUT /tier-test/k?partNumber={number}&uploadId={id}")
}

fn complete_line(id: &str) -> String {
    format!("POST /tier-test/k?uploadId={id}")
}

fn abort_line(id: &str) -> String {
    format!("DELETE /tier-test/k?uploadId={id}")
}

/// Asserts that a multipart upload through a tier of three endpoints sends its creation to the
/// first, part 1 to the second, part 2 to the third and the completion to the first again, each
/// endpoint taking nothing else, and that the object read back after the completion has both
/// parts in order.
#[tokio::test]
async fn parts_and_completion_of_one_upload_reach_different_endpoints() {
    let fronts = fronts(3).await;
    let tier = rotating(&endpoints(&fronts));
    let id = tier.create_upload(KEY).await.unwrap();
    let one = part(&tier, &id, 1, "first ").await;
    let two = part(&tier, &id, 2, "second").await;
    tier.complete_upload(KEY, &id, &[(1, one), (2, two)])
        .await
        .unwrap();
    assert_eq!(
        fronts[0].requests(),
        [create_line(), complete_line(&id)],
        "the first endpoint"
    );
    assert_eq!(fronts[1].requests(), [part_line(&id, 1)], "the second");
    assert_eq!(fronts[2].requests(), [part_line(&id, 2)], "the third");
    assert_eq!(fronts[0].incomplete_uploads(), 0);
    assert_eq!(text(&tier, KEY).await, "first second");
}

/// Asserts that the abort of an upload goes to an endpoint that did not create it and removes the
/// upload there for every endpoint: a part sent to the third endpoint afterwards finds no upload
/// (`NotFound` naming the id), and no object is made.
#[tokio::test]
async fn an_abort_at_an_endpoint_that_did_not_create_the_upload_removes_it() {
    let fronts = fronts(3).await;
    let tier = rotating(&endpoints(&fronts));
    let id = tier.create_upload(KEY).await.unwrap();
    assert_eq!(fronts[0].incomplete_uploads(), 1);
    tier.abort_upload(KEY, &id).await.unwrap();
    assert_eq!(fronts[0].incomplete_uploads(), 0);
    let chunks: PartBody<'static> = stream::once(async { Ok(body("late")) }).boxed();
    let late = tier
        .upload_part_with_len(KEY, &id, 1, Some(4), chunks)
        .await;
    assert_eq!(late, Err(Error::NotFound(id.clone())));
    assert_eq!(fronts[0].requests(), [create_line()], "the first endpoint");
    assert_eq!(fronts[1].requests(), [abort_line(&id)], "the second");
    assert_eq!(fronts[2].requests(), [part_line(&id, 1)], "the third");
    assert_eq!(fronts[0].keys(), Vec::<String>::new());
}

/// Asserts that with the second of three endpoints refusing connections, the upload still makes
/// the object: part 1, whose turn is the refused endpoint, lands on the third, part 2 on the
/// first, and the completion, whose turn is the refused endpoint again, on the third. Each call
/// is sent once to an endpoint that took it.
#[tokio::test]
async fn a_part_whose_endpoint_refuses_the_connection_lands_on_the_next() {
    let fronts = fronts(2).await;
    let refused = format!("http://{}", tierstore_testkit::port::dead_addr());
    let order = [
        fronts[0].endpoint().to_owned(),
        refused,
        fronts[1].endpoint().to_owned(),
    ];
    let tier = rotating(&order);
    let id = tier.create_upload(KEY).await.unwrap();
    let one = part(&tier, &id, 1, "first ").await;
    let two = part(&tier, &id, 2, "second").await;
    tier.complete_upload(KEY, &id, &[(1, one), (2, two)])
        .await
        .unwrap();
    assert_eq!(
        fronts[0].requests(),
        [create_line(), part_line(&id, 2)],
        "the first endpoint"
    );
    assert_eq!(
        fronts[1].requests(),
        [part_line(&id, 1), complete_line(&id)],
        "the third"
    );
    assert_eq!(text(&tier, KEY).await, "first second");
}

/// Asserts that a listing read a page of two keys at a time, each page after the last key of the
/// one before as the S3 front's continuation token says, sends its three pages to the three
/// endpoints in turn and gives every one of the five keys once, in order.
#[tokio::test]
async fn a_listing_whose_pages_go_to_different_endpoints_gives_every_key_once() {
    let fronts = fronts(3).await;
    let writer = S3Backend::new(config(fronts[0].endpoint())).unwrap();
    let written = ["a/1", "a/2", "a/3", "a/4", "a/5"];
    for key in written {
        writer.put(key, body(key)).await.unwrap();
    }
    let before = fronts[0].requests().len();
    let tier = rotating(&endpoints(&fronts));
    let mut listed: Vec<String> = Vec::new();
    loop {
        let after = listed.last().map(String::as_str);
        let page = tier.list_page("a/", after, 2).await.unwrap();
        if page.is_empty() {
            break;
        }
        listed.extend(page.into_iter().map(|object| object.key));
    }
    assert_eq!(listed, written);
    // Three pages of keys, and the fourth, empty, read that ends the listing.
    let taken: Vec<usize> = fronts.iter().map(|f| f.requests().len()).collect();
    assert_eq!(taken, [before + 2, 1, 1], "pages per endpoint");
}

/// A proxy `id` whose hot tier is reached through `front` and whose cold tier is `cold`, live.
async fn proxy_through(front: &FakeS3, id: &str, cold: &Arc<MemoryBackend>) -> Proxy {
    let hot: Arc<dyn Backend> = Arc::new(S3Backend::new(config(front.endpoint())).unwrap());
    let cold: Arc<dyn Backend> = cold.clone();
    let settings = ProxyConfig::new(id, 1 << 20, Arc::new(SystemClock));
    open_live(hot, cold, settings).await.unwrap()
}

/// Asserts the requests `line` names reached front `at` of `fronts` and no other.
fn only_at(fronts: &[FakeS3], at: usize, line: &str) {
    for (n, front) in fronts.iter().enumerate() {
        let took = front.requests().iter().any(|taken| taken == line);
        assert_eq!(took, n == at, "{line} at front {n}");
    }
}

/// Asserts that an upload made in the hot tier through one storage node (a proxy) is continued
/// through others, as section 4's diagram has it: node 1 creates it (an id with the hot prefix),
/// node 2 takes part 1, node 3 part 2, node 1 completes it, each node over its own front of the
/// shared hot store; and that node 2 then reads the object with both parts in order.
#[tokio::test]
async fn an_upload_begun_on_one_node_is_continued_and_completed_on_others() {
    let fronts = fronts(3).await;
    let cold = Arc::new(MemoryBackend::new());
    let mut nodes = Vec::new();
    for (n, front) in fronts.iter().enumerate() {
        nodes.push(proxy_through(front, &format!("p{}", n + 1), &cold).await);
    }
    let id = nodes[0].create_upload(KEY).await.unwrap();
    let inner = id
        .strip_prefix("ts-hot~")
        .expect("an upload of the hot tier");
    let one = part(&nodes[1], &id, 1, "first ").await;
    let two = part(&nodes[2], &id, 2, "second").await;
    nodes[0]
        .complete_upload(KEY, &id, &[(1, one), (2, two)])
        .await
        .unwrap();
    only_at(&fronts, 0, &create_line());
    only_at(&fronts, 1, &part_line(inner, 1));
    only_at(&fronts, 2, &part_line(inner, 2));
    only_at(&fronts, 0, &complete_line(inner));
    assert_eq!(fronts[0].incomplete_uploads(), 0);
    assert_eq!(text(&nodes[1], KEY).await, "first second");
}

/// Asserts that an upload made through one node is aborted through another, which removes it:
/// the abort reaches the second node's front alone, and the shared store holds no upload.
#[tokio::test]
async fn an_upload_begun_on_one_node_is_aborted_on_another() {
    let fronts = fronts(2).await;
    let cold = Arc::new(MemoryBackend::new());
    let first = proxy_through(&fronts[0], "p1", &cold).await;
    let second = proxy_through(&fronts[1], "p2", &cold).await;
    let id = first.create_upload(KEY).await.unwrap();
    let inner = id
        .strip_prefix("ts-hot~")
        .expect("an upload of the hot tier");
    assert_eq!(fronts[0].incomplete_uploads(), 1);
    second.abort_upload(KEY, &id).await.unwrap();
    only_at(&fronts, 1, &abort_line(inner));
    assert_eq!(fronts[0].incomplete_uploads(), 0);
}
